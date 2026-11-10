// Author: Nicola Antonello

//! The PSP's notifications over HTTP (ADR-0030, ADR-0032, ADR-0043): `POST /webhooks/mock`.
//!
//! A fake PSP delivers each notification to the router, served on a port of its own, and
//! reads the answer as a PSP does: its status, and its `Retry-After`. What the fake cannot
//! send, such as a header twice, goes to the router without a socket, signed in the test.

mod common;

use std::error::Error;
use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::api::{Pools, get, send};
use common::crashes::{NOTIFICATION_AT_COMMIT, install_crash_points, plan_death};
use common::payments::{create_command, fee_rate};
use common::{merchant_id, runtime_pool, runtime_pool_of_one, test_uuid};
use mock_psp::signing::{SigningKey, signature_header};
use mock_psp::{Attempt, MockPsp, Tamper};
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, Webhooks, router};
use payments_core::ids::PaymentId;
use payments_core::notifications::SigningKeys;
use payments_core::payments::{create_payment, register_merchant};
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tokio::net::TcpListener;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// What a PSP reads of an answer: its status, and its `Retry-After` in seconds.
fn answered(status: u16, retry_after: Option<u32>) -> Attempt {
    Attempt::Answered {
        status,
        retry_after,
    }
}

/// What the route holds to hear from this fake: the keys that it signs with.
fn webhooks_of(fake: &MockPsp) -> Webhooks {
    let keys = SigningKeys::parse(&fake.signing_keys().join(" ")).expect("the fake's keys");
    Webhooks::new(keys)
}

/// What the route holds to hear from a test that signs with this key.
fn webhooks_for(key: &SigningKey) -> Webhooks {
    Webhooks::new(SigningKeys::parse(&key.written()).expect("a key that the fake made"))
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

/// A server that holds a fake's keys, and the fake, which delivers to it.
struct World {
    pools: Pools,
    fake: MockPsp,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let world = Self {
            pools: Pools::login(pool).await?,
            fake: MockPsp::start().await?,
        };
        world.serve_with(world.pools.system.clone()).await;
        Ok(world)
    }

    /// Serves a router whose system role's pool is `system`, and has the fake deliver
    /// there from now on.
    async fn serve_with(&self, system: PgPool) {
        let state = AppState::new(
            self.pools.tenant.clone(),
            system,
            self.pools.operator.digest(),
            webhooks_of(&self.fake),
        );
        let address = serve(router(state)).await;
        self.fake
            .deliver_to(&format!("http://{address}/webhooks/mock"));
    }

    /// Payment `n`, which the service holds.
    async fn payment(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        held_payment(&self.pools.system, n).await
    }

    /// The ID of a new notification that `payment` changed.
    fn notify(&self, payment: Uuid) -> String {
        self.fake.notify_again(payment)
    }

    async fn close(self) {
        self.pools.close().await;
    }
}

async fn held_payment(system: &PgPool, n: u32) -> Result<PaymentId, Box<dyn Error>> {
    let merchant = merchant_id(n);
    register_merchant(system, merchant, fee_rate()).await?;
    let command = create_command(n, merchant);
    create_payment(system, &command).await?;
    Ok(command.id)
}

/// The notifications recorded: each one's event ID and the payment that it names.
async fn recorded(pool: &PgPool) -> sqlx::Result<Vec<(String, Uuid)>> {
    let rows = sqlx::query!("SELECT event_id, payment_id FROM psp_notifications ORDER BY event_id")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.event_id, row.payment_id))
        .collect())
}

/// A notification's body, as the fake writes one.
fn body_naming(payment: Uuid) -> Vec<u8> {
    format!(
        r#"{{"type":"payment.updated","timestamp":"2026-01-01T00:00:00Z","data":{{"payment_id":"{payment}"}}}}"#
    )
    .into_bytes()
}

/// A notification, signed now with `key`, as a PSP sends it.
fn signed(key: &SigningKey, id: &str, body: &[u8]) -> Request<Body> {
    let now = Utc::now().timestamp();
    Request::builder()
        .method(Method::POST)
        .uri("/webhooks/mock")
        .header("content-type", "application/json")
        .header("webhook-id", id)
        .header("webhook-timestamp", now.to_string())
        .header(
            "webhook-signature",
            signature_header(std::slice::from_ref(key), id, now, body),
        )
        .body(Body::from(body.to_vec()))
        .expect("a valid request")
}

