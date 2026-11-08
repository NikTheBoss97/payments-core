// Author: Nicola Antonello

//! A client and a receiver for the fake's own tests, which call it over HTTP as the
//! service will, and take its notifications as the service's `POST /webhooks/mock`
//! will.
//!
//! Cargo compiles each file directly under `tests/` as its own crate. Each test file
//! that needs these helpers includes this one with `mod common;`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of these helpers"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mock_psp::MockPsp;
use serde_json::Value;
use sha2::Sha256;
use tokio::net::TcpListener;
use uuid::Uuid;

/// A status, and a `Retry-After` in seconds if any.
pub type Reply = (StatusCode, Option<u32>);

/// What the receiver got, and what it answers next.
#[derive(Clone, Default)]
pub struct Receiver {
    received: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

impl Receiver {
    /// Answers the next deliveries with these, then with 202s.
    pub fn answer(&self, replies: impl IntoIterator<Item = Reply>) {
        self.replies.lock().expect("unpoisoned").extend(replies);
    }

    /// The deliveries received so far: their headers and bodies.
    pub fn received(&self) -> Vec<(HeaderMap, Bytes)> {
        self.received.lock().expect("unpoisoned").clone()
    }
}

async fn receive(State(receiver): State<Receiver>, headers: HeaderMap, body: Bytes) -> Response {
    receiver
        .received
        .lock()
        .expect("unpoisoned")
        .push((headers, body));
    let next = receiver.replies.lock().expect("unpoisoned").pop_front();
    let (status, retry_after) = next.unwrap_or((StatusCode::ACCEPTED, None));
    let mut response = status.into_response();
    if let Some(seconds) = retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

/// A receiver on a free local port, and its endpoint.
pub async fn receiver() -> (Receiver, String) {
    let receiver = Receiver::default();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free local port");
    let endpoint = format!(
        "http://{}/webhooks/mock",
        listener.local_addr().expect("bound")
    );
    let router = Router::new()
        .route("/webhooks/mock", post(receive))
        .with_state(receiver.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    (receiver, endpoint)
}

/// A header's value, which the test expects.
pub fn header_of<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .unwrap_or_else(|| panic!("no {name}"))
        .to_str()
        .expect("ASCII")
}

/// Whether one of the listed signatures verifies under this written key: HMAC-SHA256
/// over `msg_id.timestamp.payload`, computed here and not by the fake.
pub fn verifies(written_key: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    let secret = STANDARD
        .decode(written_key.strip_prefix("whsec_").expect("a written key"))
        .expect("base64");
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret).expect("any key length");
    mac.update(header_of(headers, "webhook-id").as_bytes());
    mac.update(b".");
    mac.update(header_of(headers, "webhook-timestamp").as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()));
    header_of(headers, "webhook-signature")
        .split(' ')
        .any(|signature| signature == expected)
}

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
