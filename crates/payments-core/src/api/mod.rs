// Author: Nicola Antonello

//! The HTTP API (ADR-0023).

mod credential;
mod keyed;
mod operator;
mod path_id;
mod payments;
pub mod problem;
mod webhooks;

use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::credentials::SecretDigest;
use crate::ids::RequestId;
pub use credential::Merchant;
pub use keyed::KeyedRequest;
pub use payments::OwnPayment;
use problem::Problem;
pub use webhooks::Webhooks;

/// What a request can reach: the two login roles' pools (ADR-0025), what the operator's
/// secret hashes to, which is all that the server holds of it (ADR-0024), what the PSP's
/// route holds, and the routes' end of the handover to the sender's task (ADR-0043).
#[derive(Debug, Clone)]
pub struct AppState {
    tenant: PgPool,
    system: PgPool,
    operator: SecretDigest,
    webhooks: Webhooks,
    handovers: mpsc::Sender<RequestId>,
}

impl AppState {
    /// The pool that merchant routes hold, whose role sees nothing until a transaction
    /// names its merchant, the system role's, for looking a key up, for the operator's
    /// commands and for the PSP's notifications, the digest of the operator's secret, what
    /// the PSP's route holds, and where the routes hand a recorded request over.
    #[must_use]
    pub fn new(
        tenant: PgPool,
        system: PgPool,
        operator: SecretDigest,
        webhooks: Webhooks,
        handovers: mpsc::Sender<RequestId>,
    ) -> Self {
        Self {
            tenant,
            system,
            operator,
            webhooks,
            handovers,
        }
    }

    /// Hands a request that this attempt recorded and committed over to the sender's task,
    /// which sends it at once (ADR-0043). A full or closed channel drops it, and the task's
    /// search finds the request, which it looks up before it sends (ADR-0042).
    fn hand_over(&self, request: RequestId) {
        let _dropped = self.handovers.try_send(request);
    }
}

/// Every route, under the one middleware. The fallbacks come before the layer, or they
/// would run outside it.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/merchants", post(operator::register))
        .route("/merchants/{id}/api-keys", post(operator::issue_key))
        .route("/api-keys/{id}/revoke", post(operator::revoke_key))
        .route("/payments", post(payments::create))
        .route("/payments/{id}", get(payments::read_payment))
        .route("/payments/{id}/authorize", post(payments::authorize))
        .route("/payments/{id}/capture", post(payments::capture))
        .route("/payments/{id}/refund", post(payments::refund))
        .route("/payments/{id}/void", post(payments::void))
        .route("/payments/{id}/cancel", post(payments::cancel))
        .route(
            "/payments/{id}/requests/{request_id}",
            get(payments::read_request),
        )
        .route("/webhooks/mock", post(webhooks::receive))
        .fallback(no_such_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(middleware::from_fn(in_its_own_task))
        .with_state(state)
}

async fn no_such_route() -> Problem {
    Problem::NotFound
}

async fn wrong_method() -> Problem {
    Problem::MethodNotAllowed
}

/// Runs the rest of the stack in a task of its own, from the extractors to the response.
///
/// hyper drops a handler's future when its client goes away, and a dropped command is a
/// lost `COMMIT` (ADR-0014), so nothing that a request does may run in that future. A
/// dropped [`JoinHandle`](tokio::task::JoinHandle) leaves its task running, so here a
/// client that hangs up cancels nothing. Never add a timeout around this: waits are
/// bounded on the server (ADR-0018).
pub async fn in_its_own_task(request: Request, next: Next) -> Response {
    match tokio::spawn(next.run(request)).await {
        Ok(response) => response,
        // Nothing aborts this task, so it panicked, or the runtime is shutting down, in
        // which case nobody reads this.
        Err(_) => Problem::Internal.into_response(),
    }
}
