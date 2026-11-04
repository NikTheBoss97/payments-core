// Author: Nicola Antonello

//! What runs a request (ADR-0023). hyper drops a handler's future when its client goes
//! away, and a dropped command is a lost `COMMIT` (ADR-0014), so one middleware runs
//! every request in a task of its own.
//!
//! These tests use real sockets on a multi-thread runtime, as the server does, because
//! only a real connection can hang up. Their handlers stand in for a command: they
//! wait for a signal instead of for a lock, so that nothing depends on timing.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use payments_core::api::in_its_own_task;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};
use tower::ServiceExt;

/// Signals between a test and the handler it watches. A `Notify` keeps one signal for
/// a waiter that has not arrived yet, so no order of events can lose one.
#[derive(Default)]
struct Probe {
    entered: Notify,
    release: Notify,
    finished: Notify,
    dropped: Notify,
}

type Shared = Arc<Probe>;

/// Signals when it is dropped, which is what cancelling a future looks like from inside.
struct SignalOnDrop(Shared);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        self.0.dropped.notify_one();
    }
}

/// A command as a handler sees it: it starts, waits for something, and finishes.
async fn work(State(probe): State<Shared>) -> &'static str {
    probe.entered.notify_one();
    probe.release.notified().await;
    probe.finished.notify_one();
    "done"
}

/// The same, for the control: it says when its own future was dropped.
async fn work_that_reports_being_dropped(State(probe): State<Shared>) -> &'static str {
    let _reports = SignalOnDrop(probe.clone());
    work(State(probe.clone())).await
}

/// Outside the middleware, the future that hyper holds: it is dropped when hyper has
/// noticed that the client went away. A test waits for that instead of sleeping.
async fn reports_the_hang_up(
    State(probe): State<Shared>,
    request: Request,
    next: Next,
) -> Response {
    let _reports = SignalOnDrop(probe);
    next.run(request).await
}

async fn reads_a_body(State(probe): State<Shared>, body: Bytes) -> String {
    probe.entered.notify_one();
    format!("{} bytes", body.len())
}

async fn panics() -> &'static str {
    panic!("on purpose: this test checks what a panic becomes")
}

/// Serves this router on a port of its own, until the runtime ends with the test.
async fn serve(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free local port");
    let address = listener.local_addr().expect("a bound socket's address");
    tokio::spawn(async move { axum::serve(listener, router).await });
    address
}

/// Sends the start of a request, and leaves the connection open.
async fn send(address: SocketAddr, request: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("the server is listening");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the server is reading");
    stream
}

/// Fails the test rather than hanging it, if something that must happen does not.
async fn within<T>(what: &str, happens: impl Future<Output = T>) -> T {
    timeout(Duration::from_secs(10), happens)
        .await
        .unwrap_or_else(|_| panic!("{what} did not happen within 10 seconds"))
}

const WORK: &str = "GET /work HTTP/1.1\r\nHost: test\r\n\r\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_runs_to_its_end_after_its_client_hangs_up() {
    let probe = Shared::default();
    let router = Router::new()
        .route("/work", get(work))
        .layer(middleware::from_fn(in_its_own_task))
        .layer(middleware::from_fn_with_state(
            probe.clone(),
            reports_the_hang_up,
        ))
        .with_state(probe.clone());
    let address = serve(router).await;

    let stream = send(address, WORK).await;
    within("the handler starting", probe.entered.notified()).await;
    drop(stream);
    within("hyper noticing the hang-up", probe.dropped.notified()).await;

    probe.release.notify_one();
    within("the handler finishing", probe.finished.notified()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_middleware_a_client_that_hangs_up_cancels_its_handler() {
    // The control. It shows that the hazard is real on these versions of axum and hyper,
    // and that these tests really do hang up: without it, the test above could pass
    // because nothing was ever cancelled.
    let probe = Shared::default();
    let router = Router::new()
        .route("/work", get(work_that_reports_being_dropped))
        .with_state(probe.clone());
    let address = serve(router).await;

    let stream = send(address, WORK).await;
    within("the handler starting", probe.entered.notified()).await;
    drop(stream);
    within("the handler being dropped", probe.dropped.notified()).await;

    probe.release.notify_one();
    let finished = timeout(Duration::from_millis(200), probe.finished.notified()).await;
    assert!(finished.is_err(), "a dropped handler finished");
}

#[tokio::test]
async fn a_panic_in_a_handler_becomes_a_500() {
    let router = Router::new()
        .route("/panic", get(panics))
        .layer(middleware::from_fn(in_its_own_task));
    let request = Request::builder()
        .uri("/panic")
        .body(Body::empty())
        .expect("a valid request");
    let response = router.oneshot(request).await.expect("a router never fails");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // What the answer says is in `api_errors`.
}

fn body_router(probe: &Shared) -> Router {
    Router::new()
        .route("/body", post(reads_a_body))
        .layer(middleware::from_fn(in_its_own_task))
        .layer(middleware::from_fn_with_state(
            probe.clone(),
            reports_the_hang_up,
        ))
        .with_state(probe.clone())
}

const THREE_OF_TEN: &str =
    "POST /body HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: 10\r\n\r\nabc";

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_starts_only_once_its_body_has_arrived() {
    let probe = Shared::default();
    let address = serve(body_router(&probe)).await;
    let mut stream = send(address, THREE_OF_TEN).await;

    let started = timeout(Duration::from_millis(200), probe.entered.notified()).await;
    assert!(started.is_err(), "a handler started on part of its body");

    stream
        .write_all(b"defghij")
        .await
        .expect("the server is reading");
    within("the handler starting", probe.entered.notified()).await;
    let mut answer = String::new();
    within("the answer", stream.read_to_string(&mut answer))
        .await
        .expect("a readable answer");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.ends_with("10 bytes"), "{answer}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_never_starts_if_its_client_hangs_up_during_the_body() {
    let probe = Shared::default();
    let address = serve(body_router(&probe)).await;
    let stream = send(address, THREE_OF_TEN).await;
    sleep(Duration::from_millis(100)).await;

    drop(stream);
    within("hyper noticing the hang-up", probe.dropped.notified()).await;

    let started = timeout(Duration::from_millis(200), probe.entered.notified()).await;
    assert!(started.is_err(), "a handler started on part of its body");
}
