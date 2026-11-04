// Author: Nicola Antonello

//! Who a request comes from (ADR-0024). The merchant comes from the credential, and every
//! failure looks the same, so that a client learns nothing about a key it does not hold.

mod common;

use std::error::Error;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get as get_route;
use common::api::{Pools, bearer, get, get_with_authorization, send};
use common::keys::{issued_key, key, print};
use common::payments::{create_command, fee_rate};
use common::{api_key_id, merchant_id};
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, Merchant};
use payments_core::credentials::ApiKeySecret;
use payments_core::idempotency::{Answer, OperatorSeal};
use payments_core::money::BasisPoints;
use payments_core::operator::revoke_api_key;
use payments_core::payments::{create_payment, find_payment, merchant_fee_rate, register_merchant};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

async fn who(merchant: Merchant) -> Result<String, Problem> {
    let rate = merchant_fee_rate(merchant.tenant())
        .await
        .map_err(|_| Problem::Unavailable)?;
    Ok(format!(
        "{} {}",
        merchant.id(),
        rate.map_or(0, BasisPoints::get)
    ))
}

/// A route that takes a merchant, and shows which one it got and what the merchant's way
/// to the database can see.
fn who_route(state: AppState) -> Router {
    Router::new()
        .route("/who", get_route(who))
        .with_state(state)
}

