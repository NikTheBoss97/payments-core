// Author: Nicola Antonello

//! The PSP in the tests: the service's client for a fake (ADR-0029), requests posted to
//! the fake as the sender will post them, and PSPs that answer what the test writes, or
//! never answer, for what the fake cannot serve.

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use mock_psp::MockPsp;
use mock_psp::wire::Detail;
use payments_core::accounts::Psp;
use payments_core::catch_up::{CaughtUp, catch_up};
use payments_core::ids::{PaymentId, RequestId};
use payments_core::money::{Currency, PositiveAmount};
use payments_core::psp::{Outgoing, PspClient};
use payments_core::requests::{Ask, RecordRequest, RequestKind, record_request};
use payments_core::sender::{Report, Sender};
use payments_core::tenancy::Tenant;
use serde_json::json;
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::payments::{AUTHORIZED, REQUESTED};
use super::runtime_pool;

/// How long the tests' clients wait for the PSP.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The service's client for this fake.
pub fn client_of(psp: &MockPsp) -> PspClient {
    PspClient::new(Psp::Mock, psp.url(), psp.api_key(), TIMEOUT)
        .expect("the fake's address and key make a client")
}

/// The sender of this fake, as the server's task holds it (ADR-0042), its budget full now.
pub fn sender_of(psp: &MockPsp) -> Sender {
    Sender::new(client_of(psp), Utc::now())
}

/// Hands `request` over to `sender` and takes a step, as the server does once a route has
/// committed the request (ADR-0043): the step sends it at once and records the PSP's
/// answer. Every jitter is drawn as zero.
pub async fn hand_over_and_step(
    sender: &mut Sender,
    system: &PgPool,
    request: &RequestId,
) -> Report {
    sender.hand_over(request);
    sender
        .send_due(system, Utc::now(), &mut || 0)
        .await
        .expect("the requests that wait for the PSP can be listed")
}

/// Takes a step of `sender` now, which finds the requests that wait, looks each up and
/// sends it, as the server's task does for a request whose handover was lost (ADR-0042).
/// Every jitter is drawn as zero, so nothing waits.
pub async fn step(sender: &mut Sender, system: &PgPool) -> Report {
    sender
        .send_due(system, Utc::now(), &mut || 0)
        .await
        .expect("the requests that wait for the PSP can be listed")
}

/// Catches `payment` up with what this fake decided, as the catch-up after a notification
/// does (ADR-0031), and checks that it applied something.
pub async fn caught_up(system: &PgPool, psp: &MockPsp, payment: PaymentId) {
    let caught_up = catch_up(system, &client_of(psp), payment)
        .await
        .expect("the fake's history can be read and applied");
    assert!(
        matches!(caught_up, CaughtUp::Current { applied: 1.. }),
        "{caught_up:?}"
    );
}

/// Records `ask` on `payment` as the merchant of `tenant`, and takes it through this fake as
/// the server does: handed over and sent, approved whole, and caught up (ADR-0043).
pub async fn applied(
    psp: &MockPsp,
    sender: &mut Sender,
    tenant: Tenant<'_>,
    system: &PgPool,
    payment: PaymentId,
    ask: Ask,
) {
    let command = RecordRequest {
        request: RequestId::now_v7(),
        payment,
        ask,
    };
    record_request(tenant, &command)
        .await
        .expect("the request can be recorded");
    hand_over_and_step(sender, system, &command.request).await;
    psp.approve(command.request.get());
    caught_up(system, psp, payment).await;
}

/// The PSP's side of a test: a fake, the sender that the server's task would hold for it,
/// and the system role's pool, which sends and catches up (ADR-0043). Each step that it
/// takes is one that the server takes.
pub struct PspSide {
    pub fake: MockPsp,
    pub sender: Sender,
    pub system: PgPool,
}

impl PspSide {
    /// Starts a fake on a free local port, with its sender and a pool of the system role.
    pub async fn start(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let fake = MockPsp::start().await?;
        Ok(Self {
            sender: sender_of(&fake),
            system: runtime_pool(pool).await?,
            fake,
        })
    }

    /// Records `command` as the merchant of `tenant`, and sends it as the server does once
    /// the route has committed it: the fake holds it, undecided.
    pub async fn sent(&mut self, tenant: Tenant<'_>, command: &RecordRequest) {
        record_request(tenant, command)
            .await
            .expect("the request can be recorded");
        hand_over_and_step(&mut self.sender, &self.system, &command.request).await;
    }

    /// Applies what the fake decided about `payment`, and checks that it applied something.
    pub async fn caught_up(&self, payment: PaymentId) {
        caught_up(&self.system, &self.fake, payment).await;
    }

    /// Records `ask` and takes it through the fake, approved whole, as [`applied`] does.
    pub async fn applied(&mut self, tenant: Tenant<'_>, payment: PaymentId, ask: Ask) {
        applied(
            &self.fake,
            &mut self.sender,
            tenant,
            &self.system,
            payment,
            ask,
        )
        .await;
    }

    /// Takes a payment that is `created` to `authorized`: its merchant asks for
    /// [`REQUESTED`], and the PSP authorizes [`AUTHORIZED`], less than asked (ADR-0016).
    pub async fn authorized(&mut self, tenant: Tenant<'_>, payment: PaymentId) {
        let command = RecordRequest {
            request: RequestId::now_v7(),
            payment,
            ask: Ask::Authorize(positive(REQUESTED)),
        };
        self.sent(tenant, &command).await;
        self.fake.approve_for(command.request.get(), AUTHORIZED);
        self.caught_up(payment).await;
    }

