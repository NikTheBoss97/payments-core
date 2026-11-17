// Author: Nicola Antonello

//! A sender for the fake's own tests, which delivers to it as the relay will, and signs
//! with HMAC code of its own rather than the fake's (ADR-0056).
//!
//! Cargo compiles each file directly under `tests/` as its own crate. Each test file that
//! needs these helpers includes this one with `mod common;`.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of these helpers"
)]

use axum::body::Bytes;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::{Client, Error};
use hyper_util::rt::TokioExecutor;
use mock_merchant::MockMerchant;
use sha2::Sha256;
use time::{Date, Month, OffsetDateTime};
use uuid::Uuid;

/// The legacy client of ADR-0036, with its one retry switched off, as the relay's will be.
pub type Sender = Client<HttpConnector, Full<Bytes>>;

pub fn sender() -> Sender {
    Client::builder(TokioExecutor::new())
        .retry_canceled_requests(false)
        .build_http()
}

/// A new secret, written as the server writes one: `whsec_` and the base64 of 32 bytes
/// from the operating system.
pub fn new_secret() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's random bytes");
    format!("whsec_{}", STANDARD.encode(bytes))
}

/// A fake that trusts a new secret, and that secret.
pub async fn merchant() -> (MockMerchant, String) {
    let merchant = MockMerchant::start().await.expect("a free local port");
    let secret = new_secret();
    merchant
        .trust(&secret)
        .expect("a secret written as the server writes one");
    (merchant, secret)
}

/// The payment that a test's event names.
pub fn payment(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// The time that every test's event carries, as its body writes it.
pub const EVENT_TIME: &str = "2026-11-16T10:20:30.123456Z";

/// [`EVENT_TIME`], built apart from any parsing.
pub fn event_time() -> OffsetDateTime {
    Date::from_calendar_date(2026, Month::November, 16)
        .and_then(|day| day.with_hms_micro(10, 20, 30, 123_456))
        .expect("a valid time")
        .assume_utc()
}

/// What the relay sends for one event, before it is signed.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub id: String,
    pub timestamp: String,
    pub body: Vec<u8>,
}

impl Delivery {
    /// An event about this payment under this ID, stamped with the true time.
    pub fn of(id: &str, payment_id: Uuid) -> Self {
        let body = serde_json::json!({
            "type": "payment.updated",
            "timestamp": EVENT_TIME,
            "data": { "payment_id": payment_id },
        });
        Self {
            id: id.to_owned(),
            timestamp: OffsetDateTime::now_utc().unix_timestamp().to_string(),
            body: serde_json::to_vec(&body).expect("JSON"),
        }
    }

    /// Its `webhook-signature` under this written secret: `v1,` and the base64 of the
    /// HMAC-SHA256 of `id.timestamp.body`, computed here and not by the fake.
    pub fn signed_by(&self, secret: &str) -> String {
        let key = STANDARD
            .decode(secret.strip_prefix("whsec_").expect("a written secret"))
            .expect("base64");
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("HMAC takes a key of any length");
        mac.update(format!("{}.{}.", self.id, self.timestamp).as_bytes());
        mac.update(&self.body);
        format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
    }

    /// Its three headers, signed under this written secret.
    pub fn headers(&self, secret: &str) -> Vec<(&'static str, String)> {
        vec![
            ("webhook-id", self.id.clone()),
            ("webhook-timestamp", self.timestamp.clone()),
            ("webhook-signature", self.signed_by(secret)),
        ]
    }
}

/// What the fake answered: its status and its headers.
#[derive(Debug)]
pub struct Answered {
    pub status: StatusCode,
    pub headers: HeaderMap,
}

/// Sends this request and reads the whole answer, so that its connection can serve again.
async fn send(sender: &Sender, request: Request<Full<Bytes>>) -> Result<Answered, Error> {
    let response = sender.request(request).await?;
    let status = response.status();
    let headers = response.headers().clone();
    let _body = response.into_body().collect().await;
    Ok(Answered { status, headers })
}

/// Posts a body with these headers to `url`.
pub async fn post(
    sender: &Sender,
    url: &str,
    headers: &[(&'static str, String)],
    body: Vec<u8>,
) -> Result<Answered, Error> {
    let mut request = Request::post(url).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let request = request
        .body(Full::new(Bytes::from(body)))
        .expect("a well-formed request");
    send(sender, request).await
}

/// Calls `url` with a `GET`, which no route of the fake serves.
pub async fn get(sender: &Sender, url: &str) -> Result<Answered, Error> {
    let request = Request::get(url)
        .body(Full::default())
        .expect("a well-formed request");
    send(sender, request).await
}

/// Delivers an event signed under this secret, as one attempt of the relay will.
pub async fn deliver(
    sender: &Sender,
    url: &str,
    secret: &str,
    delivery: &Delivery,
) -> Result<Answered, Error> {
    post(
        sender,
        url,
        &delivery.headers(secret),
        delivery.body.clone(),
    )
    .await
}
