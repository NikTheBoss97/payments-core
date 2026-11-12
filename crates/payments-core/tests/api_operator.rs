// Author: Nicola Antonello

//! The operator's routes (ADR-0024): registering merchants, and issuing and revoking their
//! API keys, behind a credential of its own and under idempotency keys of its own
//! (ADR-0026). A secret is shown once, and nothing stores it.

mod common;

use std::error::Error;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::api::{
    Pools, Reply, a_merchant, a_payment, bearer, get_with_authorization, payments_stored, post,
    post_with_authorization, records, send, unused_webhooks, written,
};
use common::crashes::{install_crash_points, plan_death};
use common::keys::issued_key;
use common::payments::VERSION_4;
use common::{api_key_id, merchant_id, runtime_pool_of_one};
use futures_util::future::join_all;
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, router};
use payments_core::credentials::SecretDigest;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

const FEE: &str = r#"{"fee_bps":250}"#;
const CREATE: &str = r#"{"currency":"EUR","amount":10000}"#;

async fn register(pools: &Pools, key: &str, body: &str) -> Reply {
    let request = post("/merchants", &pools.operator, key, body.to_owned());
    send(router(pools.state()), request).await
}

async fn issue(pools: &Pools, merchant: &str, key: &str) -> Reply {
    let path = format!("/merchants/{merchant}/api-keys");
    send(router(pools.state()), post(&path, &pools.operator, key, "")).await
}

async fn revoke(pools: &Pools, api_key: &str, key: &str) -> Reply {
    let path = format!("/api-keys/{api_key}/revoke");
    send(router(pools.state()), post(&path, &pools.operator, key, "")).await
}

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
async fn registered(pools: &Pools, key: &str) -> String {
    let reply = register(pools, key, FEE).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    member(&reply, "merchant_id")
}

/// A key that the operator issued to this merchant: its ID and its secret.
async fn issued(pools: &Pools, merchant: &str, key: &str) -> (String, String) {
    let reply = issue(pools, merchant, key).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    (member(&reply, "api_key_id"), member(&reply, "secret"))
}

/// A merchant's `POST /payments`, presenting a secret that came out of an answer.
async fn create_payment(pools: &Pools, secret: &str, key: &str) -> Reply {
    let request = post_with_authorization("/payments", &format!("Bearer {secret}"), key, CREATE);
    send(router(pools.state()), request).await
}

/// A merchant's `GET /payments/{id}`, presenting a secret that came out of an answer.
async fn read_payment(pools: &Pools, secret: &str, payment: &str) -> Reply {
    let request =
        get_with_authorization(&format!("/payments/{payment}"), &format!("Bearer {secret}"));
    send(router(pools.state()), request).await
}

async fn merchants(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM merchants"#)
        .fetch_one(pool)
        .await
}

async fn api_keys(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM api_keys"#)
        .fetch_one(pool)
        .await
}

async fn revocations(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM api_key_revocations"#)
        .fetch_one(pool)
        .await
}

async fn operator_records(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM operator_idempotency_records"#)
        .fetch_one(pool)
        .await
}

/// How many rows hold what an operator's request writes.
async fn stored(pool: &PgPool) -> sqlx::Result<[i64; 4]> {
    Ok([
        merchants(pool).await?,
        api_keys(pool).await?,
        revocations(pool).await?,
        operator_records(pool).await?,
    ])
}

/// Every row of every table of the application, as text. Bytes come out as base64, so a
/// digest does not read as the secret it came from.
async fn every_table_as_text(pool: &PgPool) -> sqlx::Result<String> {
    let tables = sqlx::query_scalar!(
        r#"SELECT query_to_xml(format('SELECT * FROM %I', c.relname), true, false, '')::text
                  AS "rows!"
           FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = 'public' AND c.relkind = 'r'"#
    )
    .fetch_all(pool)
    .await?;
    Ok(tables.join("\n"))
}

