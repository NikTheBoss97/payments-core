// Author: Nicola Antonello

//! The fake's one route (ADR-0056): `POST /webhooks`, where the relay delivers. Any other
//! path is a `404`, so that a wrong URL fails a test.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use time::OffsetDateTime;

use crate::{Answer, Shared};

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/webhooks", post(deliver))
        .with_state(shared)
}

/// Takes one delivery. The fake records everything before the first `await`, so a client
/// that hangs up, which drops this future, undoes nothing that the fake did.
async fn deliver(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let _answering = shared.answering();
    match shared.receive(&headers, body, OffsetDateTime::now_utc()) {
        Answer::Status(status) => status.into_response(),
        Answer::RetryAfter { status, seconds } => {
            (status, [(header::RETRY_AFTER, HeaderValue::from(seconds))]).into_response()
        }
        Answer::Delay(delay) => {
            tokio::time::sleep(delay).await;
            StatusCode::OK.into_response()
        }
    }
}
