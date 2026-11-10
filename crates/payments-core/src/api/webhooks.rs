// Author: Nicola Antonello

//! The PSP's notifications: `POST /webhooks/mock` (ADR-0030, ADR-0032, ADR-0043).
//!
//! The route takes no API key and no idempotency key: the signature says who sent a
//! notification, and its event ID tells its copies apart. It verifies a notification
//! before it parses the body, records it in a short transaction as the system role, and
//! answers 202. Every refusal gets the same 400, and a failure to record gets a 503 that
//! asks the PSP to send it again. A notification decides nothing, so nothing in it moves
//! a payment (ADR-0031): the catch-up that follows the answer reads what the PSP decided
//! from its history, in a task of its own, at most [`Webhooks::CATCH_UPS`] at once.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tokio::sync::Semaphore;

use super::AppState;
use super::problem::{Problem, chain};
use crate::accounts::Psp;
use crate::catch_up::{CatchUpError, CaughtUp, catch_up};
use crate::ids::PaymentId;
use crate::notifications::{RecordError, Recorded, SigningKeys, record, verify};
use crate::psp::PspClient;

/// How long the PSP waits before it sends again a notification that could not be
/// recorded, in seconds: the first delay of the specification's example schedule
/// (ADR-0043).
const RETRY_AFTER_SECONDS: &str = "5";

/// What the PSP's route holds (ADR-0043): the keys that verify its notifications, a
/// client of its API, which reads the histories, and the permits of the catch-ups that
/// notifications start.
#[derive(Debug, Clone)]
pub struct Webhooks {
    keys: Arc<SigningKeys>,
    psp: Arc<PspClient>,
    catch_ups: Arc<Semaphore>,
}

impl Webhooks {
    /// How many catch-ups that notifications start may run at once (ADR-0043).
    pub const CATCH_UPS: usize = 4;

    /// The route's view of the PSP: a client of its API, and the keys that its
    /// notifications may be signed with.
    #[must_use]
    pub fn new(psp: PspClient, keys: SigningKeys) -> Self {
        Self {
            keys: Arc::new(keys),
            psp: Arc::new(psp),
            catch_ups: Arc::new(Semaphore::new(Self::CATCH_UPS)),
        }
    }

    /// How many catch-ups that notifications started are running now.
    #[must_use]
    pub fn catch_ups_running(&self) -> usize {
        Self::CATCH_UPS - self.catch_ups.available_permits()
    }

    /// Catches `payment` up in a task of its own, if a permit is free, which the task
    /// holds until it ends. Without one, the payment waits for the sweep, as it does after
    /// a catch-up that fails (ADR-0043). Nothing here waits, so neither does the answer.
    /// The catch-up costs no token of the retry budget, and is never retried (ADR-0032,
    /// ADR-0042).
    fn catch_up_later(&self, system: &PgPool, payment: PaymentId) {
        let Ok(permit) = Arc::clone(&self.catch_ups).try_acquire_owned() else {
            tracing::info!("payment {payment}: every catch-up is busy, so the sweep catches it up");
            return;
        };
        let (system, psp) = (system.clone(), Arc::clone(&self.psp));
        tokio::spawn(async move {
            let caught_up = catch_up(&system, &psp, payment).await;
            drop(permit);
            report(payment, &caught_up);
        });
    }
}

/// Logs what a catch-up after a notification did, when an operator needs to know: a stop,
/// or a failure, which the sweep repairs (ADR-0043).
fn report(payment: PaymentId, caught_up: &Result<CaughtUp, CatchUpError>) {
    match caught_up {
        Ok(CaughtUp::Current { .. } | CaughtUp::AlreadyStopped) => {}
        Ok(CaughtUp::Stopped { stop, .. }) => tracing::warn!(
            "payment {payment} stopped at position {} of the PSP's history: {}",
            stop.position,
            stop.reason.as_str()
        ),
        Err(failed) => tracing::warn!(
            "catching payment {payment} up failed, so the sweep catches it up: {}",
            chain(failed)
        ),
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
        Ok(Recorded::Known(payment)) => state.webhooks.catch_up_later(&state.system, payment),
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
