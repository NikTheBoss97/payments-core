// Author: Nicola Antonello

//! A client for the fake's own tests, which call it over HTTP as the service will.
//!
//! Cargo compiles each file directly under `tests/` as its own crate. Each test file
//! that needs these helpers includes this one with `mod common;`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of these helpers"
)]

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mock_psp::MockPsp;
use serde_json::Value;
use uuid::Uuid;

/// What the fake answered.
#[derive(Debug)]
pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Answer {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("the fake answers JSON")
    }
}

/// The legacy client of ADR-0036, with its one retry switched off.
pub fn client() -> Client<HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build_http()
}

/// Calls the fake, with this key if any, and this body if any.
pub async fn call(method: Method, url: &str, key: Option<&str>, body: Option<Vec<u8>>) -> Answer {
    let mut request = Request::builder().method(method).uri(url);
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .expect("a well-formed request");
    let response = client().request(request).await.expect("the fake answers");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("a whole body")
        .to_bytes();
    Answer {
        status,
        headers,
        body,
    }
}

/// Sends a request to the fake, as the service's sender will.
pub async fn send(psp: &MockPsp, request: &Value) -> Answer {
    let body = serde_json::to_vec(request).expect("JSON");
    call(
        Method::POST,
        &format!("{}/v1/requests", psp.url()),
        Some(psp.api_key()),
        Some(body),
    )
    .await
}

/// Reads a payment's history, as the service's catch-up will.
pub async fn read_history(psp: &MockPsp, payment_id: Uuid) -> Answer {
    call(
        Method::GET,
        &format!("{}/v1/payments/{payment_id}/history", psp.url()),
        Some(psp.api_key()),
        None,
    )
    .await
}

/// A request of this kind for this payment, with a new ID.
pub fn request(payment_id: Uuid, kind: &str, amount: i64) -> Value {
    serde_json::json!({
        "id": Uuid::now_v7(),
        "payment_id": payment_id,
        "kind": kind,
        "amount": amount,
        "currency": "EUR",
    })
}

/// The ID in a request built by [`request`].
pub fn id_of(request: &Value) -> Uuid {
    serde_json::from_value(request["id"].clone()).expect("a UUID")
}
