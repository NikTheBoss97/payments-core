// Author: Nicola Antonello

//! The PSP's notifications: `POST /webhooks/mock` (ADR-0030, ADR-0032, ADR-0043).
//!
//! The route takes no API key and no idempotency key: the signature says who sent a
//! notification, and its event ID tells its copies apart. It verifies a notification
//! before it parses the body, records it in a short transaction as the system role, and
//! answers 202. Every refusal gets the same 400, and a failure to record gets a 503 that
//! asks the PSP to send it again. A notification decides nothing, so nothing in it moves
//! a payment (ADR-0031).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sqlx::types::chrono::Utc;

use super::AppState;
use super::problem::Problem;
use crate::accounts::Psp;
use crate::notifications::{RecordError, Recorded, SigningKeys, record, verify};

/// How long the PSP waits before it sends again a notification that could not be
/// recorded, in seconds: the first delay of the specification's example schedule
/// (ADR-0043).
const RETRY_AFTER_SECONDS: &str = "5";

/// What the PSP's route holds (ADR-0043): the keys that verify its notifications.
#[derive(Debug, Clone)]
pub struct Webhooks {
    keys: Arc<SigningKeys>,
}

impl Webhooks {
    /// The route's view of the PSP: the keys that its notifications may be signed with.
    #[must_use]
    pub fn new(keys: SigningKeys) -> Self {
        Self {
            keys: Arc::new(keys),
        }
    }
}

/// Takes one notification: see the module's documentation. A refusal logs nothing, since
/// anyone can post one (ADR-0043).
pub(super) async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    // A body above the limit is refused like any other that cannot be read.
    let Ok(body) = body else {
        return Problem::NotificationRefused.into_response();
    };
    let Ok(notification) = verify(&headers, &body, &state.webhooks.keys, Utc::now()) else {
        return Problem::NotificationRefused.into_response();
    };
    match record(&state.system, Psp::Mock, &notification).await {
        Ok(Recorded::Known(_)) => {}
        Ok(Recorded::Unknown(payment)) => tracing::warn!(
            "notification {} names payment {payment}, which the service does not hold",
            notification.event_id
        ),
        Err(failed) => return unavailable(&failed),
    }
    StatusCode::ACCEPTED.into_response()
}

/// The answer to a notification that could not be recorded, or whose record's `COMMIT`
/// has an unknown outcome: a 503 that asks the PSP to send it again, which a copy
/// recorded meanwhile answers with a 202 (ADR-0032).
fn unavailable(failed: &RecordError) -> Response {
    let mut response = Problem::Unavailable.because(failed).into_response();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_static(RETRY_AFTER_SECONDS),
    );
    response
}
