// Author: Nicola Antonello

//! A merchant's commands on a payment, over HTTP (ADR-0023, ADR-0026, ADR-0046), with the
//! status and the storage of each answer. `authorize`, `capture`, `refund` and `void` record
//! a request to the PSP and answer 202; `cancel` stays a step of our own. Each state is
//! reached through the fake: the route records a request, a step of the sender sends it,
//! the fake decides it, and a catch-up applies it (ADR-0043). The moves themselves are
//! tested in the library: these tests attack what the routes add, which is parsing, the
//! mapping to an answer, the key and the handover.

mod common;

use std::error::Error;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::api::{
    Pools, Reply, a_merchant, accepted, bearer, get_with_authorization, post, records, send,
    unused_handovers, unused_webhooks,
};
use common::crashes::{balances, install_crash_points, plan_death};
use common::payments::{
    AUTHORIZED, FEE, requests_of, steps, unbalanced_currencies, uncaused_postings,
};
use common::psp::{caught_up, sender_of, step};
use common::{payment_id, tenant_pool_of_one};
use futures_util::future::join_all;
use mock_psp::MockPsp;
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, router};
use payments_core::background::HANDOVERS;
use payments_core::credentials::ApiKeySecret;
use payments_core::ids::PaymentId;
use payments_core::sender::Sender;
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const CREATE: &str = r#"{"currency":"EUR","amount":10000}"#;
/// An authorization of 80.00, which the fake approves whole.
const AUTHORIZE: &str = r#"{"amount":8000}"#;
const CAPTURE: &str = r#"{"amount":8000}"#;

