// Author: Nicola Antonello

//! The fake's routes (ADR-0029). `POST /v1/requests` takes a request and answers at once
//! whether the PSP holds it, and `GET /v1/payments/{payment_id}/history` serves what the
//! PSP holds for a payment. Both need the service's API key (ADR-0030), and either can
//! answer a failure that the test scripted instead (ADR-0032).

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use uuid::Uuid;

use crate::model::Taken;
use crate::wire::{Received, Request};
use crate::{Failure, Route, Shared, now};

pub(crate) fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/v1/requests", post(take))
        .route("/v1/payments/{payment_id}/history", get(history))
        .with_state(shared)
}

async fn take(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&shared, &headers) {
        return problem(StatusCode::UNAUTHORIZED, "the API key is missing or wrong");
    }
    if let Some(failure) = shared.next_failure(Route::Requests) {
        return failed(failure);
    }
    let request = match serde_json::from_slice::<Request>(&body) {
        Ok(request) if request.is_well_formed() => request,
        _ => return problem(StatusCode::BAD_REQUEST, "the request is malformed"),
    };
    let id = request.id;
    let taken = shared.model().take(request, now());
    match taken {
        Taken::Conflict => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "this request ID came with other content",
        ),
        Taken::Received => {
            if shared.lose_this_reply() {
                // The PSP took the request, and its reply never comes: the caller's own
                // timeout ends the call, with the outcome unknown (ADR-0032).
                std::future::pending::<()>().await;
            }
            json(StatusCode::ACCEPTED, &Received::of(id))
        }
    }
}

async fn history(
    State(shared): State<Arc<Shared>>,
    Path(payment_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&shared, &headers) {
        return problem(StatusCode::UNAUTHORIZED, "the API key is missing or wrong");
    }
    if let Some(failure) = shared.next_failure(Route::History) {
        return failed(failure);
    }
    let history = Uuid::parse_str(&payment_id)
        .ok()
        .and_then(|payment_id| shared.model().history(payment_id));
    match history {
        Some(history) => json(StatusCode::OK, &history),
        None => problem(
            StatusCode::NOT_FOUND,
            "the PSP holds nothing for this payment",
        ),
    }
}

/// Whether the call carries the service's key. A plain comparison: the fake guards
/// nothing, and the server's own checks are its business (ADR-0024).
fn authorized(shared: &Shared, headers: &HeaderMap) -> bool {
    let expected = format!("Bearer {}", shared.api_key());
    headers
        .get(header::AUTHORIZATION)
        .is_some_and(|value| value.as_bytes() == expected.as_bytes())
}

fn failed(failure: Failure) -> Response {
    match failure {
        Failure::TooManyRequests { retry_after } => {
            retry_later(StatusCode::TOO_MANY_REQUESTS, retry_after)
        }
        Failure::Unavailable { retry_after } => {
            retry_later(StatusCode::SERVICE_UNAVAILABLE, retry_after)
        }
        Failure::InternalError => problem(StatusCode::INTERNAL_SERVER_ERROR, "the PSP failed"),
    }
}

fn retry_later(status: StatusCode, seconds: u32) -> Response {
    let mut response = problem(status, "try again later");
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    response
}

fn json(status: StatusCode, body: &impl Serialize) -> Response {
    let bytes = serde_json::to_vec(body).expect("the wire format is plain data");
    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

fn problem(status: StatusCode, detail: &str) -> Response {
    json(status, &serde_json::json!({ "error": detail }))
}
