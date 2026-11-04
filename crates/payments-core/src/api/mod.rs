// Author: Nicola Antonello

//! The HTTP API (ADR-0023).

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
