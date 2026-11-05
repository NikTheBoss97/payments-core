// Author: Nicola Antonello

//! Requests under an idempotency key, over HTTP (ADR-0026, invariant I4).
//!
//! A key's record holds the final answer and commits with the effect. So a retry gets the
//! same bytes, a duplicate in flight waits and gets them too, and anything that was not
//! final leaves the key free. The library's tests attack the record itself: these attack
//! what a client sees of it.

mod common;

use std::error::Error;

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode};
use common::api::{
    Pools, a_merchant, bearer, get_with_authorization, payments_stored, post, records, send,
};
use common::crashes::{install_crash_points, plan_death};
use common::keys::issued_key;
use common::{merchant_id, tenant_pool_of_one};
use futures_util::future::join_all;
use futures_util::stream;
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, router};
use payments_core::credentials::ApiKeySecret;
use payments_core::ids::PaymentId;
use payments_core::money::BasisPoints;
use payments_core::payments::register_merchant;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const BODY: &str = r#"{"currency":"EUR","amount":10000}"#;

fn create(secret: &ApiKeySecret, key: &str, body: &str) -> Request<Body> {
    post("/payments", secret, key, body.to_owned())
}

/// The new payment's ID, from a 201's body.
fn created_id(body: &str) -> Result<PaymentId, Box<dyn Error>> {
    let body: serde_json::Value = serde_json::from_str(body)?;
    let id = body["payment_id"].as_str().ok_or("no payment_id")?;
    Ok(PaymentId::try_from(Uuid::parse_str(id)?)?)
}

