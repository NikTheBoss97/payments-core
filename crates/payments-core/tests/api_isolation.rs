// Author: Nicola Antonello

//! Each guard of a merchant's isolation broken alone, over HTTP (ADR-0025, invariant I7).
//!
//! Isolation is enforced twice: a comparison in Rust on every route that takes a payment's ID,
//! and row-level security under the login role that merchant routes hold. A test that attacks
//! both proves little about either, so these turn one off and attack through the other.

mod common;

use std::error::Error;

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::routing::post as post_route;
use common::api::{
    Pools, Reply, a_merchant, a_payment, bearer, get_with_authorization, post, records, send,
};
use common::crashes::balances;
use common::merchant_id;
use common::payments::{authorize_command, capture_command, refund_command, steps};
use payments_core::api::problem::Problem;
use payments_core::api::{Merchant, router};
use payments_core::credentials::ApiKeySecret;
use payments_core::ids::{MerchantId, PaymentId, TransitionId};
use payments_core::money::BasisPoints;
use payments_core::payments::{
    AuthorizeError, CaptureError, RefundError, StepCommand, StepError, authorize, cancel, capture,
    find_payment, merchant_fee_rate, refund, register_merchant, void,
};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const CREATE: &str = r#"{"currency":"EUR","amount":10000}"#;
const AUTHORIZE: &str = r#"{"amount":8000,"expires_at":"2027-01-01T00:00:00Z"}"#;
const CAPTURE: &str = r#"{"amount":8000,"captured_at":"2026-12-01T00:00:00Z"}"#;
const REFUND: &str = r#"{"amount":1,"refunded_at":"2026-12-02T00:00:00Z"}"#;

/// Turns off row-level security on every table that has it, as the owner, and returns how many
/// it was. The tenant role keeps its grants, so only the comparison in Rust is left.
async fn turn_off_row_level_security(pool: &PgPool) -> sqlx::Result<i64> {
    let tables = sqlx::query_scalar!(
        r#"SELECT count(*) AS "tables!" FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = 'public' AND c.relrowsecurity"#
    )
    .fetch_one(pool)
    .await?;
    sqlx::query!(
        "DO $$ \
         DECLARE target text; \
         BEGIN \
             FOR target IN SELECT c.relname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = 'public' AND c.relrowsecurity LOOP \
                 EXECUTE format('ALTER TABLE %I DISABLE ROW LEVEL SECURITY', target); \
             END LOOP; \
         END $$"
    )
    .execute(pool)
    .await?;
    Ok(tables)
}

/// The route's request for a payment, with this merchant's key.
fn command_on(payment: PaymentId, verb: &str, body: &str, secret: &ApiKeySecret) -> Request<Body> {
    let path = format!("/payments/{payment}/{verb}");
    post(&path, secret, &format!("{verb}-{payment}"), body.to_owned())
}

/// Every route that takes a payment's ID, each with a request that would apply.
fn every_route(payment: PaymentId, secret: &ApiKeySecret) -> Vec<(&'static str, Request<Body>)> {
    vec![
        (
            "read",
            get_with_authorization(&format!("/payments/{payment}"), &bearer(secret)),
        ),
        (
            "authorize",
            command_on(payment, "authorize", AUTHORIZE, secret),
        ),
        ("capture", command_on(payment, "capture", CAPTURE, secret)),
        ("refund", command_on(payment, "refund", REFUND, secret)),
        ("void", command_on(payment, "void", "", secret)),
        ("cancel", command_on(payment, "cancel", "", secret)),
    ]
}

