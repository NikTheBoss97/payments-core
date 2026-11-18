// Author: Nicola Antonello

//! The operator's routes for a merchant's webhook endpoints (ADR-0050): registering one,
//! whose secret is derived from the master key and shown once (ADR-0051), with a URL that
//! the server refuses by what is wrong with it (ADR-0060), under the operator's credential
//! and idempotency keys (ADR-0026).

mod common;

use std::error::Error;

use axum::http::StatusCode;
use common::api::{Pools, Reply, post, send};
use common::{assert_violation, endpoint_id, merchant_id, runtime_pool};
use futures_util::future::join_all;
use payments_core::api::router;
use payments_core::ids::EndpointId;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const CHECK_VIOLATION: &str = "23514";

/// A text member of a reply's JSON.
#[track_caller]
fn member(reply: &Reply, name: &str) -> String {
    let body: serde_json::Value = serde_json::from_str(&reply.body).expect("a JSON body");
    body[name]
        .as_str()
        .unwrap_or_else(|| panic!("no {name}: {reply:?}"))
        .to_owned()
}

/// A merchant that the operator registered.
async fn merchant(pools: &Pools, key: &str) -> String {
    let request = post("/merchants", &pools.operator, key, r#"{"fee_bps":250}"#);
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    member(&reply, "merchant_id")
}

/// The body that registers this URL.
fn with_url(url: &str) -> String {
    serde_json::json!({ "url": url }).to_string()
}

async fn register(pools: &Pools, merchant: &str, key: &str, body: &str) -> Reply {
    let path = format!("/merchants/{merchant}/webhook-endpoints");
    let request = post(&path, &pools.operator, key, body.to_owned());
    send(router(pools.state()), request).await
}

/// Every endpoint held: its ID, its merchant and its URL.
async fn endpoints(pool: &PgPool) -> sqlx::Result<Vec<(Uuid, Uuid, String)>> {
    let rows = sqlx::query!("SELECT id, merchant_id, url FROM webhook_endpoints ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.merchant_id, row.url))
        .collect())
}

async fn operator_records(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM operator_idempotency_records"#)
        .fetch_one(pool)
        .await
}

/// The problem type that a reply names.
#[track_caller]
fn problem(reply: &Reply) -> String {
    let kind = member(reply, "type");
    kind.rsplit('#').next().expect("a section").to_owned()
}

#[sqlx::test]
async fn an_endpoint_shows_its_secret_once_and_a_retry_gets_its_id(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant(&pools, "merchant").await;
    let body = with_url("HTTP://Merchant.example:8000/hooks?from=payments");
    let first = register(&pools, &merchant, "endpoint", &body).await;
    assert_eq!(first.status, StatusCode::CREATED, "{first:?}");
    let id = member(&first, "endpoint_id");
    let endpoint = EndpointId::try_from(Uuid::parse_str(&id)?)?;
    // The secret that the master key derives for this endpoint, and nothing else. No
    // message shows a secret.
    assert!(
        member(&first, "secret") == pools.master.secret_of(endpoint).written(),
        "the secret shown is not the endpoint's"
    );
    let object: serde_json::Value = serde_json::from_str(&first.body)?;
    assert_eq!(object.as_object().map(serde_json::Map::len), Some(2));
    // Stored with its scheme in lowercase, and the rest as written.
    assert_eq!(
        endpoints(&pool).await?,
        [(
            endpoint.get(),
            Uuid::parse_str(&merchant)?,
            "http://Merchant.example:8000/hooks?from=payments".to_owned()
        )]
    );
    // A retry gets the same ID and no secret, and registers nothing more.
    let again = register(&pools, &merchant, "endpoint", &body).await;
    assert_eq!(again.status, StatusCode::CREATED, "{again:?}");
    assert_eq!(member(&again, "endpoint_id"), id);
    assert!(!again.body.contains("secret"), "a retry shows a secret");
    assert!(!again.body.contains("whsec_"), "a retry shows a secret");
    assert_eq!(endpoints(&pool).await?.len(), 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn text_that_is_not_an_absolute_url_is_a_malformed_body(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant(&pools, "merchant").await;
    let mut bodies: Vec<String> = [
        "",
        "not a url",
        "merchant.example",
        "merchant.example/hooks",
        "/hooks",
        "http:///hooks",
        "http://merchant .example/",
        "http://merchant.example/a b",
        "http://merchant.example:port/",
        "http://merchant.example:65536/",
        "http://:80/",
    ]
    .into_iter()
    .map(with_url)
    .collect();
    // And bodies that are not what the route takes at all.
    bodies.extend(
        [
            "{}",
            r#"{"url":1}"#,
            r#"{"url":"http://merchant.example","more":1}"#,
            "http://merchant.example",
        ]
        .map(str::to_owned),
    );
    for body in &bodies {
        let reply = register(&pools, &merchant, "endpoint", body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}: {reply:?}");
        assert_eq!(problem(&reply), "malformed-body", "{body}");
    }
    // Nothing is stored, so the key is still free.
    assert_eq!(endpoints(&pool).await?, []);
    assert_eq!(operator_records(&pool).await?, 1);
    let reply = register(&pools, &merchant, "endpoint", &with_url("http://a.example")).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_absolute_url_that_the_rules_refuse_is_unusable(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant(&pools, "merchant").await;
    for url in [
        "https://merchant.example/hooks",
        "HTTPS://merchant.example/hooks",
        "ftp://merchant.example/",
        "http://user@merchant.example/",
        "http://user:secret@merchant.example/hooks",
        "http://merchant.example/hooks#part",
        "http://merchant.example#part",
        "http://merchant.example/?query#",
    ] {
        let reply = register(&pools, &merchant, "endpoint", &with_url(url)).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{url}: {reply:?}"
        );
        assert_eq!(problem(&reply), "unusable-url", "{url}");
    }
    // Nothing is stored, so the key is still free.
    assert_eq!(endpoints(&pool).await?, []);
    assert_eq!(operator_records(&pool).await?, 1);
    let reply = register(&pools, &merchant, "endpoint", &with_url("http://a.example")).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_merchant_is_a_404_and_stores_nothing(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    // The control: the route registers an endpoint for a merchant that it holds.
    let known = merchant(&pools, "merchant").await;
    let reply = register(&pools, &known, "known", &with_url("http://a.example")).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    let unknown = merchant_id(99).to_string();
    let reply = register(&pools, &unknown, "endpoint", &with_url("http://a.example")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{reply:?}");
    assert_eq!(problem(&reply), "not-found");
    assert_eq!(endpoints(&pool).await?.len(), 1);
    assert_eq!(operator_records(&pool).await?, 2);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_registration_racing_register_one_endpoint_and_show_its_secret_once(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = merchant(&pools, "merchant").await;
    let body = with_url("http://a.example/hooks");
    let replies = join_all((0..8).map(|_| register(&pools, &merchant, "k", &body))).await;
    let id = member(&replies[0], "endpoint_id");
    for reply in &replies {
        assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
        assert_eq!(member(reply, "endpoint_id"), id);
    }
    let shown = replies
        .iter()
        .filter(|reply| reply.body.contains("whsec_"))
        .count();
    assert_eq!(shown, 1, "the copies that show the secret");
    assert_eq!(endpoints(&pool).await?.len(), 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_database_refuses_another_scheme_user_information_or_a_fragment(
    pool: PgPool,
) -> TestResult {
    // Under raw SQL as the runtime role, the check stands behind the server's rules on the
    // scheme, user information and a fragment, and refuses an empty authority and
    // whitespace. A scheme in capitals is the server's to lowercase, and the host and the
    // port are the server's alone to read (ADR-0060).
    let pools = Pools::login(&pool).await?;
    let merchant = merchant(&pools, "merchant").await;
    let merchant = Uuid::parse_str(&merchant)?;
    let app = runtime_pool(&pool).await?;
    let insert = |n: u32, url: &'static str| {
        let app = app.clone();
        async move {
            sqlx::query!(
                "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
                endpoint_id(n).get(),
                merchant,
                url
            )
            .execute(&app)
            .await
        }
    };
    for (n, url) in (1..).zip([
        "https://merchant.example/",
        "http://user@merchant.example/",
        "http://merchant.example/#part",
        "HTTP://merchant.example/",
        "http:///hooks",
        "http://merchant .example/",
    ]) {
        let err = insert(n, url).await.expect_err(url);
        assert_violation(err, CHECK_VIOLATION, "webhook_endpoints_url_check");
    }
    // The control: a URL that the rules take is stored.
    insert(99, "http://merchant.example/hooks").await?;
    app.close().await;
    pools.close().await;
    Ok(())
}
