// Author: Nicola Antonello

//! The HTTP API (ADR-0023).

mod credential;
pub mod problem;

use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use sqlx::PgPool;

pub use credential::Merchant;
use problem::Problem;

/// What a request can reach: the two login roles' pools (ADR-0025).
#[derive(Debug, Clone)]
pub struct AppState {
    tenant: PgPool,
    system: PgPool,
}

impl AppState {
    /// The pool that merchant routes hold, whose role sees nothing until a transaction
    /// names its merchant, and the system role's, for looking a key up.
    #[must_use]
    pub fn new(tenant: PgPool, system: PgPool) -> Self {
        Self { tenant, system }
    }
}

/// Every route, under the one middleware.
pub fn router() -> Router {
    Router::new()
        .fallback(no_such_route)
        .layer(middleware::from_fn(in_its_own_task))
}

async fn no_such_route() -> Problem {
    Problem::NotFound
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
