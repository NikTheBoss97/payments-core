// Author: Nicola Antonello

//! The relay's input and output (ADR-0052, ADR-0053, ADR-0062): the handover of the
//! deliveries that a catch-up committed, one attempt to deliver an event to a merchant's
//! endpoint, the search for the deliveries not finished, and the records of what the relay
//! decided. The relay decides which attempts to make and what their answers mean; this
//! module reads the database, posts the attempts and writes the records.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{Method, Request, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::db::{CommitError, commit};
use crate::events::{self, BodyError};
use crate::ids::{DeliveryId, EndpointId, EventId};
use crate::operator::EndpointUrl;
use crate::relay::{Answer, Delivery, Effect, Search, Snapshot, XactId};
use crate::retries::retry_after_from;
use crate::signing::{MasterKey, sign};

/// How long an attempt waits for its answer's head (ADR-0053).
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// How much of an answer's body is read after its head, only so that the connection can be
/// used again (ADR-0053).
const DRAIN_BYTES: usize = 65_536;

/// How long an answer's body may take to end after its head (ADR-0053).
const DRAIN_TIME: Duration = Duration::from_secs(1);

/// How many deliveries the catch-ups can hand over before the relay's task takes them
/// (ADR-0052). Beyond it, a delivery is dropped, and the task's search finds it.
pub const HANDOVERS: usize = 1_024;

/// Where a catch-up hands over the deliveries that it committed (ADR-0052, ADR-0057): a
/// channel to the relay's task, which drops a delivery when it is full or closed, so that
/// a catch-up never waits on the relay. Without a channel, as in tests and in a process
/// that runs no relay, it hands over nothing, and the relay's search finds every delivery.
#[derive(Debug, Clone)]
pub struct Handover(Option<mpsc::Sender<Delivery>>);

impl Handover {
    /// A handover through this channel.
    #[must_use]
    pub fn new(channel: mpsc::Sender<Delivery>) -> Self {
        Self(Some(channel))
    }

    /// A handover that hands over nothing.
    #[must_use]
    pub fn none() -> Self {
        Self(None)
    }

    /// Hands over each delivery, dropping one that the channel cannot take at once.
    pub fn hand_over(&self, deliveries: impl IntoIterator<Item = Delivery>) {
        let Some(channel) = &self.0 else {
            return;
        };
        for delivery in deliveries {
            // A full or closed channel drops it, and the relay's search finds it.
            let _dropped = channel.try_send(delivery);
        }
    }
}

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

/// Why a search could not be made: the snapshot or the deliveries could not be read. The
/// relay learns nothing, and searches again at a later step.
#[derive(Debug, thiserror::Error)]
#[error("the deliveries not finished could not be searched")]
pub struct SearchError {
    #[source]
    source: sqlx::Error,
}