#[sqlx::test]
async fn a_merchant_that_the_operator_registers_can_take_a_payment(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;

    let reply = register(&pools, "register", FEE).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    let merchant = member(&reply, "merchant_id");
    assert_eq!(reply.body, format!(r#"{{"merchant_id":"{merchant}"}}"#));
    let fee = sqlx::query_scalar!(
        "SELECT fee_bps FROM merchants WHERE id = $1",
        Uuid::parse_str(&merchant)?
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(fee, 250);

    let reply = issue(&pools, &merchant, "issue").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    let (api_key, secret) = (member(&reply, "api_key_id"), member(&reply, "secret"));
    assert_eq!(
        reply.body,
        format!(r#"{{"api_key_id":"{api_key}","secret":"{secret}"}}"#)
    );
    let digits = secret.strip_prefix("pck_").expect("the prefix");
    assert_eq!(digits.len(), 64);
    assert!(
        digits
            .bytes()
            .all(|d| matches!(d, b'0'..=b'9' | b'a'..=b'f'))
    );

    // The key belongs to the merchant, and only its digest is stored.
    let row = sqlx::query!(
        "SELECT merchant_id, digest FROM api_keys WHERE id = $1",
        Uuid::parse_str(&api_key)?
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.merchant_id.to_string(), merchant);
    assert_eq!(row.digest, SecretDigest::of(&secret).as_bytes());

    // The key opens the merchant's routes, and a payment copies the fee the operator set.
    let reply = create_payment(&pools, &secret, "create").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    let payment = member(&reply, "payment_id");
    let owner = sqlx::query_scalar!(
        "SELECT merchant_id FROM payments WHERE id = $1",
        Uuid::parse_str(&payment)?
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(owner.to_string(), merchant);
    let reply = read_payment(&pools, &secret, &payment).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert!(reply.body.contains(r#""fee_bps":250"#), "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_fee_at_each_end_of_its_range_is_taken(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    for (fee, key) in [(0, "none"), (10_000, "all")] {
        let body = format!(r#"{{"fee_bps":{fee}}}"#);
        let reply = register(&pools, key, &body).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{fee}: {reply:?}");
        let stored = sqlx::query_scalar!(
            "SELECT fee_bps FROM merchants WHERE id = $1",
            Uuid::parse_str(&member(&reply, "merchant_id"))?
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(stored, fee);
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retried_registration_gets_the_first_answer_and_registers_nothing_more(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let first = register(&pools, "k", FEE).await;
    let second = register(&pools, "k", FEE).await;
    assert_eq!(first.status, StatusCode::CREATED, "{first:?}");
    assert_eq!(second.status, first.status);
    assert_eq!(second.body, first.body);
    assert_eq!(second.headers, first.headers);
    assert_eq!(merchants(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retried_issue_gets_the_keys_id_and_never_its_secret(pool: PgPool) -> TestResult {
    // The secret is shown once (ADR-0024). An operator who lost that answer issues another.
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let first = issue(&pools, &merchant, "k").await;
    let (api_key, secret) = (member(&first, "api_key_id"), member(&first, "secret"));

    let second = issue(&pools, &merchant, "k").await;

    assert_eq!(second.status, StatusCode::CREATED, "{second:?}");
    assert_eq!(second.header("content-type"), "application/json");
    assert_eq!(second.body, format!(r#"{{"api_key_id":"{api_key}"}}"#));
    assert_eq!(api_keys(&pool).await?, 1);
    let reply = create_payment(&pools, &secret, "create").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn nothing_stored_holds_a_secret(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let first = issue(&pools, &merchant, "issue").await;
    let (api_key, secret) = (member(&first, "api_key_id"), member(&first, "secret"));
    issue(&pools, &merchant, "issue").await;
    let reply = create_payment(&pools, &secret, "create").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    let reply = revoke(&pools, &api_key, "revoke").await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");

    // The control for what follows: this is what a stored secret would look like.
    assert!(first.body.contains("pck_"), "{first:?}");
    assert!(first.body.contains(r#""secret""#), "{first:?}");

    let answers = sqlx::query_scalar!(
        r#"SELECT body AS "body!" FROM operator_idempotency_records
           UNION ALL SELECT body FROM idempotency_records"#
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(answers.len(), 4, "{answers:?}");
    for answer in &answers {
        assert!(!answer.contains("pck_"), "{answer}");
        assert!(!answer.contains("secret"), "{answer}");
    }

    // Nor does any other table. The control: the scan does read the rows.
    let everything = every_table_as_text(&pool).await?;
    assert!(everything.contains(&merchant), "the scan read no merchant");
    assert!(everything.contains(&api_key), "the scan read no API key");
    let digits = secret.strip_prefix("pck_").expect("the prefix");
    assert!(
        !everything.contains(digits),
        "a merchant's secret is stored"
    );
    let operator = pools.operator.expose();
    let digits = operator.strip_prefix("pck_").expect("the prefix");
    assert!(
        !everything.contains(digits),
        "the operator's secret is stored"
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_revoked_key_stops_at_once_and_its_merchants_other_keys_go_on(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (old_id, old) = issued(&pools, &merchant, "issue-old").await;
    let (_, new) = issued(&pools, &merchant, "issue-new").await;
    let reply = create_payment(&pools, &old, "create").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    let payment = member(&reply, "payment_id");
    for secret in [&old, &new] {
        let reply = read_payment(&pools, secret, &payment).await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    }

    let reply = revoke(&pools, &old_id, "revoke").await;

    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(reply.header("content-type"), "application/json");
    assert_eq!(reply.body, format!(r#"{{"api_key_id":"{old_id}"}}"#));
    let refused = read_payment(&pools, &old, &payment).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{refused:?}");
    assert_eq!(refused.body, Problem::Unauthorized.body());
    let reply = read_payment(&pools, &new, &payment).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn revoking_a_revoked_key_is_a_200_and_changes_nothing(pool: PgPool) -> TestResult {
    // What the request asks for already holds, and each key keeps its own answer.
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (api_key, _) = issued(&pools, &merchant, "issue").await;
    let first = revoke(&pools, &api_key, "first").await;
    let retry = revoke(&pools, &api_key, "first").await;
    let second = revoke(&pools, &api_key, "second").await;
    for reply in [&first, &retry, &second] {
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        assert_eq!(reply.body, first.body);
    }
    assert_eq!(revocations(&pool).await?, 1);
    assert_eq!(operator_records(&pool).await?, 4);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_registration_racing_register_one_merchant(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let replies = join_all((0..8).map(|_| register(&pools, "k", FEE))).await;
    for reply in &replies {
        assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
        assert_eq!(reply.body, replies[0].body);
    }
    assert_eq!(merchants(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_an_issue_racing_issue_one_key_and_show_its_secret_once(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let replies = join_all((0..8).map(|_| issue(&pools, &merchant, "k"))).await;
    let id = member(&replies[0], "api_key_id");
    for reply in &replies {
        assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
        assert_eq!(member(reply, "api_key_id"), id);
    }
    let shown: Vec<&Reply> = replies
        .iter()
        .filter(|reply| reply.body.contains("pck_"))
        .collect();
    assert_eq!(shown.len(), 1, "{replies:?}");
    assert_eq!(api_keys(&pool).await?, 1);
    let reply = create_payment(&pools, &member(shown[0], "secret"), "create").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_a_revocation_racing_give_one_answer(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (api_key, _) = issued(&pools, &merchant, "issue").await;
    let replies = join_all((0..8).map(|_| revoke(&pools, &api_key, "k"))).await;
    for reply in &replies {
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        assert_eq!(reply.body, replies[0].body);
    }
    assert_eq!(revocations(&pool).await?, 1);
    assert_eq!(operator_records(&pool).await?, 3);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_key_is_for_one_request_and_the_first_answer_stands(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let first = register(&pools, "k", FEE).await;
    let merchant = member(&first, "merchant_id");
    let (api_key, _) = issued(&pools, &merchant, "issue").await;

    for reply in [
        // Another fee, and the same fee in another layout.
        register(&pools, "k", r#"{"fee_bps":251}"#).await,
        register(&pools, "k", r#"{"fee_bps": 250}"#).await,
        // Another route.
        issue(&pools, &merchant, "k").await,
        revoke(&pools, &api_key, "k").await,
    ] {
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
        assert_eq!(reply.body, Problem::KeyReused.body());
    }
    assert_eq!(stored(&pool).await?, [1, 1, 0, 2]);
    assert_eq!(register(&pools, "k", FEE).await.body, first.body);

    // Another merchant under the same route.
    let another = registered(&pools, "register-another").await;
    issue(&pools, &merchant, "issue-both").await;
    let reply = issue(&pools, &another, "issue-both").await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_operators_keys_are_apart_from_a_merchants(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let first = register(&pools, "shared", FEE).await;
    let (_, secret) = issued(&pools, &member(&first, "merchant_id"), "issue").await;

    let reply = create_payment(&pools, &secret, "shared").await;

    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(register(&pools, "shared", FEE).await.body, first.body);
    assert_eq!(operator_records(&pool).await?, 2);
    assert_eq!(records(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

/// A `POST` with these `Authorization` values, none or several, which may not be text.
fn with_authorizations(path: &str, values: &[&[u8]], body: &'static str) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("idempotency-key", "\"k\"");
    for value in values {
        request = request.header("authorization", *value);
    }
    request.body(Body::from(body)).expect("a valid request")
}

#[sqlx::test]
async fn every_bad_credential_gets_one_answer_on_every_operator_route_and_runs_nothing(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, mine) = a_merchant(&pools, 1).await?;
    let revoked = issued_key(&pools.system, merchant, 2).await?;
    let reply = revoke(&pools, &api_key_id(2).to_string(), "revoke").await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    let before = stored(&pool).await?;

    let good = format!("Bearer {}", pools.operator.expose());
    let as_basic = format!("Basic {}", pools.operator.expose());
    let no_scheme = pools.operator.expose().to_owned();
    let upper = format!("Bearer {}", pools.operator.expose().to_uppercase());
    let shorter = format!("Bearer {}", &pools.operator.expose()[..67]);
    let longer = format!("{good}0");
    let holds = format!("Bearer {}", written(pools.operator.digest()));
    let a_merchants = bearer(&mine);
    let a_revoked = bearer(&revoked);
    let cases: [(&str, Vec<&[u8]>); 14] = [
        ("none", vec![]),
        ("empty", vec![b""]),
        ("the scheme alone", vec![b"Bearer"]),
        ("the scheme and a space", vec![b"Bearer "]),
        ("another scheme", vec![as_basic.as_bytes()]),
        ("no scheme", vec![no_scheme.as_bytes()]),
        ("upper case", vec![upper.as_bytes()]),
        ("cut short", vec![shorter.as_bytes()]),
        ("one digit more", vec![longer.as_bytes()]),
        ("what the environment holds", vec![holds.as_bytes()]),
        ("a merchant's key", vec![a_merchants.as_bytes()]),
        ("a merchant's revoked key", vec![a_revoked.as_bytes()]),
        ("not text", vec![b"Bearer \xff\xfe"]),
        // Which of two is meant cannot be known, even if each is good on its own.
        ("twice", vec![good.as_bytes(), good.as_bytes()]),
    ];
    let routes: [(String, &'static str); 3] = [
        ("/merchants".to_owned(), FEE),
        (format!("/merchants/{merchant}/api-keys"), ""),
        (format!("/api-keys/{}/revoke", api_key_id(1)), ""),
    ];
    for (path, body) in &routes {
        for (what, values) in &cases {
            let request = with_authorizations(path, values, body);
            let reply = send(router(pools.state()), request).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}, {what}");
            assert_eq!(reply.header("www-authenticate"), "Bearer", "{path}, {what}");
            assert_eq!(
                reply.header("content-type"),
                "application/problem+json",
                "{path}, {what}"
            );
            assert_eq!(reply.body, Problem::Unauthorized.body(), "{path}, {what}");
        }
    }
    assert_eq!(stored(&pool).await?, before);

    // The control: the secret that these cases damaged opens every one of these routes.
    for (path, body) in routes {
        let request = post_with_authorization(&path, &good, &path, body);
        let reply = send(router(pools.state()), request).await;
        assert!(reply.status.is_success(), "{path}: {reply:?}");
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_operators_secret_opens_no_merchants_route(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, mine) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    let read = format!("/payments/{payment}");
    let cancel = format!("/payments/{payment}/cancel");
    for request in [
        post("/payments", &pools.operator, "k", CREATE),
        get_with_authorization(&read, &bearer(&pools.operator)),
        post(&cancel, &pools.operator, "k", ""),
    ] {
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{reply:?}");
        assert_eq!(reply.body, Problem::Unauthorized.body());
    }
    assert_eq!(payments_stored(&pool).await?, 1);

    // The control: the merchant's own key opens them.
    let request = get_with_authorization(&read, &bearer(&mine));
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert!(reply.body.contains(r#""status":"created""#), "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_operators_scheme_reads_like_a_merchants(pool: PgPool) -> TestResult {
    // RFC 9110 section 11.1 and RFC 6750 section 2.1.
    let pools = Pools::login(&pool).await?;
    for (n, prefix) in ["Bearer ", "bearer ", "BEARER ", "Bearer   "]
        .iter()
        .enumerate()
    {
        let value = format!("{prefix}{}", pools.operator.expose());
        let request = post_with_authorization("/merchants", &value, &format!("k-{n}"), FEE);
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{prefix:?}: {reply:?}");
    }
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_bad_operator_credential_is_a_401_without_touching_the_database(
    pool: PgPool,
) -> TestResult {
    // The digest is in memory, so a database that has gone cannot turn a bad secret into a
    // 503. The good secret gets as far as looking its key up.
    let pools = Pools::login(&pool).await?;
    pools.system.close().await;
    pools.tenant.close().await;
    let unknown = payments_core::credentials::ApiKeySecret::generate();
    let reply = send(
        router(pools.state()),
        post("/merchants", &unknown, "k", FEE),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{reply:?}");

    let reply = register(&pools, "k", FEE).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    assert_eq!(reply.body, Problem::Unavailable.body());
    Ok(())
}

#[sqlx::test]
async fn a_body_that_cannot_be_used_is_refused_and_leaves_the_key_free(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let bad: [&[u8]; 21] = [
        b"",
        b" ",
        b"null",
        b"[]",
        b"{}",
        b"250",
        b"not JSON",
        b"\xff\xfe",
        br#"{"fee_bps":null}"#,
        br#"{"fee_bps":"250"}"#,
        br#"{"fee_bps":-1}"#,
        br#"{"fee_bps":2.5}"#,
        br#"{"fee_bps":250.0}"#,
        br#"{"fee_bps":1e2}"#,
        br#"{"fee_bps":10001}"#,
        br#"{"fee_bps":65535}"#,
        br#"{"fee_bps":65536}"#,
        br#"{"fee_bps":250,"fee_bps":250}"#,
        // A field that it does not know, even one that names a merchant.
        br#"{"fee_bps":250,"merchant_id":"019b76da-a800-7000-8000-000000000001"}"#,
        br#"{"fee":250}"#,
        br#"{"FEE_BPS":250}"#,
    ];
    for body in bad {
        let request = post("/merchants", &pools.operator, "k", body.to_vec());
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body:?}: {reply:?}");
        assert_eq!(reply.body, Problem::MalformedBody.body(), "{body:?}");
    }
    assert_eq!(stored(&pool).await?, [0, 0, 0, 0]);

    let reply = register(&pools, "k", FEE).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_route_that_takes_no_body_refuses_one_and_leaves_the_key_free(
    pool: PgPool,
) -> TestResult {
    // Not even an empty object: two bodies that it ignored would still read as two requests
    // under a key.
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (api_key, _) = issued(&pools, &merchant, "issue").await;
    let before = stored(&pool).await?;
    let routes = [
        format!("/merchants/{merchant}/api-keys"),
        format!("/api-keys/{api_key}/revoke"),
    ];
    for path in &routes {
        for body in ["{}", "null", " ", "\n", "anything"] {
            let request = post(path, &pools.operator, "k", body);
            let reply = send(router(pools.state()), request).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}: {reply:?}");
            assert_eq!(reply.body, Problem::MalformedBody.body(), "{path}");
        }
    }
    assert_eq!(stored(&pool).await?, before);

    for (n, path) in routes.iter().enumerate() {
        let key = format!("k-{n}");
        let request = post(path, &pools.operator, &key, "{}");
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}: {reply:?}");
        let request = post(path, &pools.operator, &key, "");
        let reply = send(router(pools.state()), request).await;
        assert!(reply.status.is_success(), "{path}: {reply:?}");
    }
    pools.close().await;
    Ok(())
}

/// A `POST` with the operator's secret, these `Idempotency-Key` values, and no body.
fn with_keys(pools: &Pools, path: &str, keys: &[&str], body: &'static str) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("authorization", bearer(&pools.operator));
    for key in keys {
        request = request.header("idempotency-key", *key);
    }
    request.body(Body::from(body)).expect("a valid request")
}

#[sqlx::test]
async fn every_operator_route_refuses_a_request_with_no_key_and_runs_nothing(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (api_key, _) = issued(&pools, &merchant, "issue").await;
    let before = stored(&pool).await?;
    let routes: [(String, &'static str); 3] = [
        ("/merchants".to_owned(), FEE),
        (format!("/merchants/{merchant}/api-keys"), ""),
        (format!("/api-keys/{api_key}/revoke"), ""),
    ];
    for (path, body) in &routes {
        for keys in [&[][..], &["bare-token"], &["\"a\"", "\"b\""]] {
            let request = with_keys(&pools, path, keys, body);
            let reply = send(router(pools.state()), request).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}: {reply:?}");
            assert_eq!(reply.body, Problem::MalformedKey.body(), "{path}");
        }
    }
    assert_eq!(stored(&pool).await?, before);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_id_that_is_not_one_reads_as_no_such_thing_and_runs_nothing(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (api_key, _) = issued(&pools, &merchant, "issue").await;
    let before = stored(&pool).await?;

    for (route, real) in [
        ("/merchants/{}/api-keys", &merchant),
        ("/api-keys/{}/revoke", &api_key),
    ] {
        for text in [
            "nothing".to_owned(),
            real.to_uppercase(),
            real.replace('-', ""),
            format!("urn:uuid:{real}"),
            real.replacen('0', "%30", 1),
            VERSION_4.to_string(),
            Uuid::nil().to_string(),
        ] {
            let path = route.replace("{}", &text);
            let request = post(&path, &pools.operator, "k", "");
            let reply = send(router(pools.state()), request).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}: {reply:?}");
            assert_eq!(reply.body, Problem::NotFound.body(), "{path}");
        }
    }
    assert_eq!(stored(&pool).await?, before);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn no_such_merchant_or_api_key_is_a_404_and_leaves_the_key_free(pool: PgPool) -> TestResult {
    // IDs are made by the server, so this cannot become true later, and no answer is stored
    // (ADR-0023).
    let pools = Pools::login(&pool).await?;
    let no_merchant = merchant_id(99).to_string();
    let no_key = api_key_id(99).to_string();
    for reply in [
        issue(&pools, &no_merchant, "k").await,
        revoke(&pools, &no_key, "k").await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{reply:?}");
        assert_eq!(reply.body, Problem::NotFound.body());
    }
    assert_eq!(stored(&pool).await?, [0, 0, 0, 0]);

    let reply = register(&pools, "k", FEE).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_credential_comes_first_then_the_id_then_the_key(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let good = format!("/merchants/{merchant}/api-keys");

    // No credential and a bad ID: someone with no key learns nothing about an ID.
    let request = with_authorizations("/merchants/nothing/api-keys", &[], "");
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{reply:?}");

    // A bad ID and no key.
    let request = with_keys(&pools, "/merchants/nothing/api-keys", &[], "");
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{reply:?}");

    // A good ID and no key.
    let request = with_keys(&pools, &good, &[], "");
    let reply = send(router(pools.state()), request).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{reply:?}");
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_operator_route_takes_post_and_nothing_else(pool: PgPool) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let id = merchant_id(1);
    for path in [
        "/merchants".to_owned(),
        format!("/merchants/{id}/api-keys"),
        format!("/api-keys/{id}/revoke"),
    ] {
        for method in [Method::GET, Method::PUT, Method::DELETE] {
            let request = Request::builder()
                .method(method.clone())
                .uri(&path)
                .header("authorization", bearer(&pools.operator))
                .body(Body::empty())?;
            let reply = send(router(pools.state()), request).await;
            assert_eq!(
                reply.status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path}: {reply:?}"
            );
            assert_eq!(reply.header("allow"), "POST", "{method} {path}");
            assert_eq!(reply.body, Problem::MethodNotAllowed.body());
        }
    }
    for path in [format!("/merchants/{id}"), format!("/api-keys/{id}")] {
        let request = get_with_authorization(&path, &bearer(&pools.operator));
        let reply = send(router(pools.state()), request).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{path}: {reply:?}");
    }
    pools.close().await;
    Ok(())
}

/// Sends a request whose backend dies just before the key's row, which must be a 503 that
/// leaves nothing, and then sends it again, on a new connection, under the same key.
async fn killed_then_retried(
    pool: &PgPool,
    pools: &Pools,
    system: &PgPool,
    path: &str,
    body: &'static str,
) -> Result<Reply, Box<dyn Error>> {
    let state = AppState::new(
        pools.tenant.clone(),
        system.clone(),
        pools.operator.digest(),
        unused_webhooks(),
        common::api::unused_handovers(),
    );
    let before = stored(pool).await?;
    plan_death(system, "die_before operator_idempotency_records").await?;
    let request = post(path, &pools.operator, path, body);
    let reply = send(router(state.clone()), request).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{path}: {reply:?}"
    );
    assert_eq!(reply.body, Problem::Unavailable.body(), "{path}");
    assert_eq!(stored(pool).await?, before, "{path}");
    Ok(send(router(state), post(path, &pools.operator, path, body)).await)
}

#[sqlx::test]
async fn a_command_that_cannot_be_confirmed_is_a_503_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    install_crash_points(&pool).await?;
    let system = runtime_pool_of_one(&pool).await?;

    let reply = killed_then_retried(&pool, &pools, &system, "/merchants", FEE).await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(stored(&pool).await?, [1, 0, 0, 1]);

    let keys = format!("/merchants/{}/api-keys", member(&reply, "merchant_id"));
    let reply = killed_then_retried(&pool, &pools, &system, &keys, "").await?;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(stored(&pool).await?, [1, 1, 0, 2]);
    // The attempt that died left nothing, so this answer is the first, and shows the secret.
    let secret = member(&reply, "secret");
    let revoking = format!("/api-keys/{}/revoke", member(&reply, "api_key_id"));
    let reply = create_payment(&pools, &secret, "before").await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");

    let reply = killed_then_retried(&pool, &pools, &system, &revoking, "").await?;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(stored(&pool).await?, [1, 1, 1, 3]);
    let reply = create_payment(&pools, &secret, "after").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{reply:?}");
    system.close().await;
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_statement_the_database_refuses_is_a_500_and_leaves_the_key_free(
    pool: PgPool,
) -> TestResult {
    // A rule that the types do not know about stands for a bug. Its answer says nothing of
    // the cause, and it is no final answer to store.
    let pools = Pools::login(&pool).await?;
    let merchant = registered(&pools, "register").await;
    let (api_key, _) = issued(&pools, &merchant, "issue").await;
    let before = stored(&pool).await?;
    sqlx::query!("ALTER TABLE merchants ADD CONSTRAINT refuses_all CHECK (fee_bps < 0) NOT VALID")
        .execute(&pool)
        .await?;
    sqlx::query!(
        "ALTER TABLE api_keys ADD CONSTRAINT refuses_all CHECK (octet_length(digest) < 0) NOT VALID"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "ALTER TABLE api_key_revocations ADD CONSTRAINT refuses_all \
         CHECK (recorded_at IS NULL) NOT VALID"
    )
    .execute(&pool)
    .await?;

    for reply in [
        register(&pools, "k", FEE).await,
        issue(&pools, &merchant, "k").await,
        revoke(&pools, &api_key, "k").await,
    ] {
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR, "{reply:?}");
        assert_eq!(reply.body, Problem::Internal.body());
    }
    assert_eq!(stored(&pool).await?, before);

    sqlx::query!("ALTER TABLE merchants DROP CONSTRAINT refuses_all")
        .execute(&pool)
        .await?;
    sqlx::query!("ALTER TABLE api_keys DROP CONSTRAINT refuses_all")
        .execute(&pool)
        .await?;
    sqlx::query!("ALTER TABLE api_key_revocations DROP CONSTRAINT refuses_all")
        .execute(&pool)
        .await?;
    assert_eq!(register(&pools, "k", FEE).await.status, StatusCode::CREATED);
    assert_eq!(
        issue(&pools, &merchant, "k-issue").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        revoke(&pools, &api_key, "k-revoke").await.status,
        StatusCode::OK
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn nothing_runs_when_a_key_cannot_be_looked_up(pool: PgPool) -> TestResult {
    // The rest of the database works, so a request that went on regardless would get as far
    // as its effect. No statement has looked a key up on these connections, so none still
    // points at the table.
    let pools = Pools::login(&pool).await?;
    let (merchant, _) = a_merchant(&pools, 1).await?;
    let before = stored(&pool).await?;
    sqlx::query!("ALTER TABLE operator_idempotency_records RENAME TO operator_records_gone")
        .execute(&pool)
        .await?;

    for reply in [
        register(&pools, "k", FEE).await,
        issue(&pools, &merchant.to_string(), "k").await,
        revoke(&pools, &api_key_id(1).to_string(), "k").await,
    ] {
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
        assert_eq!(reply.body, Problem::Unavailable.body());
    }
    assert_eq!(
        [
            merchants(&pool).await?,
            api_keys(&pool).await?,
            revocations(&pool).await?
        ],
        before[..3]
    );
    pools.close().await;
    Ok(())
}