    /// Takes a payment that is `authorized` to `captured`, for all of [`AUTHORIZED`].
    pub async fn captured(&mut self, tenant: Tenant<'_>, payment: PaymentId) {
        self.applied(tenant, payment, Ask::Capture(positive(AUTHORIZED)))
            .await;
    }

    pub async fn close(self) {
        self.system.close().await;
    }
}

fn positive(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// What this fake decided about `request`: the PSP's word, to compare with ours.
pub fn decided(psp: &MockPsp, payment: PaymentId, request: &RequestId) -> Detail {
    let history = psp
        .history(payment.get())
        .expect("the fake holds the payment");
    history
        .facts
        .into_iter()
        .map(|fact| fact.detail)
        .find(|detail| match detail {
            Detail::Authorized { request_id, .. }
            | Detail::Declined { request_id }
            | Detail::Captured { request_id, .. }
            | Detail::CaptureRefused { request_id }
            | Detail::Voided { request_id }
            | Detail::VoidRefused { request_id }
            | Detail::Refunded { request_id, .. }
            | Detail::RefundRefused { request_id } => *request_id == request.get(),
            Detail::Expired | Detail::Settled => false,
        })
        .expect("the fake decided the request")
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

/// What the sender sends for this recorded request, in euros like [`send`].
pub fn outgoing(command: &RecordRequest) -> Outgoing {
    let (kind, amount) = wire_ask(&command.ask);
    Outgoing {
        request: RequestId::try_from(command.request.get()).expect("a request's ID"),
        payment: command.payment,
        kind: RequestKind::parse(kind).expect("a kind of the wire's"),
        amount,
        currency: Currency::parse("EUR").expect("a currency"),
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

/// What each route of a [`ScriptedPsp`] answers, and how many calls each took.
#[derive(Debug)]
struct Scripted {
    history: Mutex<Script>,
    send: Mutex<Script>,
    histories: AtomicUsize,
    sends: AtomicUsize,
}

/// A PSP whose routes answer whatever the test wrote last, for any payment, with or
/// without a key: the answers that the fake never gives. It counts the calls to each
/// route, so a test can tell a lookup from a send.
pub struct ScriptedPsp {
    url: String,
    scripted: Arc<Scripted>,
    server: JoinHandle<()>,
}

impl ScriptedPsp {
    /// Starts one on a free local port, answering 404 until the test writes an answer.
    pub async fn start() -> Self {
        let not_found = Script {
            status: StatusCode::NOT_FOUND,
            retry_after: None,
            body: Vec::new(),
        };
        let scripted = Arc::new(Scripted {
            history: Mutex::new(not_found.clone()),
            send: Mutex::new(not_found),
            histories: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
        });
        let router = Router::new()
            .route("/v1/payments/{payment_id}/history", get(history))
            .route("/v1/requests", post(take))
            .with_state(Arc::clone(&scripted));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a free local port");
        let url = format!("http://{}", listener.local_addr().expect("a bound address"));
        let server = tokio::spawn(async move {
            let _ended = axum::serve(listener, router).await;
        });
        Self {
            url,
            scripted,
            server,
        }
    }

    /// A client of this PSP, with a key that it ignores.
    pub fn client(&self) -> PspClient {
        PspClient::new(Psp::Mock, &self.url, "scripted", TIMEOUT).expect("a local address")
    }

    /// Answers every later call, to either route, with this status, `Retry-After` and
    /// body.
    pub fn answer(&self, status: StatusCode, retry_after: Option<u32>, body: Vec<u8>) {
        self.answer_history(status, retry_after, body.clone());
        self.answer_sends(status, retry_after, body);
    }

    /// Answers every later read of a history with this status, `Retry-After` and body.
    pub fn answer_history(&self, status: StatusCode, retry_after: Option<u32>, body: Vec<u8>) {
        *self
            .scripted
            .history
            .lock()
            .expect("no test panicked holding it") = Script {
            status,
            retry_after,
            body,
        };
    }

    /// Answers every later send with this status, `Retry-After` and body.
    pub fn answer_sends(&self, status: StatusCode, retry_after: Option<u32>, body: Vec<u8>) {
        *self
            .scripted
            .send
            .lock()
            .expect("no test panicked holding it") = Script {
            status,
            retry_after,
            body,
        };
    }

    /// How many histories it was asked for, and how many requests it was sent.
    pub fn calls(&self) -> (usize, usize) {
        (
            self.scripted.histories.load(Ordering::SeqCst),
            self.scripted.sends.load(Ordering::SeqCst),
        )
    }
}

impl Drop for ScriptedPsp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn history(State(scripted): State<Arc<Scripted>>) -> Response {
    scripted.histories.fetch_add(1, Ordering::SeqCst);
    answer(&scripted.history)
}

async fn take(State(scripted): State<Arc<Scripted>>) -> Response {
    scripted.sends.fetch_add(1, Ordering::SeqCst);
    answer(&scripted.send)
}

fn answer(script: &Mutex<Script>) -> Response {
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