/// Why an effect could not be recorded.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// The insert failed, and wrote nothing.
    #[error("the effect could not be written")]
    Database(#[source] sqlx::Error),
    /// The `COMMIT` failed, so the effect may or may not be recorded (ADR-0014).
    #[error("the effect may or may not be recorded")]
    NotConfirmed(#[source] CommitError),
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

/// Reads, as the system role, the snapshot and then the deliveries that `search` asks for
/// (ADR-0062). The snapshot comes first, in a statement of its own: its `xmax`, and the
/// transactions then in progress. A later statement reads the deliveries not finished,
/// with no success and an endpoint not disabled: of every transaction for a full search,
/// and otherwise of the transactions that had not completed at the previous snapshot, in
/// its list or at or above its `xmax`. Each comes with the transaction that wrote it, which
/// the relay compares with the new snapshot (ADR-0064). The relay keeps the new snapshot for
/// the next search.
///
/// # Errors
///
/// [`SearchError`] when either could not be read.
pub async fn search(
    pool: &PgPool,
    search: &Search,
) -> Result<(Snapshot, Vec<Delivery>), SearchError> {
    let failed = |source| SearchError { source };
    let read = sqlx::query!(
        r#"SELECT pg_snapshot_xmax(s)::text::bigint AS "xmax!",
                  ARRAY(SELECT x::text::bigint FROM pg_snapshot_xip(s) AS x) AS "in_progress!"
             FROM pg_current_snapshot() AS s"#
    )
    .fetch_one(pool)
    .await
    .map_err(failed)?;
    let snapshot = Snapshot {
        xmax: read_xact_id(read.xmax),
        in_progress: read.in_progress.into_iter().map(read_xact_id).collect(),
    };
    let found = match search {
        Search::Full => {
            sqlx::query_as!(
                Found,
                r#"SELECT d.id, d.endpoint_id, d.event_id,
                          d.xact_id::text::bigint AS "xact_id!"
                   FROM webhook_deliveries d
                  WHERE NOT EXISTS (SELECT 1 FROM webhook_delivery_successes s
                                     WHERE s.delivery_id = d.id)
                    AND NOT EXISTS (SELECT 1 FROM webhook_endpoint_disablements x
                                     WHERE x.endpoint_id = d.endpoint_id)
                  ORDER BY d.id"#
            )
            .fetch_all(pool)
            .await
        }
        Search::Since(previous) => {
            let in_progress: Vec<i64> = previous.in_progress.iter().copied().map(bound).collect();
            // Bound as bigints and read back through text as xid8s, as ADR-0055 binds
            // them, so that the index on xact_id serves both conditions.
            sqlx::query_as!(
                Found,
                r#"SELECT d.id, d.endpoint_id, d.event_id,
                          d.xact_id::text::bigint AS "xact_id!"
                   FROM webhook_deliveries d
                  WHERE (d.xact_id >= $1::bigint::text::xid8
                         OR d.xact_id = ANY ($2::bigint[]::text[]::xid8[]))
                    AND NOT EXISTS (SELECT 1 FROM webhook_delivery_successes s
                                     WHERE s.delivery_id = d.id)
                    AND NOT EXISTS (SELECT 1 FROM webhook_endpoint_disablements x
                                     WHERE x.endpoint_id = d.endpoint_id)
                  ORDER BY d.id"#,
                bound(previous.xmax),
                &in_progress
            )
            .fetch_all(pool)
            .await
        }
    }
    .map_err(failed)?;
    Ok((snapshot, found.into_iter().map(Found::delivery).collect()))
}

/// A delivery as a search reads it, with the transaction that wrote it (ADR-0064).
struct Found {
    id: Uuid,
    endpoint_id: Uuid,
    event_id: Uuid,
    xact_id: i64,
}

impl Found {
    fn delivery(self) -> Delivery {
        Delivery {
            id: DeliveryId::try_from(self.id)
                .expect("a delivery's ID is a UUIDv7: webhook_deliveries_id_check"),
            endpoint: EndpointId::try_from(self.endpoint_id)
                .expect("an endpoint's ID is a UUIDv7: webhook_endpoints_id_check"),
            event: EventId::try_from(self.event_id)
                .expect("an event's ID is a UUIDv7: payment_events_id_check"),
            xact: read_xact_id(self.xact_id),
        }
    }
}

/// A transaction's ID as a snapshot or a row gives it: an `xid8` read through text as a
/// `bigint`.
pub(crate) fn read_xact_id(id: i64) -> XactId {
    XactId::new(u64::try_from(id).expect("an xid8 cast to a bigint is not negative"))
}

/// A transaction's ID that a snapshot gave, to bind as the `bigint` it was read as.
fn bound(id: XactId) -> i64 {
    i64::try_from(id.get()).expect("the IDs that a snapshot gives fit in a bigint")
}

/// Records what the relay decided (ADR-0053): a delivery's success, or an endpoint's
/// disablement for the relay's reason, once, as the system role, and committed through the
/// one function of ADR-0014. Recording either again writes nothing, and a first
/// disablement keeps its reason, as the operator's does (ADR-0050).
///
/// # Errors
///
/// See [`RecordError`].
pub async fn record(pool: &PgPool, effect: Effect) -> Result<(), RecordError> {
    let mut tx = pool.begin().await.map_err(RecordError::Database)?;
    let inserted = match effect {
        Effect::Record(delivery) => {
            sqlx::query!(
                "INSERT INTO webhook_delivery_successes (delivery_id) VALUES ($1) \
                 ON CONFLICT (delivery_id) DO NOTHING",
                delivery.get()
            )
            .execute(&mut *tx)
            .await
        }
        Effect::Disable(endpoint, reason) => {
            sqlx::query!(
                "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason) \
                 VALUES ($1, $2) ON CONFLICT (endpoint_id) DO NOTHING",
                endpoint.get(),
                reason.as_str()
            )
            .execute(&mut *tx)
            .await
        }
    };
    match inserted {
        Ok(_) => commit(tx).await.map_err(RecordError::NotConfirmed),
        Err(failed) => {
            // The failed insert aborted the transaction: drop it (ADR-0014).
            drop(tx);
            Err(RecordError::Database(failed))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Delivery `n`, its endpoint and its event, with UUIDv7 IDs made from `n`.
    fn delivery(n: u128) -> Delivery {
        let id = |kind: u128| {
            Uuid::from_u128(0x019b_76da_a800_7000_8000_0000_0000_0000 | (kind << 32) | n)
        };
        Delivery {
            id: DeliveryId::try_from(id(1)).expect("a UUIDv7"),
            endpoint: EndpointId::try_from(id(2)).expect("a UUIDv7"),
            event: EventId::try_from(id(3)).expect("a UUIDv7"),
            xact: XactId::new(1),
        }
    }

    #[test]
    fn a_handover_passes_each_delivery_and_drops_what_the_channel_cannot_hold() {
        let (channel, mut handed) = mpsc::channel(2);
        let handover = Handover::new(channel);
        handover.hand_over([delivery(1), delivery(2), delivery(3)]);
        assert_eq!(handed.try_recv().ok(), Some(delivery(1)));
        assert_eq!(handed.try_recv().ok(), Some(delivery(2)));
        assert!(
            handed.try_recv().is_err(),
            "the third was dropped, not waited for"
        );
        // A closed channel drops them too, and nothing waits.
        drop(handed);
        handover.hand_over([delivery(4)]);
    }

    #[test]
    fn no_handover_hands_over_nothing() {
        Handover::none().hand_over([delivery(1)]);
    }
}