#[sqlx::test]
async fn without_row_level_security_the_comparison_alone_refuses_a_stranger_on_every_route(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (owner, mine) = a_merchant(&pools, 1).await?;
    let (stranger, theirs) = a_merchant(&pools, 2).await?;
    let payment = a_payment(&pools, owner, 1).await?;
    assert!(turn_off_row_level_security(&pool).await? > 0);

    // The control: the guard is off. The library's read has no comparison of its own, so it now
    // shows the stranger the payment, under the very role that merchant routes hold.
    let them = Tenant::new(&pools.tenant, stranger);
    assert!(find_payment(them, payment).await?.is_some());

    let before = records(&pool).await?;
    for (route, request) in every_route(payment, &theirs) {
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{route}: {reply:?}");
        assert_eq!(reply.body, Problem::NotFound.body(), "{route}");
    }
    assert_eq!(steps(&pool, payment).await?, 1);
    assert_eq!(records(&pool).await?, before);

    // The control: the owner gets through the same routes.
    let request = get_with_authorization(&format!("/payments/{payment}"), &bearer(&mine));
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    let reply = send(
        router(pools.state()),
        command_on(payment, "authorize", AUTHORIZE, &mine),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn without_row_level_security_two_merchants_still_keep_their_own_keys_and_rates(
    pool: PgPool,
) -> TestResult {
    // Every query names its merchant as well as the policy doing so (ADR-0025). So one key
    // and one body give two payments, each at its own merchant's rate.
    let pools = Pools::login(&pool).await?;
    let (first, second) = (merchant_id(1), merchant_id(2));
    register_merchant(&pools.system, first, BasisPoints::new(250)?).await?;
    register_merchant(&pools.system, second, BasisPoints::new(300)?).await?;
    let mine = common::keys::issued_key(&pools.system, first, 1).await?;
    let theirs = common::keys::issued_key(&pools.system, second, 2).await?;
    assert!(turn_off_row_level_security(&pool).await? > 0);

    let mut payments = Vec::new();
    for (secret, rate) in [(&mine, 250), (&theirs, 300)] {
        let request = post("/payments", secret, "shared", CREATE);
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
        let body: serde_json::Value = serde_json::from_str(&reply.body)?;
        let payment = body["payment_id"].as_str().expect("an ID").to_owned();
        let request = get_with_authorization(&format!("/payments/{payment}"), &bearer(secret));
        let read = send(router(pools.state()), request).await;
        assert!(
            read.body.contains(&format!(r#""fee_bps":{rate}"#)),
            "{read:?}"
        );
        payments.push(payment);
    }
    assert_ne!(payments[0], payments[1]);
    assert_eq!(records(&pool).await?, 2);

    let me = Tenant::new(&pools.tenant, first);
    assert_eq!(merchant_fee_rate(me).await?, Some(BasisPoints::new(250)?));
    pools.close().await;
    Ok(())
}

/// What a route that forgets the comparison came to.
fn outcome<T, E>(result: Result<T, E>, not_found: impl Fn(&E) -> bool) -> &'static str {
    match result {
        Ok(_) => "applied",
        Err(err) if not_found(&err) => "nothing",
        Err(_) => "failed",
    }
}

/// A route that has forgotten the comparison: it takes its merchant from the credential, as it
/// must, and the payment's ID straight from the path, and runs the library's command. Only
/// row-level security is left between a stranger and the payment.
async fn forgetful(merchant: Merchant, Path((verb, id)): Path<(String, String)>) -> String {
    let id = Uuid::parse_str(&id).expect("a UUID");
    let payment = PaymentId::try_from(id).expect("a payment's ID");
    let me = merchant.tenant();
    let step = StepCommand {
        step: TransitionId::now_v7(),
        payment,
    };
    match verb.as_str() {
        "read" => match find_payment(me, payment).await {
            Ok(Some(_)) => "applied",
            Ok(None) => "nothing",
            Err(_) => "failed",
        },
        "authorize" => outcome(authorize(me, &authorize_command(7, payment)).await, |err| {
            matches!(err, AuthorizeError::NotFound { .. })
        }),
        "capture" => outcome(capture(me, &capture_command(7, payment)).await, |err| {
            matches!(err, CaptureError::NotFound { .. })
        }),
        "refund" => outcome(refund(me, &refund_command(7, payment, 1)).await, |err| {
            matches!(err, RefundError::NotFound { .. })
        }),
        "void" => outcome(void(me, &step).await, |err| {
            matches!(err, StepError::NotFound { .. })
        }),
        "cancel" => outcome(cancel(me, &step).await, |err| {
            matches!(err, StepError::NotFound { .. })
        }),
        other => panic!("no such verb: {other}"),
    }
    .to_owned()
}

/// `POST /forgetful/{verb}/{id}` with this merchant's key.
async fn through_forgetful(
    pools: &Pools,
    secret: &ApiKeySecret,
    verb: &str,
    payment: PaymentId,
) -> Reply {
    let route = Router::new()
        .route("/forgetful/{verb}/{id}", post_route(forgetful))
        .with_state(pools.state());
    let request = post(
        &format!("/forgetful/{verb}/{payment}"),
        secret,
        "unused",
        "",
    );
    send(route, request).await
}

/// A payment of this merchant at the status where each verb would apply.
async fn ready_for(
    pools: &Pools,
    merchant: MerchantId,
    verb: &str,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let me = Tenant::new(&pools.tenant, merchant);
    let payment = a_payment(pools, merchant, n).await?;
    if matches!(verb, "capture" | "refund" | "void") {
        authorize(me, &authorize_command(n, payment)).await?;
    }
    if verb == "refund" {
        capture(me, &capture_command(n, payment)).await?;
    }
    Ok(payment)
}

#[sqlx::test]
async fn with_the_comparison_forgotten_row_level_security_alone_refuses_a_stranger(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (owner, mine) = a_merchant(&pools, 1).await?;
    let (_, theirs) = a_merchant(&pools, 2).await?;

    for (n, verb) in (1_u32..).zip(["read", "authorize", "capture", "refund", "void", "cancel"]) {
        let payment = ready_for(&pools, owner, verb, n).await?;
        let before = steps(&pool, payment).await?;
        let money = balances(&pool, owner).await?;

        let reply = through_forgetful(&pools, &theirs, verb, payment).await;
        assert_eq!(reply.status, StatusCode::OK, "{verb}: {reply:?}");
        assert_eq!(reply.body, "nothing", "{verb}");
        assert_eq!(steps(&pool, payment).await?, before, "{verb}");
        assert_eq!(balances(&pool, owner).await?, money, "{verb}");

        // The control: the route does reach a payment, for the merchant that owns it.
        let reply = through_forgetful(&pools, &mine, verb, payment).await;
        assert_eq!(reply.body, "applied", "{verb}");
    }
    pools.close().await;
    Ok(())
}