#[sqlx::test]
async fn a_new_payment_is_a_201_with_its_id_and_the_reply(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;

    let reply = send(router(pools.state()), create(&secret, "k", BODY)).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    let payment = created_id(&reply.body)?;
    assert_eq!(
        reply.body,
        format!(
            r#"{{"payment_id":"{payment}","seq":0,"from":"created","to":"created","refundable":0}}"#
        )
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_new_payment_has_the_amount_asked_for_and_the_rate_the_operator_set(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&pools.system, merchant, BasisPoints::new(300)?).await?;
    let secret = issued_key(&pools.system, merchant, 1).await?;

    let reply = send(
        router(pools.state()),
        create(&secret, "k", r#"{"currency":"USD","amount":1234}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    let payment = created_id(&reply.body)?;

    let read = send(
        router(pools.state()),
        get_with_authorization(&format!("/payments/{payment}"), &bearer(&secret)),
    )
    .await;
    assert_eq!(
        read.body,
        format!(
            r#"{{"payment_id":"{payment}","psp":"mock","currency":"USD","amount":1234,"fee_bps":300,"status":"created","refundable":0}}"#
        )
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_the_same_answer_and_changes_nothing(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;

    let first = send(router(pools.state()), create(&secret, "k", BODY)).await;
    let second = send(router(pools.state()), create(&secret, "k", BODY)).await;

    assert_eq!(first.status, StatusCode::CREATED, "{first:?}");
    assert_eq!(second.status, first.status);
    assert_eq!(second.body, first.body);
    assert_eq!(second.headers, first.headers);
    assert_eq!(payments_stored(&pool).await?, 1);
    assert_eq!(records(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_reused_for_another_request_is_refused_and_the_first_answer_stands(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let first = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(first.status, StatusCode::CREATED, "{first:?}");

    // Another amount, and the same fields in another layout: a fingerprint of the raw
    // bytes can raise a false alarm, but it can never merge two requests.
    for other in [
        r#"{"currency":"EUR","amount":20000}"#,
        r#"{"currency":"EUR", "amount":10000}"#,
    ] {
        let reply = send(router(pools.state()), create(&secret, "k", other)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{other}");
        assert_eq!(reply.header("content-type"), "application/problem+json");
        assert_eq!(reply.body, Problem::KeyReused.body(), "{other}");
    }

    let again = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(again.body, first.body);
    assert_eq!(payments_stored(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn two_merchants_may_hold_one_key(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, mine) = a_merchant(&pools, 1).await?;
    let (_, theirs) = a_merchant(&pools, 2).await?;

    let first = send(router(pools.state()), create(&mine, "shared", BODY)).await;
    let second = send(router(pools.state()), create(&theirs, "shared", BODY)).await;

    assert_eq!(first.status, StatusCode::CREATED, "{first:?}");
    assert_eq!(second.status, StatusCode::CREATED, "{second:?}");
    assert_ne!(created_id(&first.body)?, created_id(&second.body)?);
    assert_eq!(payments_stored(&pool).await?, 2);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_request_racing_give_one_payment_and_one_answer(pool: PgPool) -> TestResult {
    // Each attempt makes its own IDs, and a create takes no lock, so only the key's
    // unique index stands between them.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let replies =
        join_all((0..8).map(|_| send(router(pools.state()), create(&secret, "race", BODY)))).await;

    for reply in &replies {
        assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
        assert_eq!(reply.body, replies[0].body);
    }
    assert_eq!(payments_stored(&pool).await?, 1);
    assert_eq!(records(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

/// A create with these `Idempotency-Key` values, none or several, which may not be text.
fn with_keys(secret: &ApiKeySecret, values: &[&[u8]]) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/payments")
        .header("authorization", bearer(secret));
    for value in values {
        request = request.header("idempotency-key", *value);
    }
    request.body(Body::from(BODY)).expect("a valid request")
}

#[sqlx::test]
async fn a_key_that_is_missing_or_malformed_is_refused_and_nothing_runs(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let too_long = format!("\"{}\"", "k".repeat(256));
    let cases: [(&str, Vec<&[u8]>); 10] = [
        ("none", vec![]),
        ("empty", vec![b""]),
        ("a bare token", vec![b"abc"]),
        ("quotes around nothing", vec![br#""""#]),
        ("a quote left open", vec![br#""abc"#]),
        ("a parameter", vec![br#""abc";p=1"#]),
        ("a space inside", vec![br#""a b""#]),
        ("too long", vec![too_long.as_bytes()]),
        ("not text", vec![b"\"\xff\xfe\""]),
        ("two of them", vec![br#""abc""#, br#""def""#]),
    ];
    for (what, values) in &cases {
        let reply = send(router(pools.state()), with_keys(&secret, values)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{what}: {reply:?}");
        assert_eq!(reply.body, Problem::MalformedKey.body(), "{what}");
    }
    assert_eq!(payments_stored(&pool).await?, 0);

    // The control: the same request with a good key goes through.
    let reply = send(router(pools.state()), with_keys(&secret, &[br#""abc""#])).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_credential_comes_before_the_key(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/payments")
        .body(Body::from(BODY))?;
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_body_that_cannot_be_used_is_refused_and_leaves_the_key_free(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let cases: [&[u8]; 22] = [
        b"",
        b"not JSON",
        b"[]",
        b"{}",
        b"null",
        br#"{"currency":"EUR"}"#,
        br#"{"amount":10000}"#,
        // A field that is misspelt or forbidden is never silently ignored.
        br#"{"currency":"EUR","amount":10000,"merchant_id":"anyone"}"#,
        br#"{"currency":"EUR","amount":10000,"fee_bps":0}"#,
        br#"{"currency":"EUR","amount":10000,"amout":1}"#,
        br#"{"currency":"EUR","amount":10000,"amount":20000}"#,
        // An amount is a whole number of minor units, in range.
        br#"{"currency":"EUR","amount":100.5}"#,
        br#"{"currency":"EUR","amount":100.0}"#,
        br#"{"currency":"EUR","amount":1e4}"#,
        br#"{"currency":"EUR","amount":"10000"}"#,
        br#"{"currency":"EUR","amount":0}"#,
        br#"{"currency":"EUR","amount":-1}"#,
        br#"{"currency":"EUR","amount":1000000000000001}"#,
        br#"{"currency":"EUR","amount":9223372036854775808}"#,
        // A currency is three upper-case letters. One that is not known is another answer.
        br#"{"currency":"eur","amount":10000}"#,
        br#"{"currency":"EURO","amount":10000}"#,
        // Whatever follows the document, and bytes that are not text.
        b"{\"currency\":\"EUR\",\"amount\":10000} \xff",
    ];
    for body in cases {
        let reply = send(
            router(pools.state()),
            post("/payments", &secret, "k", body.to_vec()),
        )
        .await;
        let shown = String::from_utf8_lossy(body);
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{shown}: {reply:?}");
        assert_eq!(reply.body, Problem::MalformedBody.body(), "{shown}");
    }
    assert_eq!(payments_stored(&pool).await?, 0);
    assert_eq!(records(&pool).await?, 0);

    // Nothing was decided, so the key is still free, even for what differs from all of
    // them. The cap itself is the control for the amount above it.
    let body = r#"{"currency":"EUR","amount":1000000000000000}"#;
    let reply = send(router(pools.state()), create(&secret, "k", body)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_currency_is_refused_and_leaves_the_key_free(pool: PgPool) -> TestResult {
    // Currencies change only by a migration, so this is no state that could change, and
    // nothing is stored (ADR-0023).
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let reply = send(
        router(pools.state()),
        create(&secret, "k", r#"{"currency":"XXZ","amount":10000}"#),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    assert_eq!(reply.body, Problem::UnknownCurrency.body());
    assert_eq!(payments_stored(&pool).await?, 0);
    assert_eq!(records(&pool).await?, 0);

    let reply = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_body_above_the_limit_is_a_problem(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let limit = 2 * 1024 * 1024;

    let reply = send(
        router(pools.state()),
        post("/payments", &secret, "k", vec![b'x'; limit + 1]),
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.status
    );
    assert_eq!(reply.header("content-type"), "application/problem+json");
    assert_eq!(reply.body, Problem::BodyTooLarge.body());

    // The control: at the limit it is read, and only its content is wrong.
    let reply = send(
        router(pools.state()),
        post("/payments", &secret, "k", vec![b'x'; limit]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.status);
    assert_eq!(reply.body, Problem::MalformedBody.body());
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_that_cannot_be_looked_up_is_a_503(pool: PgPool) -> TestResult {
    // The credential looks up fine on the system role's pool, and the tenant role's is
    // gone. Nothing may run when the key's state is not known.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    pools.tenant.close().await;
    let reply = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    assert_eq!(reply.body, Problem::Unavailable.body());
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn nothing_runs_when_a_key_cannot_be_looked_up(pool: PgPool) -> TestResult {
    // The rest of the database works, so a request that went on regardless would get as
    // far as storing its answer, and that would be another answer.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    sqlx::query!("ALTER TABLE idempotency_records RENAME TO idempotency_records_gone")
        .execute(&pool)
        .await?;
    let reply = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    assert_eq!(reply.body, Problem::Unavailable.body());
    assert_eq!(payments_stored(&pool).await?, 0);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_body_that_fails_on_its_way_in_is_refused(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    let failing = Body::from_stream(stream::iter([Err::<Bytes, _>(std::io::Error::other(
        "the client went away",
    ))]));
    let reply = send(
        router(pools.state()),
        post("/payments", &secret, "k", failing),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{reply:?}");
    assert_eq!(reply.body, Problem::MalformedBody.body());
    assert_eq!(payments_stored(&pool).await?, 0);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_command_that_cannot_be_confirmed_is_a_503_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    // The backend dies just before the key's row, so nothing commits. The client is told
    // to send the request again, which is safe, and that time it applies.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    install_crash_points(&pool).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let state = AppState::new(tenant.clone(), pools.system.clone());
    plan_death(&tenant, "die_before idempotency_records").await?;

    let reply = send(router(state.clone()), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    assert_eq!(reply.body, Problem::Unavailable.body());
    assert_eq!(payments_stored(&pool).await?, 0);
    assert_eq!(records(&pool).await?, 0);

    let reply = send(router(state), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(payments_stored(&pool).await?, 1);
    tenant.close().await;
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_statement_the_database_refuses_is_a_500_and_leaves_the_key_free(
    pool: PgPool,
) -> TestResult {
    // A rule that the types do not know about stands for a bug. Its answer says nothing
    // of the cause, and it is no final answer to store.
    let pools = Pools::login(&pool).await?;
    let (_, secret) = a_merchant(&pools, 1).await?;
    sqlx::query!("ALTER TABLE payments ADD CONSTRAINT refuses_all CHECK (amount < 0) NOT VALID")
        .execute(&pool)
        .await?;

    let reply = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR, "{reply:?}");
    assert_eq!(reply.body, Problem::Internal.body());
    assert_eq!(records(&pool).await?, 0);

    sqlx::query!("ALTER TABLE payments DROP CONSTRAINT refuses_all")
        .execute(&pool)
        .await?;
    let reply = send(router(pools.state()), create(&secret, "k", BODY)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}