fn refund_of(minor: i64) -> String {
    format!(r#"{{"amount":{minor}}}"#)
}

/// `POST /payments/{id}/{verb}` under this key, as the merchant whose secret this is.
async fn command_as(
    pools: &Pools,
    secret: &ApiKeySecret,
    payment: PaymentId,
    verb: &str,
    key: &str,
    body: &str,
) -> Reply {
    let path = format!("/payments/{payment}/{verb}");
    send(
        router(pools.state()),
        post(&path, secret, key, body.to_owned()),
    )
    .await
}

/// A merchant with its key, the server's two pools, and a fake PSP with the sender that the
/// server's task would hold.
struct World {
    pools: Pools,
    secret: ApiKeySecret,
    fake: MockPsp,
    sender: Sender,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let pools = Pools::login(pool).await?;
        let (_, secret) = a_merchant(&pools, 1).await?;
        let fake = MockPsp::start().await?;
        Ok(Self {
            sender: sender_of(&fake),
            pools,
            secret,
            fake,
        })
    }

    /// `POST /payments/{id}/{verb}` under this key.
    async fn command(&self, payment: PaymentId, verb: &str, key: &str, body: &str) -> Reply {
        command_as(&self.pools, &self.secret, payment, verb, key, body).await
    }

    /// A new payment, through its route.
    async fn created(&self, key: &str) -> Result<PaymentId, Box<dyn Error>> {
        let reply = send(
            router(self.pools.state()),
            post("/payments", &self.secret, key, CREATE),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
        let body: serde_json::Value = serde_json::from_str(&reply.body)?;
        let id = body["payment_id"].as_str().ok_or("no payment_id")?;
        Ok(PaymentId::try_from(Uuid::parse_str(id)?)?)
    }

    /// Takes the request that `reply` recorded through the fake: a step sends it, the fake
    /// approves it whole, and a catch-up applies it.
    async fn applied(&mut self, payment: PaymentId, reply: &Reply) {
        let request = accepted(reply, payment);
        step(&mut self.sender, &self.pools.system).await;
        self.fake.approve(request);
        caught_up(&self.pools.system, &self.fake, payment).await;
    }

    /// A payment authorized for 80.00, through its routes and the fake.
    async fn authorized(&mut self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.created(&format!("create-{n}")).await?;
        let reply = self
            .command(payment, "authorize", &format!("authorize-{n}"), AUTHORIZE)
            .await;
        self.applied(payment, &reply).await;
        Ok(payment)
    }

    /// A payment captured for 80.00, through its routes and the fake.
    async fn captured(&mut self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.authorized(n).await?;
        let reply = self
            .command(payment, "capture", &format!("capture-{n}"), CAPTURE)
            .await;
        self.applied(payment, &reply).await;
        Ok(payment)
    }

    /// The payment as `GET /payments/{id}` shows it.
    async fn read(&self, payment: PaymentId) -> Result<serde_json::Value, Box<dyn Error>> {
        let reply = send(
            router(self.pools.state()),
            get_with_authorization(&format!("/payments/{payment}"), &bearer(&self.secret)),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        Ok(serde_json::from_str(&reply.body)?)
    }

    async fn close(self) {
        self.pools.close().await;
    }
}

/// A cancel's answer: the step that it applied, as phase 3 answered every command.
#[track_caller]
fn assert_step(reply: &Reply, seq: i32, from: &str, to: &str, refundable: i64) {
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    assert_eq!(
        reply.body,
        format!(r#"{{"seq":{seq},"from":"{from}","to":"{to}","refundable":{refundable}}}"#)
    );
}

#[sqlx::test]
async fn a_payment_goes_through_its_life_and_the_money_follows(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let merchant = common::merchant_id(1);
    let payment = world.created("create").await?;

    let reply = world.command(payment, "authorize", "a", AUTHORIZE).await;
    world.applied(payment, &reply).await;
    assert_eq!(world.read(payment).await?["status"], "authorized");
    assert_eq!(balances(&world.pools.system, merchant).await?, [0, 0, 0]);

    let reply = world.command(payment, "capture", "c", CAPTURE).await;
    world.applied(payment, &reply).await;
    assert_eq!(
        balances(&world.pools.system, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );

    let reply = world
        .command(payment, "refund", "r1", &refund_of(3_000))
        .await;
    world.applied(payment, &reply).await;
    assert_eq!(
        balances(&world.pools.system, merchant).await?,
        [5_000, FEE - 5_000, -FEE]
    );

    let reply = world
        .command(payment, "refund", "r2", &refund_of(5_000))
        .await;
    world.applied(payment, &reply).await;
    assert_eq!(
        balances(&world.pools.system, merchant).await?,
        [0, FEE, -FEE]
    );

    assert_eq!(world.read(payment).await?["status"], "refunded");
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_replays_its_202_and_records_no_second_request(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let payment = world.captured(1).await?;

    let first = world
        .command(payment, "refund", "r", &refund_of(1_000))
        .await;
    let second = world
        .command(payment, "refund", "r", &refund_of(1_000))
        .await;

    accepted(&first, payment);
    assert_eq!(second.status, first.status);
    assert_eq!(second.body, first.body);
    assert_eq!(second.headers, first.headers);
    assert_eq!(requests_of(&pool, payment).await?, 3);
    world.applied(payment, &first).await;
    assert_eq!(world.read(payment).await?["refundable"], AUTHORIZED - 1_000);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_refund_racing_record_one_request_and_give_one_answer(
    pool: PgPool,
) -> TestResult {
    // The sharpest case: a partial refund still fits after it is recorded, so without the
    // key each copy, with its fresh IDs, would record another.
    let mut world = World::new(&pool).await?;
    let payment = world.captured(1).await?;
    let body = refund_of(1);

    let replies = join_all((0..8).map(|_| world.command(payment, "refund", "r", &body))).await;

    accepted(&replies[0], payment);
    for reply in &replies {
        assert_eq!(reply.status, replies[0].status, "{reply:?}");
        assert_eq!(reply.body, replies[0].body);
        assert_eq!(reply.headers, replies[0].headers);
    }
    assert_eq!(requests_of(&pool, payment).await?, 3);
    world.applied(payment, &replies[0]).await;
    assert_eq!(world.read(payment).await?["refundable"], AUTHORIZED - 1);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_each_command_racing_give_one_answer(pool: PgPool) -> TestResult {
    // Copies wait on the payment's lock. By the time one gets it, the first has recorded
    // its request or moved the payment on, so it would be refused, and only the key can
    // give it the real answer.
    let mut world = World::new(&pool).await?;
    let to_authorize = world.created("create-1").await?;
    let to_capture = world.authorized(2).await?;
    let to_void = world.authorized(4).await?;
    let to_cancel = world.created("create-3").await?;

    for (verb, payment, body) in [
        ("authorize", to_authorize, AUTHORIZE),
        ("capture", to_capture, CAPTURE),
        ("void", to_void, ""),
    ] {
        let key = format!("race-{verb}");
        let before = requests_of(&pool, payment).await?;
        let replies = join_all((0..8).map(|_| world.command(payment, verb, &key, body))).await;
        accepted(&replies[0], payment);
        for reply in &replies {
            assert_eq!(reply.body, replies[0].body, "{verb}: {reply:?}");
            assert_eq!(reply.headers, replies[0].headers, "{verb}");
        }
        assert_eq!(requests_of(&pool, payment).await?, before + 1, "{verb}");
    }

    // A cancel stays a step of our own (ADR-0028).
    let replies =
        join_all((0..8).map(|_| world.command(to_cancel, "cancel", "race-cancel", ""))).await;
    for reply in &replies {
        assert_step(reply, 1, "created", "voided", 0);
    }
    assert_eq!(steps(&pool, to_cancel).await?, 2);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_new_key_is_a_new_request(pool: PgPool) -> TestResult {
    // The control for the three tests above: the same refund under another key does record
    // another request, so it is the key that stopped those copies.
    let mut world = World::new(&pool).await?;
    let payment = world.captured(1).await?;
    let body = refund_of(1);
    let first = world.command(payment, "refund", "first", &body).await;
    let second = world.command(payment, "refund", "second", &body).await;
    assert_ne!(accepted(&first, payment), accepted(&second, payment));
    assert_eq!(requests_of(&pool, payment).await?, 4);
    world.applied(payment, &first).await;
    world.applied(payment, &second).await;
    assert_eq!(world.read(payment).await?["refundable"], AUTHORIZED - 2);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_recorded_request_is_handed_over_once_and_never_by_a_replay(pool: PgPool) -> TestResult {
    // The route hands over only what its own attempt recorded and committed (ADR-0043).
    let world = World::new(&pool).await?;
    let payment = world.created("create").await?;
    let (handovers, mut handed_over) = mpsc::channel(HANDOVERS);
    let state = AppState::new(
        world.pools.tenant.clone(),
        world.pools.system.clone(),
        world.pools.operator.digest(),
        unused_webhooks(),
        handovers,
    );
    let path = format!("/payments/{payment}/authorize");

    let reply = send(
        router(state.clone()),
        post(&path, &world.secret, "a", AUTHORIZE),
    )
    .await;
    let request = accepted(&reply, payment);
    assert_eq!(handed_over.try_recv()?.get(), request);

    let again = send(
        router(state.clone()),
        post(&path, &world.secret, "a", AUTHORIZE),
    )
    .await;
    assert_eq!(again.body, reply.body);
    assert!(
        handed_over.try_recv().is_err(),
        "a replay hands nothing over"
    );

    // A refusal records no request, so it has none to hand over.
    let capture = format!("/payments/{payment}/capture");
    let refused = send(router(state), post(&capture, &world.secret, "c", CAPTURE)).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{refused:?}");
    assert!(
        handed_over.try_recv().is_err(),
        "a refusal hands nothing over"
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_illegal_move_is_a_409_that_is_stored(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created("create").await?;
    let before = records(&pool).await?;

    for (verb, body) in [("capture", CAPTURE.to_owned()), ("refund", refund_of(1))] {
        let reply = world.command(payment, verb, verb, &body).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{verb}: {reply:?}");
        assert_eq!(reply.header("content-type"), "application/problem+json");
        assert_eq!(reply.body, Problem::IllegalMove.body(), "{verb}");
    }
    assert_eq!(records(&pool).await?, before + 2);
    assert_eq!(requests_of(&pool, payment).await?, 0);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_amount_above_its_limit_is_a_422_that_is_stored(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;

    // Above the requested 100.00, the authorized 80.00, and the 80.00 still refundable.
    let payment = world.created("create").await?;
    let reply = world
        .command(payment, "authorize", "a", r#"{"amount":10001}"#)
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::AmountAboveLimit.body());
    assert_eq!(reply.header("content-type"), "application/problem+json");

    let payment = world.authorized(2).await?;
    let reply = world
        .command(payment, "capture", "c", r#"{"amount":8001}"#)
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::AmountAboveLimit.body());

    let payment = world.captured(3).await?;
    let before = records(&pool).await?;
    let reply = world
        .command(payment, "refund", "r", &refund_of(8_001))
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::AmountAboveLimit.body());
    assert_eq!(records(&pool).await?, before + 1);
    assert_eq!(requests_of(&pool, payment).await?, 2);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_late_duplicate_of_a_refusal_stays_a_refusal(pool: PgPool) -> TestResult {
    // A client was told no. The payment then moves on, so a copy of the same request,
    // delivered late, would be recorded if it were decided again (ADR-0026).
    let mut world = World::new(&pool).await?;
    let payment = world.created("create").await?;

    let refused = world.command(payment, "capture", "late", CAPTURE).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{refused:?}");

    let reply = world.command(payment, "authorize", "a", AUTHORIZE).await;
    world.applied(payment, &reply).await;

    let late = world.command(payment, "capture", "late", CAPTURE).await;
    assert_eq!(late.status, refused.status);
    assert_eq!(late.body, refused.body);
    assert_eq!(requests_of(&pool, payment).await?, 1);

    // The control: the same capture under a new key is recorded, so it was the key that held.
    let reply = world.command(payment, "capture", "fresh", CAPTURE).await;
    accepted(&reply, payment);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_is_for_one_route_and_one_payment(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created("create").await?;
    let another = world.created("create-another").await?;
    let first = world.command(payment, "authorize", "k", AUTHORIZE).await;
    accepted(&first, payment);

    for (what, target, verb, body) in [
        ("another route", payment, "capture", AUTHORIZE),
        ("another payment", another, "authorize", AUTHORIZE),
        ("another body", payment, "authorize", r#"{"amount":7000}"#),
    ] {
        let reply = world.command(target, verb, "k", body).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{what}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::KeyReused.body(), "{what}");
    }
    assert_eq!(requests_of(&pool, another).await?, 0);
    assert_eq!(requests_of(&pool, payment).await?, 1);
    world.close().await;
    Ok(())
}

/// The commands on a payment, each with a body that would apply if nothing else stopped it.
fn every_command() -> Vec<(&'static str, String)> {
    vec![
        ("authorize", AUTHORIZE.to_owned()),
        ("capture", CAPTURE.to_owned()),
        ("refund", refund_of(1)),
        ("void", String::new()),
        ("cancel", String::new()),
    ]
}

#[sqlx::test]
async fn another_merchants_payment_cannot_be_touched(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let (_, stranger) = a_merchant(&world.pools, 2).await?;
    let payment = world.created("create").await?;
    let before = records(&pool).await?;

    for (verb, body) in every_command() {
        let theirs = command_as(&world.pools, &stranger, payment, verb, verb, &body).await;
        let nobodys = command_as(&world.pools, &stranger, payment_id(99), verb, verb, &body).await;
        assert_eq!(theirs.status, StatusCode::NOT_FOUND, "{verb}: {theirs:?}");
        assert_eq!(theirs.body, Problem::NotFound.body(), "{verb}");
        assert_eq!(theirs.status, nobodys.status, "{verb}");
        assert_eq!(theirs.body, nobodys.body, "{verb}");
        assert_eq!(theirs.headers, nobodys.headers, "{verb}");
    }
    assert_eq!(records(&pool).await?, before);
    assert_eq!(requests_of(&pool, payment).await?, 0);
    assert_eq!(steps(&pool, payment).await?, 1);
    world.close().await;
    Ok(())
}

/// A `POST` with a credential and a body, and no idempotency key.
fn without_a_key(path: &str, secret: &ApiKeySecret, body: String) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("authorization", bearer(secret))
        .body(Body::from(body))
        .expect("a valid request")
}

#[sqlx::test]
async fn every_post_route_refuses_a_request_with_no_key(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created("create").await?;
    let before = records(&pool).await?;

    let mut routes = vec![("/payments".to_owned(), CREATE.to_owned())];
    for (verb, body) in every_command() {
        routes.push((format!("/payments/{payment}/{verb}"), body));
    }
    for (path, body) in routes {
        let reply = send(
            router(world.pools.state()),
            without_a_key(&path, &world.secret, body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}: {reply:?}");
        assert_eq!(reply.body, Problem::MalformedKey.body(), "{path}");
    }
    assert_eq!(records(&pool).await?, before);
    assert_eq!(requests_of(&pool, payment).await?, 0);
    world.close().await;
    Ok(())
}

/// Bodies that a command which asks the PSP for an amount must refuse. `time` names the
/// PSP's business time for that command, which a merchant no longer sends (ADR-0046).
fn unusable_bodies(time: &str) -> Vec<String> {
    vec![
        String::new(),
        "not JSON".to_owned(),
        "{}".to_owned(),
        "null".to_owned(),
        // A field unknown or misspelt, the PSP's time among them.
        format!(r#"{{"amount":1,"{time}":"2027-01-01T00:00:00Z"}}"#),
        r#"{"amount":1,"merchant_id":"anyone"}"#.to_owned(),
        r#"{"amount":1,"happened_at":"2027-01-01T00:00:00Z"}"#.to_owned(),
        r#"{"amounts":1}"#.to_owned(),
        // An amount that is no whole, positive number of minor units in range.
        r#"{"amount":0}"#.to_owned(),
        r#"{"amount":-1}"#.to_owned(),
        r#"{"amount":1.5}"#.to_owned(),
        r#"{"amount":1.0}"#.to_owned(),
        r#"{"amount":1e3}"#.to_owned(),
        r#"{"amount":"1"}"#.to_owned(),
        r#"{"amount":null}"#.to_owned(),
        r#"{"amount":1000000000000001}"#.to_owned(),
    ]
}

#[sqlx::test]
async fn a_body_that_cannot_be_used_is_refused_and_leaves_the_key_free(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;

    // Each payment is where its command would be recorded, so only the body can refuse it.
    let created_one = world.created("create").await?;
    let authorized_one = world.authorized(2).await?;
    let captured_one = world.captured(3).await?;
    let mut stored = records(&pool).await?;

    for (verb, payment, time, good) in [
        ("authorize", created_one, "expires_at", AUTHORIZE.to_owned()),
        ("capture", authorized_one, "captured_at", CAPTURE.to_owned()),
        ("refund", captured_one, "refunded_at", refund_of(1)),
    ] {
        let key = format!("key-{verb}");
        let before = requests_of(&pool, payment).await?;
        for body in unusable_bodies(time) {
            let reply = world.command(payment, verb, &key, &body).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{verb} {body}: {reply:?}"
            );
            assert_eq!(reply.body, Problem::MalformedBody.body(), "{verb} {body}");
        }
        assert_eq!(records(&pool).await?, stored, "{verb}");
        assert_eq!(requests_of(&pool, payment).await?, before, "{verb}");

        // Had any of them been stored, this would be a reuse of the key.
        let reply = world.command(payment, verb, &key, &good).await;
        accepted(&reply, payment);
        stored += 1;
    }
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_command_route_takes_only_post(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created("create").await?;
    for (verb, _) in every_command() {
        let reply = send(
            router(world.pools.state()),
            get_with_authorization(
                &format!("/payments/{payment}/{verb}"),
                &bearer(&world.secret),
            ),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{verb}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::MethodNotAllowed.body(), "{verb}");
        assert!(reply.header("allow").contains("POST"), "{verb}: {reply:?}");
    }
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_verb_that_is_no_command_is_a_404(pool: PgPool) -> TestResult {
    // decline, expire and settle report what the PSP decided, so they reach a payment only
    // through its history (ADR-0031). A merchant could otherwise settle its own payment.
    let mut world = World::new(&pool).await?;
    let payment = world.authorized(1).await?;
    for verb in ["decline", "expire", "settle", "delete"] {
        let reply = world.command(payment, verb, verb, "").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{verb}: {reply:?}");
        assert_eq!(reply.body, Problem::NotFound.body(), "{verb}");
    }
    assert_eq!(world.read(payment).await?["status"], "authorized");
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_payment_is_cancelled_before_its_authorization_and_voided_after(
    pool: PgPool,
) -> TestResult {
    let mut world = World::new(&pool).await?;

    let payment = world.created("create").await?;
    let reply = world.command(payment, "cancel", "c", "").await;
    assert_step(&reply, 1, "created", "voided", 0);
    let again = world.command(payment, "cancel", "c", "").await;
    assert_eq!(again.status, reply.status);
    assert_eq!(again.body, reply.body);
    assert_eq!(world.read(payment).await?["status"], "voided");

    let payment = world.authorized(2).await?;
    let reply = world.command(payment, "void", "v", "").await;
    let again = world.command(payment, "void", "v", "").await;
    assert_eq!(again.body, reply.body);
    world.applied(payment, &reply).await;
    assert_eq!(world.read(payment).await?["status"], "voided");
    assert_eq!(steps(&pool, payment).await?, 3);

    // Neither moved any money: nothing had been captured.
    assert_eq!(
        balances(&world.pools.system, common::merchant_id(1)).await?,
        [0, 0, 0]
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn cancel_and_void_are_final_refusals_where_the_status_forbids_them(
    pool: PgPool,
) -> TestResult {
    let mut world = World::new(&pool).await?;
    let fresh = world.created("create").await?;
    let held = world.authorized(2).await?;
    let taken = world.captured(3).await?;
    let before = records(&pool).await?;

    let cases = [
        ("void", fresh),
        ("cancel", held),
        ("void", taken),
        ("cancel", taken),
    ];
    for (n, (verb, payment)) in cases.into_iter().enumerate() {
        let reply = world.command(payment, verb, &format!("k-{n}"), "").await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{verb}: {reply:?}");
        assert_eq!(reply.body, Problem::IllegalMove.body(), "{verb}");
    }
    assert_eq!(records(&pool).await?, before + 4);
    assert_eq!(world.read(taken).await?["status"], "captured");
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_cancel_once_an_authorization_is_requested_is_a_final_conflict(
    pool: PgPool,
) -> TestResult {
    // The authorization is only recorded, not decided: the PSP may hold it, which only a
    // void releases (ADR-0039).
    let world = World::new(&pool).await?;
    let payment = world.created("create").await?;
    let reply = world.command(payment, "authorize", "a", AUTHORIZE).await;
    accepted(&reply, payment);
    let before = records(&pool).await?;

    let reply = world.command(payment, "cancel", "c", "").await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/problem+json");
    assert_eq!(reply.body, Problem::AuthorizationRequested.body());
    let again = world.command(payment, "cancel", "c", "").await;
    assert_eq!(again.body, reply.body);
    assert_eq!(records(&pool).await?, before + 1);
    assert_eq!(world.read(payment).await?["status"], "created");
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_body_on_a_route_that_takes_none_is_refused_and_leaves_the_key_free(
    pool: PgPool,
) -> TestResult {
    // Not even an empty object: a route that ignored a body could hide a client's mistake,
    // and two bodies that it ignored would still read as two requests under a key.
    let mut world = World::new(&pool).await?;
    let fresh = world.created("create").await?;
    let held = world.authorized(2).await?;

    for (verb, payment) in [("cancel", fresh), ("void", held)] {
        for body in ["{}", "null", " ", "\n", "anything"] {
            let reply = world.command(payment, verb, "k", body).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{verb} {body:?}");
            assert_eq!(reply.body, Problem::MalformedBody.body(), "{verb} {body:?}");
        }
        let key = format!("k-{verb}");
        let refused = world.command(payment, verb, &key, "{}").await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{verb}");
        let reply = world.command(payment, verb, &key, "").await;
        assert!(reply.status.is_success(), "{verb}: {reply:?}");
    }
    world.close().await;
    Ok(())
}

/// A payment for each command, in the state where it would be recorded or applied, with its
/// body, and whether it records a request rather than a step of our own.
async fn one_of_each(
    world: &mut World,
) -> Result<Vec<(&'static str, PaymentId, String, bool)>, Box<dyn Error>> {
    Ok(vec![
        (
            "authorize",
            world.created("create-1").await?,
            AUTHORIZE.to_owned(),
            true,
        ),
        (
            "capture",
            world.authorized(2).await?,
            CAPTURE.to_owned(),
            true,
        ),
        ("refund", world.captured(3).await?, refund_of(1), true),
        ("void", world.authorized(4).await?, String::new(), true),
        (
            "cancel",
            world.created("create-5").await?,
            String::new(),
            false,
        ),
    ])
}

/// What a command's effect counts: the requests that it recorded, or the steps that it
/// applied.
async fn effects(pool: &PgPool, payment: PaymentId, records_a_request: bool) -> sqlx::Result<i64> {
    if records_a_request {
        requests_of(pool, payment).await
    } else {
        steps(pool, payment).await
    }
}

#[sqlx::test]
async fn a_command_that_cannot_be_confirmed_is_a_503_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    // The backend dies just before the key's row, so nothing commits. The client is told to
    // send the request again, and that time it takes effect, once.
    let mut world = World::new(&pool).await?;
    let commands = one_of_each(&mut world).await?;
    install_crash_points(&pool).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let state = AppState::new(
        tenant.clone(),
        world.pools.system.clone(),
        world.pools.operator.digest(),
        unused_webhooks(),
        unused_handovers(),
    );

    for (verb, payment, body, records_a_request) in commands {
        let path = format!("/payments/{payment}/{verb}");
        let before = effects(&pool, payment, records_a_request).await?;
        plan_death(&tenant, "die_before idempotency_records").await?;
        let reply = send(
            router(state.clone()),
            post(&path, &world.secret, verb, body.clone()),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{verb}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::Unavailable.body(), "{verb}");
        assert_eq!(
            effects(&pool, payment, records_a_request).await?,
            before,
            "{verb}"
        );

        let reply = send(
            router(state.clone()),
            post(&path, &world.secret, verb, body),
        )
        .await;
        assert!(reply.status.is_success(), "{verb}: {reply:?}");
        assert_eq!(
            effects(&pool, payment, records_a_request).await?,
            before + 1,
            "{verb}"
        );
    }
    tenant.close().await;
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_statement_the_database_refuses_is_a_500_and_leaves_the_key_free(
    pool: PgPool,
) -> TestResult {
    // A rule that the types do not know about stands for a bug. Its answer says nothing of
    // the cause, and it is no final answer to store.
    let mut world = World::new(&pool).await?;
    let commands = one_of_each(&mut world).await?;
    let before = records(&pool).await?;
    sqlx::query!(
        "ALTER TABLE payment_transitions ADD CONSTRAINT refuses_all CHECK (seq < 0) NOT VALID"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "ALTER TABLE payment_requests ADD CONSTRAINT refuses_all CHECK (amount < 0) NOT VALID"
    )
    .execute(&pool)
    .await?;

    for (verb, payment, body, _) in &commands {
        let reply = world.command(*payment, verb, verb, body).await;
        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{verb}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::Internal.body(), "{verb}");
    }
    assert_eq!(records(&pool).await?, before);

    sqlx::query!("ALTER TABLE payment_transitions DROP CONSTRAINT refuses_all")
        .execute(&pool)
        .await?;
    sqlx::query!("ALTER TABLE payment_requests DROP CONSTRAINT refuses_all")
        .execute(&pool)
        .await?;
    for (verb, payment, body, _) in &commands {
        let reply = world.command(*payment, verb, verb, body).await;
        assert!(reply.status.is_success(), "{verb}: {reply:?}");
    }
    world.close().await;
    Ok(())
}