#[sqlx::test]
async fn a_verified_notification_is_recorded_and_answered_202(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let event = world.notify(payment.get());
    // The fake sends no API key and no idempotency key: the route needs neither.
    assert_eq!(world.fake.deliver(&event).await, answered(202, None));
    assert_eq!(recorded(&pool).await?, [(event, payment.get())]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_copy_is_answered_202_and_recorded_once(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let event = world.notify(payment.get());
    for _ in 0..3 {
        assert_eq!(world.fake.deliver(&event).await, answered(202, None));
    }
    assert_eq!(recorded(&pool).await?, [(event, payment.get())]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_notification_about_a_payment_that_the_service_does_not_hold_is_kept_and_answered_202(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let unknown = test_uuid(9);
    let event = world.notify(unknown);
    assert_eq!(world.fake.deliver(&event).await, answered(202, None));
    assert_eq!(recorded(&pool).await?, [(event, unknown)]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_spoiled_notification_is_refused_with_400_and_recorded_nowhere(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let event = world.notify(payment.get());
    // Times well outside the window: its ends are tested with a fixed clock.
    let now = Utc::now().timestamp();
    for tamper in [
        Tamper::WrongKey,
        Tamper::Timestamp(now - 400),
        Tamper::Timestamp(now + 400),
        Tamper::ChangedBody,
    ] {
        assert_eq!(
            world.fake.deliver_tampered(&event, tamper).await,
            answered(400, None),
            "{tamper:?}"
        );
    }
    assert_eq!(recorded(&pool).await?, []);
    // The same notification, unspoiled, is taken.
    assert_eq!(world.fake.deliver(&event).await, answered(202, None));
    assert_eq!(recorded(&pool).await?, [(event, payment.get())]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn every_refusal_is_the_same_problem_and_records_nothing(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let key = SigningKey::generate();
    let state = AppState::new(
        pools.tenant.clone(),
        pools.system.clone(),
        pools.operator.digest(),
        webhooks_for(&key),
    );
    let body = body_naming(test_uuid(1));
    let mut unsigned = signed(&key, "msg_1", &body);
    unsigned.headers_mut().remove("webhook-signature");
    let mut twice = signed(&key, "msg_1", &body);
    let signature = twice.headers()["webhook-signature"].clone();
    twice.headers_mut().append("webhook-signature", signature);
    let too_large = vec![b' '; 2 * 1024 * 1024 + 1];
    for (what, request) in [
        ("no signature", unsigned),
        ("two signature headers", twice),
        ("an ID with a dot", signed(&key, "msg.1", &body)),
        (
            "another key",
            signed(&SigningKey::generate(), "msg_1", &body),
        ),
        (
            "a body that is not JSON",
            signed(&key, "msg_1", b"not json"),
        ),
        ("a body that names no payment", signed(&key, "msg_1", b"{}")),
        ("a body above 2 MB", signed(&key, "msg_1", &too_large)),
    ] {
        let reply = send(router(state.clone()), request).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{what}");
        assert_eq!(
            reply.header("content-type"),
            "application/problem+json",
            "{what}"
        );
        assert_eq!(reply.body, Problem::NotificationRefused.body(), "{what}");
    }
    assert_eq!(recorded(&pool).await?, []);
    // A notification that verifies, against the same state.
    let reply = send(router(state), signed(&key, "msg_1", &body)).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.body, "");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_notification_that_cannot_be_recorded_is_a_503_that_asks_for_it_again(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let key = SigningKey::generate();
    let closed = runtime_pool(&pool).await?;
    closed.close().await;
    let state = AppState::new(
        pools.tenant.clone(),
        closed,
        pools.operator.digest(),
        webhooks_for(&key),
    );
    let body = body_naming(test_uuid(1));
    let reply = send(router(state), signed(&key, "msg_1", &body)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.header("retry-after"), "5");
    assert_eq!(reply.body, Problem::Unavailable.body());
    assert_eq!(recorded(&pool).await?, []);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_whose_commit_dies_is_a_503_and_the_copy_that_follows_gets_202(
    pool: PgPool,
) -> TestResult {
    install_crash_points(&pool).await?;
    let world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let event = world.notify(payment.get());
    let one = runtime_pool_of_one(&pool).await?;
    world.serve_with(one.clone()).await;
    plan_death(&one, NOTIFICATION_AT_COMMIT).await?;
    // The outcome is unknown, so the route asks for the notification again, and the PSP
    // reads when.
    assert_eq!(world.fake.deliver(&event).await, answered(503, Some(5)));
    assert_eq!(recorded(&pool).await?, []);
    assert_eq!(world.fake.deliver(&event).await, answered(202, None));
    assert_eq!(recorded(&pool).await?, [(event, payment.get())]);
    one.close().await;
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_route_records_as_the_system_role_without_the_merchants_pool(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let key = SigningKey::generate();
    let closed = runtime_pool(&pool).await?;
    closed.close().await;
    let state = AppState::new(
        closed,
        pools.system.clone(),
        pools.operator.digest(),
        webhooks_for(&key),
    );
    let payment = held_payment(&pools.system, 1).await?;
    let reply = send(
        router(state),
        signed(&key, "msg_1", &body_naming(payment.get())),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(
        recorded(&pool).await?,
        [("msg_1".to_owned(), payment.get())]
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn only_the_mock_psp_takes_notifications_and_only_by_post(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let other = Request::builder()
        .method(Method::POST)
        .uri("/webhooks/other")
        .body(Body::empty())?;
    let reply = send(router(pools.state()), other).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, Problem::NotFound.body());
    let reply = send(router(pools.state()), get("/webhooks/mock")).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(reply.body, Problem::MethodNotAllowed.body());
    pools.close().await;
    Ok(())
}
