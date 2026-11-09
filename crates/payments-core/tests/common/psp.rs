// Author: Nicola Antonello

//! The PSP in the tests: the service's client for a fake (ADR-0029), requests posted to
//! the fake as the sender will post them, and PSPs that answer what the test writes, or
//! never answer, for what the fake cannot serve.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use mock_psp::MockPsp;
use payments_core::accounts::Psp;
use payments_core::psp::PspClient;
use payments_core::requests::{Ask, RecordRequest};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// How long the tests' clients wait for the PSP.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The service's client for this fake.
pub fn client_of(psp: &MockPsp) -> PspClient {
    PspClient::new(Psp::Mock, psp.url(), psp.api_key(), TIMEOUT)
        .expect("the fake's address and key make a client")
}

/// The kind and the amount that the wire gives a request (ADR-0038).
pub fn wire_ask(ask: &Ask) -> (&'static str, i64) {
    match ask {
        Ask::Authorize(amount) => ("authorize", amount.get()),
        Ask::Capture(amount) => ("capture", amount.get()),
        Ask::Refund(amount) => ("refund", amount.get()),
        Ask::Void => ("void", 0),
    }
}

/// Posts a recorded request to the fake, in euros, as the sender will once it exists
/// (ADR-0028), and checks that the fake holds it.
pub async fn send(psp: &MockPsp, command: &RecordRequest) {
    let (kind, amount) = wire_ask(&command.ask);
    let body = json!({
        "id": command.request.get(),
        "payment_id": command.payment.get(),
        "kind": kind,
        "amount": amount,
        "currency": "EUR",
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("{}/v1/requests", psp.url()))
        .header(header::AUTHORIZATION, format!("Bearer {}", psp.api_key()))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&body).expect("plain data"),
        )))
        .expect("a well-formed request");
    let response = Client::builder(TokioExecutor::new())
        .build_http()
        .request(request)
        .await
        .expect("the fake answers");
    assert_eq!(response.status(), StatusCode::ACCEPTED, "the fake holds it");
}

/// What a [`ScriptedPsp`] answers: a status, a `Retry-After` if any, and a body.
#[derive(Debug, Clone)]
struct Script {
    status: StatusCode,
    retry_after: Option<u32>,
    body: Vec<u8>,
}

/// A PSP whose history route answers whatever the test wrote last, for any payment,
/// with or without a key: the answers that the fake never gives.
pub struct ScriptedPsp {
    url: String,
    script: Arc<Mutex<Script>>,
    server: JoinHandle<()>,
}

impl ScriptedPsp {
    /// Starts one on a free local port, answering 404 until the test writes an answer.
    pub async fn start() -> Self {
        let script = Arc::new(Mutex::new(Script {
            status: StatusCode::NOT_FOUND,
            retry_after: None,
            body: Vec::new(),
        }));
        let router = Router::new()
            .route("/v1/payments/{payment_id}/history", get(answer))
            .with_state(Arc::clone(&script));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free local port");
        let url = format!("http://{}", listener.local_addr().expect("a bound address"));
        let server = tokio::spawn(async move {
            let _ended = axum::serve(listener, router).await;
        });
        Self {
            url,
            script,
            server,
        }
    }

    /// A client of this PSP, with a key that it ignores.
    pub fn client(&self) -> PspClient {
        PspClient::new(Psp::Mock, &self.url, "scripted", TIMEOUT).expect("a local address")
    }

    /// Answers every later call with this status, `Retry-After` and body.
    pub fn answer(&self, status: StatusCode, retry_after: Option<u32>, body: Vec<u8>) {
        *self.script.lock().expect("no test panicked holding it") = Script {
            status,
            retry_after,
            body,
        };
    }
}

impl Drop for ScriptedPsp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn answer(State(script): State<Arc<Mutex<Script>>>) -> Response {
    let script = script.lock().expect("no test panicked holding it").clone();
    let mut response = (script.status, script.body).into_response();
    if let Some(seconds) = script.retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

/// The address of a PSP that takes every connection and never answers, and the task
/// that holds the connections. Abort it to let them go.
pub async fn silent_psp() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free local port");
    let url = format!("http://{}", listener.local_addr().expect("a bound address"));
    let holder = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    (url, holder)
}

/// The address of a local port that nothing listens on, so a connection is refused.
pub async fn closed_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free local port");
    let url = format!("http://{}", listener.local_addr().expect("a bound address"));
    drop(listener);
    url
}
