// Author: Nicola Antonello

//! The relay's input and output (ADR-0052, ADR-0053, ADR-0062): one attempt to deliver an
//! event to a merchant's endpoint. The relay decides which attempts to make and what their
//! answers mean; this module reads what an attempt needs from the database, and posts it.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{Method, Request, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;

use crate::events::{self, BodyError};
use crate::operator::EndpointUrl;
use crate::relay::{Answer, Delivery};
use crate::retries::retry_after_from;
use crate::signing::{MasterKey, sign};

/// How long an attempt waits for its answer's head (ADR-0053).
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// How much of an answer's body is read after its head, only so that the connection can be
/// used again (ADR-0053).
const DRAIN_BYTES: usize = 65_536;

/// How long an answer's body may take to end after its head (ADR-0053).
const DRAIN_TIME: Duration = Duration::from_secs(1);

/// Posts the attempts to deliver events, each signed under its endpoint's secret
/// (ADR-0051), through a client that follows no redirect and sends nothing again by itself
/// (ADR-0036), and waits a bounded time for each answer (ADR-0053).
#[derive(Debug)]
pub struct DeliveryClient {
    master: MasterKey,
    timeout: Duration,
    client: Client<HttpConnector, Full<Bytes>>,
}

/// What an attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The endpoint answered, or gave no answer, which the relay reads (ADR-0053).
    Answered(Answer),
    /// The endpoint is disabled, by the operator or by the relay before a restart: nothing
    /// was sent, and the endpoint's deliveries end (ADR-0062).
    Disabled,
}

/// Why an attempt was not made: the service's database did not give what it needs. The
/// delivery waits like a retry, and the endpoint is not blamed (ADR-0053).
#[derive(Debug, thiserror::Error)]
pub enum NotSent {
    /// The endpoint's URL and disablement could not be read.
    #[error("the endpoint could not be read")]
    Endpoint(#[source] sqlx::Error),
    /// The event's body could not be read.
    #[error("the event's body could not be read")]
    Body(#[source] BodyError),
}

impl DeliveryClient {
    /// A client that signs under the secrets that `master` derives, and waits at most
    /// `timeout` for each answer's head: [`TIMEOUT`] in the service.
    #[must_use]
    pub fn new(master: MasterKey, timeout: Duration) -> Self {
        Self {
            master,
            timeout,
            client: Client::builder(TokioExecutor::new())
                .retry_canceled_requests(false)
                .build_http(),
        }
    }

    /// Attempts `delivery` at `at`, the attempt's start, which signs it (ADR-0051) and from
    /// which a `Retry-After` date counts (ADR-0053). It reads the endpoint's URL with its
    /// disablement, then the event's body, from `pool`, the system role's, and posts the
    /// body. A URL that ADR-0060's rules refuse cannot be reached as written, so the attempt
    /// gets no answer.
    ///
    /// # Errors
    ///
    /// [`NotSent`] when the database did not give what the attempt needs.
    pub async fn attempt(
        &self,
        pool: &PgPool,
        delivery: Delivery,
        at: OffsetDateTime,
    ) -> Result<Outcome, NotSent> {
        let endpoint = sqlx::query!(
            r#"SELECT e.url,
                      EXISTS (SELECT 1 FROM webhook_endpoint_disablements d
                               WHERE d.endpoint_id = e.id) AS "disabled!"
                 FROM webhook_endpoints e
                WHERE e.id = $1"#,
            delivery.endpoint.get()
        )
        .fetch_optional(pool)
        .await
        .map_err(NotSent::Endpoint)?
        .expect("a delivery's endpoint exists: webhook_deliveries_endpoint_fkey, and LG001");
        if endpoint.disabled {
            return Ok(Outcome::Disabled);
        }
        let Ok(url) = EndpointUrl::parse(&endpoint.url) else {
            return Ok(Outcome::Answered(Answer::NoAnswer));
        };
        let body = events::body(pool, delivery.event)
            .await
            .map_err(NotSent::Body)?
            .expect("a delivery's event exists: webhook_deliveries_event_fkey, and LG001");
        let secret = self.master.secret_of(delivery.endpoint);
        let signed = sign(&secret, delivery.event, at, &body);
        let request = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .header("webhook-id", signed.id)
            .header("webhook-timestamp", signed.timestamp)
            .header("webhook-signature", signed.signature)
            .body(Full::new(Bytes::from(body)))
            .expect("a URL that ADR-0060's rules read, and headers of visible ASCII");
        Ok(Outcome::Answered(self.post(request, at).await))
    }

    /// Posts one attempt. Its answer is its head, if that arrives within the timeout. The
    /// body changes nothing: it is read for at most [`DRAIN_TIME`] and [`DRAIN_BYTES`] only
    /// so that the connection can be used again, and one that does not end within them
    /// closes the connection (ADR-0053).
    async fn post(&self, request: Request<Full<Bytes>>, at: OffsetDateTime) -> Answer {
        let Ok(Ok(response)) =
            tokio::time::timeout(self.timeout, self.client.request(request)).await
        else {
            return Answer::NoAnswer;
        };
        let answer = Answer::Status {
            status: response.status().as_u16(),
            retry_after: retry_after_from(response.headers(), at),
        };
        let drain = Limited::new(response.into_body(), DRAIN_BYTES).collect();
        // Whether the body ends in time decides only whether the connection is kept.
        let _ = tokio::time::timeout(DRAIN_TIME, drain).await;
        answer
    }
}