#[sqlx::test]
async fn a_good_key_names_its_merchant(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (a, b) = (merchant_id(1), merchant_id(2));
    register_merchant(&pools.system, a, BasisPoints::new(250)?).await?;
    register_merchant(&pools.system, b, BasisPoints::new(300)?).await?;
    let mine = issued_key(&pools.system, a, 1).await?;
    let theirs = issued_key(&pools.system, b, 2).await?;

    for (secret, merchant, rate) in [(&mine, a, 250), (&theirs, b, 300)] {
        let value = format!("Bearer {}", secret.expose());
        let reply = send(
            who_route(pools.state()),
            get_with_authorization("/who", &value),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        assert_eq!(reply.body, format!("{merchant} {rate}"));
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_scheme_is_case_insensitive_and_may_have_several_spaces_after_it(
    pool: PgPool,
) -> TestResult {
    // RFC 9110 section 11.1 and RFC 6750 section 2.1.
    let pools = Pools::login(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&pools.system, merchant, fee_rate()).await?;
    let secret = issued_key(&pools.system, merchant, 1).await?;

    for prefix in ["Bearer ", "bearer ", "BEARER ", "bEaReR ", "Bearer   "] {
        let value = format!("{prefix}{}", secret.expose());
        let reply = send(
            who_route(pools.state()),
            get_with_authorization("/who", &value),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{prefix:?}: {reply:?}");
    }
    pools.close().await;
    Ok(())
}

/// A request with these `Authorization` values, none or several, which may not be text.
fn with_authorizations(values: &[&[u8]]) -> Request<Body> {
    let mut request = Request::builder().uri("/who");
    for value in values {
        request = request.header("authorization", *value);
    }
    request.body(Body::empty()).expect("a valid request")
}

#[sqlx::test]
async fn every_bad_credential_gets_the_same_answer(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&pools.system, merchant, fee_rate()).await?;
    let good = issued_key(&pools.system, merchant, 1).await?;
    let another = issued_key(&pools.system, merchant, 2).await?;
    let revoked = issued_key(&pools.system, merchant, 3).await?;
    let k = key("revoke");
    let answer = Answer {
        status: 200,
        body: "{}".to_owned(),
    };
    let seal = OperatorSeal {
        key: &k,
        fingerprint: print(100),
        answer: &answer,
    };
    revoke_api_key(&pools.system, &seal, api_key_id(3)).await?;

    let good_value = format!("Bearer {}", good.expose());
    let digits_only = format!("Bearer {}", &good.expose()[4..]);
    let as_basic = format!("Basic {}", good.expose());
    let no_scheme = good.expose().to_owned();
    let shorter = format!("Bearer {}", &good.expose()[..67]);
    let upper = format!("Bearer {}", good.expose().to_uppercase());
    let unknown = format!("Bearer {}", ApiKeySecret::generate().expose());
    let revoked_value = format!("Bearer {}", revoked.expose());
    let digest_value: String = good
        .digest()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let digest_value = format!("Bearer {digest_value}");
    let another_value = format!("Bearer {}", another.expose());
    let not_text: &[u8] = b"Bearer \xff\xfe";
    let cases: [(&str, Vec<&[u8]>); 12] = [
        ("none", vec![]),
        ("empty", vec![b""]),
        ("the scheme alone", vec![b"Bearer"]),
        ("the scheme and a space", vec![b"Bearer "]),
        ("another scheme", vec![as_basic.as_bytes()]),
        ("no scheme", vec![no_scheme.as_bytes()]),
        ("no prefix", vec![digits_only.as_bytes()]),
        ("cut short", vec![shorter.as_bytes()]),
        ("upper case", vec![upper.as_bytes()]),
        ("never issued", vec![unknown.as_bytes()]),
        ("revoked", vec![revoked_value.as_bytes()]),
        ("what the table holds", vec![digest_value.as_bytes()]),
    ];
    for (what, values) in &cases {
        let reply = send(who_route(pools.state()), with_authorizations(values)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{what}");
        assert_eq!(reply.header("www-authenticate"), "Bearer", "{what}");
        assert_eq!(
            reply.header("content-type"),
            "application/problem+json",
            "{what}"
        );
        assert_eq!(reply.body, Problem::Unauthorized.body(), "{what}");
    }
    // Which of two is meant cannot be known, even if each is good on its own.
    let two_good = [good_value.as_bytes(), another_value.as_bytes()];
    let reply = send(who_route(pools.state()), with_authorizations(&two_good)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let reply = send(who_route(pools.state()), with_authorizations(&[not_text])).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

    // The control: the key that these cases damaged is good as it stands.
    let reply = send(
        who_route(pools.state()),
        with_authorizations(&[good_value.as_bytes()]),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchants_way_to_the_database_is_under_row_level_security(pool: PgPool) -> TestResult {
    // The library's read has no comparison of its own, so on any pool but the tenant
    // role's it would show this stranger the payment (ADR-0025).
    let pools = Pools::login(&pool).await?;
    let (owner, stranger) = (merchant_id(1), merchant_id(2));
    register_merchant(&pools.system, owner, fee_rate()).await?;
    register_merchant(&pools.system, stranger, fee_rate()).await?;
    let mine = issued_key(&pools.system, owner, 1).await?;
    let theirs = issued_key(&pools.system, stranger, 2).await?;
    let command = create_command(1, owner);
    create_payment(Tenant::new(&pools.tenant, owner), &command).await?;
    let payment = command.id;
    let sees = move |merchant: Merchant| async move {
        match find_payment(merchant.tenant(), payment).await {
            Ok(Some(_)) => "found",
            Ok(None) => "nothing",
            Err(_) => "failed",
        }
    };

    for (secret, expected) in [(&mine, "found"), (&theirs, "nothing")] {
        let route = Router::new()
            .route("/sees", get_route(sees))
            .with_state(pools.state());
        let reply = send(route, get_with_authorization("/sees", &bearer(secret))).await;
        assert_eq!(reply.body, expected);
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_that_cannot_be_looked_up_is_a_503_and_never_a_401(pool: PgPool) -> TestResult {
    // A client must never conclude that a good key is bad (ADR-0024).
    let pools = Pools::login(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&pools.system, merchant, fee_rate()).await?;
    let secret = issued_key(&pools.system, merchant, 1).await?;
    let value = format!("Bearer {}", secret.expose());

    // A role with no privilege on the table, and a pool that has gone.
    let broken_grant = AppState::new(pools.tenant.clone(), pools.tenant.clone());
    let gone = AppState::new(pools.tenant.clone(), {
        let closed = pools.system.clone();
        closed.close().await;
        closed
    });
    for (what, state) in [("broken grant", broken_grant), ("pool gone", gone)] {
        let reply = send(who_route(state), get_with_authorization("/who", &value)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{what}");
        assert_eq!(reply.body, Problem::Unavailable.body(), "{what}");
        assert!(reply.headers.get("www-authenticate").is_none(), "{what}");
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn no_credential_is_a_401_without_touching_the_database(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    pools.system.close().await;
    let reply = send(who_route(pools.state()), get("/who")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    pools.close().await;
    Ok(())
}
