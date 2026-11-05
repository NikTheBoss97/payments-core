// Author: Nicola Antonello

//! A merchant's commands on a payment, over HTTP (ADR-0023, ADR-0026), with the status and
//! the storage of each answer. The moves themselves are tested in the library: these tests
//! attack what the routes add, which is parsing, the mapping to an answer, and the key.

mod common;

use std::error::Error;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::api::{Pools, Reply, a_merchant, bearer, get_with_authorization, post, records, send};
use common::crashes::balances;
use common::payment_id;
use common::payments::{AUTHORIZED, FEE, steps, unbalanced_currencies, uncaused_postings};
use futures_util::future::join_all;
use payments_core::api::problem::Problem;
use payments_core::api::router;
use payments_core::credentials::ApiKeySecret;
use payments_core::ids::PaymentId;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const CREATE: &str = r#"{"currency":"EUR","amount":10000}"#;
const AUTHORIZE: &str = r#"{"amount":8000,"expires_at":"2027-01-01T00:00:00Z"}"#;
const CAPTURE: &str = r#"{"amount":8000,"captured_at":"2026-12-01T00:00:00Z"}"#;

fn refund_of(minor: i64) -> String {
    format!(r#"{{"amount":{minor},"refunded_at":"2026-12-02T00:00:00Z"}}"#)
}

/// `POST /payments/{id}/{verb}` under this key.
async fn step(
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

/// A new payment, through its route.
async fn created(
    pools: &Pools,
    secret: &ApiKeySecret,
    key: &str,
) -> Result<PaymentId, Box<dyn Error>> {
    let reply = send(
        router(pools.state()),
        post("/payments", secret, key, CREATE),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    let body: serde_json::Value = serde_json::from_str(&reply.body)?;
    let id = body["payment_id"].as_str().ok_or("no payment_id")?;
    Ok(PaymentId::try_from(Uuid::parse_str(id)?)?)
}

/// A payment authorized for 80.00, through its routes.
async fn authorized(
    pools: &Pools,
    secret: &ApiKeySecret,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(pools, secret, &format!("create-{n}")).await?;
    let reply = step(
        pools,
        secret,
        payment,
        "authorize",
        &format!("authorize-{n}"),
        AUTHORIZE,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    Ok(payment)
}

/// A payment captured for 80.00, through its routes.
async fn captured(
    pools: &Pools,
    secret: &ApiKeySecret,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = authorized(pools, secret, n).await?;
    let reply = step(
        pools,
        secret,
        payment,
        "capture",
        &format!("capture-{n}"),
        CAPTURE,
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    Ok(payment)
}

/// The payment as `GET /payments/{id}` shows it.
async fn read(
    pools: &Pools,
    secret: &ApiKeySecret,
    payment: PaymentId,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let reply = send(
        router(pools.state()),
        get_with_authorization(&format!("/payments/{payment}"), &bearer(secret)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    Ok(serde_json::from_str(&reply.body)?)
}

#[track_caller]
fn assert_reply(reply: &Reply, seq: i32, from: &str, to: &str, refundable: i64) {
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    assert_eq!(
        reply.body,
        format!(r#"{{"seq":{seq},"from":"{from}","to":"{to}","refundable":{refundable}}}"#)
    );
}

#[sqlx::test]
async fn a_payment_goes_through_its_life_and_the_money_follows(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;

    let reply = step(&pools, &secret, payment, "authorize", "a", AUTHORIZE).await;
    assert_reply(&reply, 1, "created", "authorized", 0);
    assert_eq!(balances(&pools.system, merchant).await?, [0, 0, 0]);

    let reply = step(&pools, &secret, payment, "capture", "c", CAPTURE).await;
    assert_reply(&reply, 2, "authorized", "captured", AUTHORIZED);
    assert_eq!(
        balances(&pools.system, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );

    let reply = step(&pools, &secret, payment, "refund", "r1", &refund_of(3_000)).await;
    assert_reply(&reply, 3, "captured", "captured", 5_000);
    assert_eq!(
        balances(&pools.system, merchant).await?,
        [5_000, FEE - 5_000, -FEE]
    );

    let reply = step(&pools, &secret, payment, "refund", "r2", &refund_of(5_000)).await;
    assert_reply(&reply, 4, "captured", "refunded", 0);
    assert_eq!(balances(&pools.system, merchant).await?, [0, FEE, -FEE]);

    assert_eq!(read(&pools, &secret, payment).await?["status"], "refunded");
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_replays_its_answer_and_moves_no_money(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = captured(&pools, &secret, 1).await?;

    let first = step(&pools, &secret, payment, "refund", "r", &refund_of(1_000)).await;
    let second = step(&pools, &secret, payment, "refund", "r", &refund_of(1_000)).await;

    assert_reply(&first, 3, "captured", "captured", 7_000);
    assert_eq!(second.status, first.status);
    assert_eq!(second.body, first.body);
    assert_eq!(second.headers, first.headers);
    assert_eq!(steps(&pool, payment).await?, 4);
    assert_eq!(
        balances(&pools.system, merchant).await?,
        [7_000, FEE - 7_000, -FEE]
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_refund_racing_apply_once_and_give_one_answer(pool: PgPool) -> TestResult {
    // The sharpest case: a partial refund still fits after it applies, so without the key
    // each copy, with its fresh IDs, would move the money again.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = captured(&pools, &secret, 1).await?;
    let body = refund_of(1);

    let replies =
        join_all((0..8).map(|_| step(&pools, &secret, payment, "refund", "r", &body))).await;

    for reply in &replies {
        assert_reply(reply, 3, "captured", "captured", AUTHORIZED - 1);
    }
    assert_eq!(steps(&pool, payment).await?, 4);
    assert_eq!(
        read(&pools, &secret, payment).await?["refundable"],
        AUTHORIZED - 1
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_new_key_is_a_new_request(pool: PgPool) -> TestResult {
    // The control for the two tests above: the same refund under another key does apply,
    // so it is the key that stopped those copies.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = captured(&pools, &secret, 1).await?;
    let body = refund_of(1);
    step(&pools, &secret, payment, "refund", "first", &body).await;
    let reply = step(&pools, &secret, payment, "refund", "second", &body).await;
    assert_reply(&reply, 4, "captured", "captured", AUTHORIZED - 2);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_illegal_move_is_a_409_that_is_stored(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;
    let before = records(&pool).await?;

    for (verb, body) in [("capture", CAPTURE.to_owned()), ("refund", refund_of(1))] {
        let reply = step(&pools, &secret, payment, verb, verb, &body).await;
        assert_eq!(reply.status, StatusCode::CONFLICT, "{verb}: {reply:?}");
        assert_eq!(reply.header("content-type"), "application/problem+json");
        assert_eq!(reply.body, Problem::IllegalMove.body(), "{verb}");
    }
    assert_eq!(records(&pool).await?, before + 2);
    assert_eq!(steps(&pool, payment).await?, 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_amount_above_its_limit_is_a_422_that_is_stored(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;

    // Above the requested 100.00, the authorized 80.00, and the 80.00 still refundable.
    let payment = created(&pools, &secret, "create").await?;
    let above = r#"{"amount":10001,"expires_at":"2027-01-01T00:00:00Z"}"#;
    let reply = step(&pools, &secret, payment, "authorize", "a", above).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::AmountAboveLimit.body());
    assert_eq!(reply.header("content-type"), "application/problem+json");

    let payment = authorized(&pools, &secret, 2).await?;
    let above = r#"{"amount":8001,"captured_at":"2026-12-01T00:00:00Z"}"#;
    let reply = step(&pools, &secret, payment, "capture", "c", above).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::AmountAboveLimit.body());

    let payment = captured(&pools, &secret, 3).await?;
    let before = records(&pool).await?;
    let reply = step(&pools, &secret, payment, "refund", "r", &refund_of(8_001)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::AmountAboveLimit.body());
    assert_eq!(records(&pool).await?, before + 1);
    assert_eq!(steps(&pool, payment).await?, 3);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_late_duplicate_of_a_refusal_stays_a_refusal(pool: PgPool) -> TestResult {
    // A client was told no. The payment then moves on, so a copy of the same request,
    // delivered late, would apply if it were decided again (ADR-0026).
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;

    let refused = step(&pools, &secret, payment, "capture", "late", CAPTURE).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{refused:?}");

    let reply = step(&pools, &secret, payment, "authorize", "a", AUTHORIZE).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");

    let late = step(&pools, &secret, payment, "capture", "late", CAPTURE).await;
    assert_eq!(late.status, refused.status);
    assert_eq!(late.body, refused.body);
    assert_eq!(
        read(&pools, &secret, payment).await?["status"],
        "authorized"
    );

    // The control: the same capture under a new key applies, so it was the key that held.
    let reply = step(&pools, &secret, payment, "capture", "fresh", CAPTURE).await;
    assert_reply(&reply, 2, "authorized", "captured", AUTHORIZED);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_is_for_one_route_and_one_payment(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;
    let another = created(&pools, &secret, "create-another").await?;
    let first = step(&pools, &secret, payment, "authorize", "k", AUTHORIZE).await;
    assert_eq!(first.status, StatusCode::OK, "{first:?}");

    for (what, target, verb, body) in [
        ("another route", payment, "capture", AUTHORIZE),
        ("another payment", another, "authorize", AUTHORIZE),
        (
            "another body",
            payment,
            "authorize",
            r#"{"amount":7000,"expires_at":"2027-01-01T00:00:00Z"}"#,
        ),
    ] {
        let reply = step(&pools, &secret, target, verb, "k", body).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{what}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::KeyReused.body(), "{what}");
    }
    assert_eq!(read(&pools, &secret, another).await?["status"], "created");
    pools.close().await;
    Ok(())
}

/// The commands on a payment, each with a body that would apply if nothing else stopped it.
fn every_command() -> Vec<(&'static str, String)> {
    vec![
        ("authorize", AUTHORIZE.to_owned()),
        ("capture", CAPTURE.to_owned()),
        ("refund", refund_of(1)),
    ]
}

#[sqlx::test]
async fn another_merchants_payment_cannot_be_touched(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, owner) = a_merchant(&pools, 1).await?;
    let (_, stranger) = a_merchant(&pools, 2).await?;
    let payment = created(&pools, &owner, "create").await?;
    let before = records(&pool).await?;

    for (verb, body) in every_command() {
        let theirs = step(&pools, &stranger, payment, verb, verb, &body).await;
        let nobodys = step(&pools, &stranger, payment_id(99), verb, verb, &body).await;
        assert_eq!(theirs.status, StatusCode::NOT_FOUND, "{verb}: {theirs:?}");
        assert_eq!(theirs.body, Problem::NotFound.body(), "{verb}");
        assert_eq!(theirs.status, nobodys.status, "{verb}");
        assert_eq!(theirs.body, nobodys.body, "{verb}");
        assert_eq!(theirs.headers, nobodys.headers, "{verb}");
    }
    assert_eq!(records(&pool).await?, before);
    assert_eq!(steps(&pool, payment).await?, 1);
    pools.close().await;
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
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;
    let before = records(&pool).await?;

    let mut routes = vec![("/payments".to_owned(), CREATE.to_owned())];
    for (verb, body) in every_command() {
        routes.push((format!("/payments/{payment}/{verb}"), body));
    }
    for (path, body) in routes {
        let reply = send(router(pools.state()), without_a_key(&path, &secret, body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}: {reply:?}");
        assert_eq!(reply.body, Problem::MalformedKey.body(), "{path}");
    }
    assert_eq!(records(&pool).await?, before);
    assert_eq!(steps(&pool, payment).await?, 1);
    pools.close().await;
    Ok(())
}

/// Bodies that a command with a time in this field must refuse.
fn unusable_bodies(time: &str) -> Vec<String> {
    let at = "2027-01-01T00:00:00Z";
    vec![
        String::new(),
        "not JSON".to_owned(),
        "{}".to_owned(),
        "null".to_owned(),
        // A field missing, unknown or misspelt.
        r#"{"amount":1}"#.to_owned(),
        format!(r#"{{"{time}":"{at}"}}"#),
        format!(r#"{{"amount":1,"{time}":"{at}","merchant_id":"anyone"}}"#),
        format!(r#"{{"amount":1,"happened_at":"{at}"}}"#),
        // An amount that is no whole, positive number of minor units in range.
        format!(r#"{{"amount":0,"{time}":"{at}"}}"#),
        format!(r#"{{"amount":-1,"{time}":"{at}"}}"#),
        format!(r#"{{"amount":1.5,"{time}":"{at}"}}"#),
        format!(r#"{{"amount":1.0,"{time}":"{at}"}}"#),
        format!(r#"{{"amount":1e3,"{time}":"{at}"}}"#),
        format!(r#"{{"amount":"1","{time}":"{at}"}}"#),
        format!(r#"{{"amount":1000000000000001,"{time}":"{at}"}}"#),
        // A time that is no RFC 3339 string, or that gives no offset.
        format!(r#"{{"amount":1,"{time}":"soon"}}"#),
        format!(r#"{{"amount":1,"{time}":"2027-13-01T00:00:00Z"}}"#),
        format!(r#"{{"amount":1,"{time}":"2027-01-01T00:00:00"}}"#),
        format!(r#"{{"amount":1,"{time}":"2027-01-01"}}"#),
        format!(r#"{{"amount":1,"{time}":1767225600}}"#),
        format!(r#"{{"amount":1,"{time}":null}}"#),
    ]
}

#[sqlx::test]
async fn a_body_that_cannot_be_used_is_refused_and_leaves_the_key_free(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;

    // Each payment is where its command would apply, so only the body can refuse it.
    let created_one = created(&pools, &secret, "create").await?;
    let authorized_one = authorized(&pools, &secret, 2).await?;
    let captured_one = captured(&pools, &secret, 3).await?;
    let mut stored = records(&pool).await?;

    for (verb, payment, time, good) in [
        ("authorize", created_one, "expires_at", AUTHORIZE.to_owned()),
        ("capture", authorized_one, "captured_at", CAPTURE.to_owned()),
        ("refund", captured_one, "refunded_at", refund_of(1)),
    ] {
        let key = format!("key-{verb}");
        for body in unusable_bodies(time) {
            let reply = step(&pools, &secret, payment, verb, &key, &body).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{verb} {body}: {reply:?}"
            );
            assert_eq!(reply.body, Problem::MalformedBody.body(), "{verb} {body}");
        }
        assert_eq!(records(&pool).await?, stored, "{verb}");

        // Had any of them been stored, this would be a reuse of the key.
        let reply = step(&pools, &secret, payment, verb, &key, &good).await;
        assert_eq!(reply.status, StatusCode::OK, "{verb}: {reply:?}");
        stored += 1;
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_time_is_stored_as_it_will_read_back(pool: PgPool) -> TestResult {
    // Whatever the offset, and however fine, it becomes whole microseconds in UTC, and a
    // leap second folds into the next second (ADR-0022).
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;
    let leap = r#"{"amount":8000,"expires_at":"2026-12-31T23:59:60Z"}"#;
    let reply = step(&pools, &secret, payment, "authorize", "a", leap).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    let offset = r#"{"amount":8000,"captured_at":"2026-12-01T01:00:00.1234567+01:00"}"#;
    let reply = step(&pools, &secret, payment, "capture", "c", offset).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");

    let expires_at = sqlx::query_scalar!(
        "SELECT expires_at FROM payment_authorizations WHERE payment_id = $1",
        payment.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(expires_at.to_rfc3339(), "2027-01-01T00:00:00+00:00");
    let captured_at = sqlx::query_scalar!(
        "SELECT captured_at FROM payment_captures WHERE payment_id = $1",
        payment.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(captured_at.to_rfc3339(), "2026-12-01T00:00:00.123456+00:00");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_command_route_takes_only_post(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = created(&pools, &secret, "create").await?;
    for (verb, _) in every_command() {
        let reply = send(
            router(pools.state()),
            get_with_authorization(&format!("/payments/{payment}/{verb}"), &bearer(&secret)),
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
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_verb_that_is_no_command_is_a_404(pool: PgPool) -> TestResult {
    // decline, expire and settle report what the PSP decided, so they stay off HTTP until
    // phase 4's webhooks (ADR-0023). A merchant could otherwise settle its own payment.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let payment = authorized(&pools, &secret, 1).await?;
    for verb in ["decline", "expire", "settle", "delete"] {
        let reply = step(&pools, &secret, payment, verb, verb, "").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{verb}: {reply:?}");
        assert_eq!(reply.body, Problem::NotFound.body(), "{verb}");
    }
    assert_eq!(
        read(&pools, &secret, payment).await?["status"],
        "authorized"
    );
    pools.close().await;
    Ok(())
}
