// Author: Nicola Antonello

//! Reading a payment over HTTP, and the one extractor that every `/payments/{id}` route
//! takes its ID through (ADR-0023, ADR-0025, invariant I7).
//!
//! Isolation is enforced twice: by a comparison in Rust and by row-level security. So a
//! stranger's payment must read exactly like no payment at all, and each guard has to
//! hold without the other.

mod common;

use std::error::Error;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::api::{Pools, a_merchant, a_payment, bearer, get, get_with_authorization, send};
use common::payment_id;
use common::payments::{authorize_command, capture_command};
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, router};
use payments_core::credentials::ApiKeySecret;
use payments_core::payments::{authorize, capture, find_payment};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// `GET /payments/{id}`, with this secret.
fn read(id: impl std::fmt::Display, secret: &ApiKeySecret) -> Request<Body> {
    get_with_authorization(&format!("/payments/{id}"), &bearer(secret))
}

#[sqlx::test]
async fn a_merchant_reads_its_own_payment(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;

    let reply = send(router(pools.state()), read(payment, &secret)).await;

    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    assert_eq!(
        reply.body,
        format!(
            r#"{{"payment_id":"{payment}","psp":"mock","currency":"EUR","amount":10000,"fee_bps":250,"status":"created","refundable":0}}"#
        )
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_read_shows_where_a_payment_stands_now(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    let me = Tenant::new(&pools.tenant, merchant);
    authorize(me, &authorize_command(1, payment)).await?;
    capture(me, &capture_command(1, payment)).await?;

    let reply = send(router(pools.state()), read(payment, &secret)).await;

    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert!(reply.body.contains(r#""status":"captured""#), "{reply:?}");
    assert!(reply.body.contains(r#""refundable":8000"#), "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn another_merchants_payment_reads_exactly_like_no_payment(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (owner, _) = a_merchant(&pools, 1).await?;
    let (_, stranger) = a_merchant(&pools, 2).await?;
    let payment = a_payment(&pools, owner, 1).await?;

    let theirs = send(router(pools.state()), read(payment, &stranger)).await;
    let nobodys = send(router(pools.state()), read(payment_id(99), &stranger)).await;

    assert_eq!(theirs.status, StatusCode::NOT_FOUND, "{theirs:?}");
    assert_eq!(theirs.body, Problem::NotFound.body());
    assert_eq!(theirs.status, nobodys.status);
    assert_eq!(theirs.body, nobodys.body);
    assert_eq!(theirs.headers, nobodys.headers);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_comparison_in_rust_holds_with_no_row_level_security(pool: PgPool) -> TestResult {
    // Merchant routes given the system role's pool, whose BYPASSRLS stands for a policy
    // that is gone. The control shows that the row really is visible to that pool, so
    // only the comparison can refuse it.
    let pools = Pools::login(&pool).await?;
    let (owner, mine) = a_merchant(&pools, 1).await?;
    let (_, stranger) = a_merchant(&pools, 2).await?;
    let payment = a_payment(&pools, owner, 1).await?;
    let no_policy = AppState::new(
        pools.system.clone(),
        pools.system.clone(),
        pools.operator.digest(),
    );
    assert!(find_payment(&pools.system, payment).await?.is_some());

    let reply = send(router(no_policy.clone()), read(payment, &stranger)).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{reply:?}");

    let reply = send(router(no_policy), read(payment, &mine)).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_id_that_is_not_one_reads_exactly_like_no_payment(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    let canonical = payment.to_string();
    let hyphenless = canonical.replace('-', "");
    let upper = canonical.to_uppercase();
    let braced = format!("%7B{canonical}%7D");
    let urn = format!("urn:uuid:{canonical}");
    let encoded = canonical.replacen('0', "%30", 1);
    let not_seven = Uuid::parse_str("019b76da-a800-4000-8000-000000000001")?.to_string();
    let nil = Uuid::nil().to_string();

    // Each other spelling of a real ID has to be refused as well: one payment has one
    // path, or a retry that spells it differently would read as another request.
    for id in [
        "not-a-uuid",
        "1",
        "%FF",
        hyphenless.as_str(),
        upper.as_str(),
        braced.as_str(),
        urn.as_str(),
        encoded.as_str(),
        not_seven.as_str(),
        nil.as_str(),
    ] {
        let reply = send(router(pools.state()), read(id, &secret)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}: {reply:?}");
        assert_eq!(reply.body, Problem::NotFound.body(), "{id}");
    }
    let reply = send(router(pools.state()), read(canonical, &secret)).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_credential_comes_before_the_id(pool: PgPool) -> TestResult {
    // Someone with no key learns nothing, not even which IDs are well formed.
    let pools = Pools::login(&pool).await?;
    let (merchant, _) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    for path in [
        format!("/payments/{payment}"),
        format!("/payments/{}", payment_id(99)),
        "/payments/not-a-uuid".to_owned(),
    ] {
        let reply = send(router(pools.state()), get(&path)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(reply.body, Problem::Unauthorized.body(), "{path}");
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_wrong_method_is_a_problem_that_names_the_right_ones(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let request = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/payments/{}", payment_id(1)))
        .body(Body::empty())?;
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/problem+json");
    assert_eq!(reply.body, Problem::MethodNotAllowed.body());
    assert!(reply.header("allow").contains("GET"), "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_payment_that_cannot_be_read_is_a_503_and_never_a_404(pool: PgPool) -> TestResult {
    // The key looks up fine on the system role's pool, and the tenant role's is gone.
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    pools.tenant.close().await;
    let reply = send(router(pools.state()), read(payment, &secret)).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    assert_eq!(reply.body, Problem::Unavailable.body());
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_row_that_breaks_the_rules_panics_into_a_500_on_a_real_route(pool: PgPool) -> TestResult {
    // The owner goes past every guard, which the failure model leaves out, so the read
    // finds what the types rule out and panics. On the real router, that has to reach the
    // one middleware: the other tests of it build routers of their own.
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    sqlx::query!("ALTER TABLE payments DROP CONSTRAINT payments_psp_check")
        .execute(&pool)
        .await?;
    sqlx::query!("ALTER TABLE payments DISABLE TRIGGER USER")
        .execute(&pool)
        .await?;
    sqlx::query!(
        "UPDATE payments SET psp = 'nobody' WHERE id = $1",
        payment.get()
    )
    .execute(&pool)
    .await?;

    let reply = send(router(pools.state()), read(payment, &secret)).await;

    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR, "{reply:?}");
    assert_eq!(reply.body, Problem::Internal.body());
    pools.close().await;
    Ok(())
}
