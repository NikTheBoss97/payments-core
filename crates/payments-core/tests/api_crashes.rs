// Author: Nicola Antonello

//! The backend killed around each statement of every route, over HTTP (ADR-0014, ADR-0026).
//!
//! The library's crash tests show what a command leaves behind. These show what a client is
//! told. At every point its answer is a 503 that says nothing of the cause, nothing written in
//! the transaction stays, and sending the request again under its key applies it once. A late
//! copy then gets that answer and changes nothing.

mod common;

use std::error::Error;

use axum::http::StatusCode;
use common::api::{
    Pools, Reply, a_merchant, a_payment, bearer, get_with_authorization, post, send,
    unused_webhooks,
};
use common::crashes::{
    AT_COMMIT, ATTEMPTS, OPERATOR_AT_COMMIT, OPERATOR_KEY_POINTS, POSTING_POINTS, Role,
    STEP_POINTS, assert_books_are_whole, balances, install_crash_points, kill_during_commit,
    plan_death, slow_commit_pool, stored,
};
use common::keys::issued_key;
use common::payments::{AUTHORIZED, FEE, authorize_command, capture_command, steps, take_lock};
use common::{
    api_key_id, merchant_id, payment_id, runtime_pool_of_one, tenant_pool_of_one,
    tenant_transaction, wait_for_lock_wait,
};
use futures_util::future::join;
use payments_core::api::problem::Problem;
use payments_core::api::{AppState, router};
use payments_core::credentials::ApiKeySecret;
use payments_core::idempotency::Fingerprint;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::payments::{authorize, capture};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

const CREATE: &str = r#"{"currency":"EUR","amount":10000}"#;
const AUTHORIZE: &str = r#"{"amount":8000,"expires_at":"2027-01-01T00:00:00Z"}"#;
const CAPTURE: &str = r#"{"amount":8000,"captured_at":"2026-12-01T00:00:00Z"}"#;
const REFUND: &str = r#"{"amount":3000,"refunded_at":"2026-12-02T00:00:00Z"}"#;
const REGISTER: &str = r#"{"fee_bps":250}"#;

/// The points around a merchant's key's own row.
const KEY_POINTS: [&str; 2] = [
    "die_before idempotency_records",
    "die_after idempotency_records",
];

/// How many rows hold what a merchant's request and an operator's write in their transactions.
/// A request that died must leave these as they were.
type Rows = ([i64; 8], [i64; 4]);

async fn rows(pool: &PgPool) -> sqlx::Result<Rows> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM merchants) AS "merchants!",
                  (SELECT count(*) FROM api_keys) AS "api_keys!",
                  (SELECT count(*) FROM api_key_revocations) AS "revocations!",
                  (SELECT count(*) FROM operator_idempotency_records) AS "answers!""#
    )
    .fetch_one(pool)
    .await?;
    Ok((
        stored(pool).await?,
        [row.merchants, row.api_keys, row.revocations, row.answers],
    ))
}

/// A server, one of whose roles holds a pool of one session, whose death a test can plan.
struct Kills<'a> {
    pool: &'a PgPool,
    session: &'a PgPool,
    state: AppState,
}

/// What a request came to once it was sent again, and what a late copy of it then got.
struct Settled {
    first: Reply,
    late: Reply,
}

impl Kills<'_> {
    async fn send(&self, path: &str, secret: &ApiKeySecret, key: &str, body: &str) -> Reply {
        let request = post(path, secret, key, body.to_owned());
        send(router(self.state.clone()), request).await
    }

    /// Sends a request whose backend dies at this point: a 503 that leaves nothing.
    async fn killed(
        &self,
        point: &str,
        path: &str,
        secret: &ApiKeySecret,
        key: &str,
        body: &str,
    ) -> TestResult {
        let before = rows(self.pool).await?;
        plan_death(self.session, point).await?;
        let reply = self.send(path, secret, key, body).await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{point}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::Unavailable.body(), "{point}");
        assert_eq!(rows(self.pool).await?, before, "{point}");
        Ok(())
    }

    /// Sends it again, on a new session, which must change something, and then once more,
    /// which must change nothing.
    async fn settled(
        &self,
        point: &str,
        path: &str,
        secret: &ApiKeySecret,
        key: &str,
        body: &str,
    ) -> Result<Settled, Box<dyn Error>> {
        let before = rows(self.pool).await?;
        let first = self.send(path, secret, key, body).await;
        let after = rows(self.pool).await?;
        assert_ne!(
            after, before,
            "{point}: sent again, it did nothing: {first:?}"
        );
        let late = self.send(path, secret, key, body).await;
        assert_eq!(
            rows(self.pool).await?,
            after,
            "{point}: a late copy did something"
        );
        assert_eq!(late.status, first.status, "{point}");
        Ok(Settled { first, late })
    }

    async fn killed_then_settled(
        &self,
        point: &str,
        path: &str,
        secret: &ApiKeySecret,
        key: &str,
        body: &str,
    ) -> Result<Settled, Box<dyn Error>> {
        self.killed(point, path, secret, key, body).await?;
        self.settled(point, path, secret, key, body).await
    }
}

/// The pools, a pool of one session as the tenant role, and the crash points.
async fn merchant_side(pool: &PgPool) -> Result<(Pools, PgPool), Box<dyn Error>> {
    let pools = Pools::login(pool).await?;
    install_crash_points(pool).await?;
    let tenant = tenant_pool_of_one(pool).await?;
    Ok((pools, tenant))
}

/// The status that a payment starts from.
#[derive(Clone, Copy)]
enum Start {
    Created,
    Authorized,
    Captured,
}

impl Start {
    /// Payment `n` of this merchant, there through the library.
    async fn payment(
        self,
        pools: &Pools,
        merchant: MerchantId,
        n: u32,
    ) -> Result<PaymentId, Box<dyn Error>> {
        let me = Tenant::new(&pools.tenant, merchant);
        let payment = a_payment(pools, merchant, n).await?;
        if matches!(self, Self::Authorized | Self::Captured) {
            authorize(me, &authorize_command(n, payment)).await?;
        }
        if matches!(self, Self::Captured) {
            capture(me, &capture_command(n, payment)).await?;
        }
        Ok(payment)
    }
}

/// Kills this command at each point, each on a payment of its own, and checks its answer. It
/// returns how many applied.
async fn killed_at_every_point(
    pool: &PgPool,
    start: Start,
    verb: &str,
    body: &str,
    points: &[&str],
    answer: &str,
) -> Result<i64, Box<dyn Error>> {
    let (pools, tenant) = merchant_side(pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let kills = Kills {
        pool,
        session: &tenant,
        state: AppState::new(
            tenant.clone(),
            pools.system.clone(),
            pools.operator.digest(),
            unused_webhooks(),
        ),
    };
    let mut applied = 0;
    for (n, point) in (1_u32..).zip(points) {
        let payment = start.payment(&pools, merchant, n).await?;
        let path = format!("/payments/{payment}/{verb}");
        let key = format!("{verb}-{n}");
        let settled = kills
            .killed_then_settled(point, &path, &secret, &key, body)
            .await?;
        assert_eq!(
            settled.first.status,
            StatusCode::OK,
            "{point}: {:?}",
            settled.first
        );
        assert_eq!(settled.first.body, answer, "{point}");
        assert_eq!(settled.late.body, answer, "{point}");
        applied += 1;
    }
    assert_books_are_whole(pool).await?;
    tenant.close().await;
    pools.close().await;
    Ok(applied)
}

#[sqlx::test]
async fn a_create_killed_anywhere_is_a_503_and_the_retry_settles_it(pool: PgPool) -> TestResult {
    let (pools, tenant) = merchant_side(&pool).await?;
    let kills = Kills {
        pool: &pool,
        session: &tenant,
        state: AppState::new(
            tenant.clone(),
            pools.system.clone(),
            pools.operator.digest(),
            unused_webhooks(),
        ),
    };
    let points = [
        "die_before accounts",
        "die_after accounts",
        "die_before payments",
        "die_after payments",
    ]
    .into_iter()
    .chain(STEP_POINTS)
    .chain(KEY_POINTS)
    .chain([AT_COMMIT]);
    for (n, point) in (1_u32..).zip(points) {
        // A merchant of its own, so that each payment has a new account to open. Accounts
        // that an attempt had opened stay, committed on their own, and are not in the count.
        let (_, secret) = a_merchant(&pools, n).await?;
        let settled = kills
            .killed_then_settled(point, "/payments", &secret, "create", CREATE)
            .await?;
        assert_eq!(
            settled.first.status,
            StatusCode::CREATED,
            "{point}: {:?}",
            settled.first
        );
        assert_eq!(settled.late.body, settled.first.body, "{point}");
    }
    assert_books_are_whole(&pool).await?;
    tenant.close().await;
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_authorization_killed_anywhere_is_a_503_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain([
            "die_before payment_authorizations",
            "die_after payment_authorizations",
        ])
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    killed_at_every_point(
        &pool,
        Start::Created,
        "authorize",
        AUTHORIZE,
        &points,
        r#"{"seq":1,"from":"created","to":"authorized","refundable":0}"#,
    )
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_capture_killed_anywhere_is_a_503_moves_no_money_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_captures", "die_after payment_captures"])
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    let captures = killed_at_every_point(
        &pool,
        Start::Authorized,
        "capture",
        CAPTURE,
        &points,
        r#"{"seq":2,"from":"authorized","to":"captured","refundable":8000}"#,
    )
    .await?;
    assert_eq!(
        balances(&pool, merchant_id(1)).await?,
        [
            captures * AUTHORIZED,
            captures * (FEE - AUTHORIZED),
            -captures * FEE
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn a_refund_killed_anywhere_is_a_503_moves_no_money_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    // The sharpest case: a partial refund still fits after it applies, so a late copy would
    // move the money twice if the key did not stop it.
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_refunds", "die_after payment_refunds"])
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    let refunds = killed_at_every_point(
        &pool,
        Start::Captured,
        "refund",
        REFUND,
        &points,
        r#"{"seq":3,"from":"captured","to":"captured","refundable":5000}"#,
    )
    .await?;
    assert_eq!(
        balances(&pool, merchant_id(1)).await?,
        [
            refunds * (AUTHORIZED - 3_000),
            refunds * (FEE - AUTHORIZED + 3_000),
            -refunds * FEE
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn a_void_killed_anywhere_is_a_503_and_the_retry_settles_it(pool: PgPool) -> TestResult {
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    killed_at_every_point(
        &pool,
        Start::Authorized,
        "void",
        "",
        &points,
        r#"{"seq":2,"from":"authorized","to":"voided","refundable":0}"#,
    )
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_cancel_killed_anywhere_is_a_503_and_the_retry_settles_it(pool: PgPool) -> TestResult {
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    killed_at_every_point(
        &pool,
        Start::Created,
        "cancel",
        "",
        &points,
        r#"{"seq":1,"from":"created","to":"voided","refundable":0}"#,
    )
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_refusal_killed_as_it_is_stored_is_decided_again(pool: PgPool) -> TestResult {
    // A client that was never answered has been told nothing, so deciding again is right, and
    // it is only a stored refusal that is final. Here the payment moves on in between, so the
    // same request now applies.
    let (pools, tenant) = merchant_side(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let kills = Kills {
        pool: &pool,
        session: &tenant,
        state: AppState::new(
            tenant.clone(),
            pools.system.clone(),
            pools.operator.digest(),
            unused_webhooks(),
        ),
    };
    for (n, point) in (1_u32..).zip(KEY_POINTS) {
        let payment = Start::Created.payment(&pools, merchant, n).await?;
        let path = format!("/payments/{payment}/capture");
        let key = format!("capture-{n}");
        kills.killed(point, &path, &secret, &key, CAPTURE).await?;

        let me = Tenant::new(&pools.tenant, merchant);
        authorize(me, &authorize_command(n, payment)).await?;

        let settled = kills.settled(point, &path, &secret, &key, CAPTURE).await?;
        assert_eq!(
            settled.first.status,
            StatusCode::OK,
            "{point}: {:?}",
            settled.first
        );
        assert_eq!(steps(&pool, payment).await?, 3, "{point}");
    }
    assert_books_are_whole(&pool).await?;
    tenant.close().await;
    pools.close().await;
    Ok(())
}

/// Ends the session that waits on this kind of lock. Pass the test's own pool: only a superuser
/// may signal another session.
async fn kill_the_waiter(pool: &PgPool, wait_event: &str) -> sqlx::Result<()> {
    wait_for_lock_wait(pool, wait_event).await?;
    sqlx::query_scalar!(
        r#"SELECT pg_terminate_backend(pid) AS "sent!" FROM pg_stat_activity
            WHERE datname = current_database()
              AND wait_event_type = 'Lock' AND wait_event = $1"#,
        wait_event
    )
    .fetch_all(pool)
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_capture_killed_while_it_waits_for_the_lock_can_be_sent_again(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = Start::Authorized.payment(&pools, merchant, 1).await?;
    let path = format!("/payments/{payment}/capture");
    let mut holder = pool.begin().await?;
    take_lock(&mut holder, payment).await?;

    let request = post(&path, &secret, "k", CAPTURE);
    let (reply, killed) = join(
        send(router(pools.state()), request),
        kill_the_waiter(&pool, "advisory"),
    )
    .await;
    killed?;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    assert_eq!(reply.body, Problem::Unavailable.body());
    holder.rollback().await?;
    assert_eq!(steps(&pool, payment).await?, 2);

    let reply = send(router(pools.state()), post(&path, &secret, "k", CAPTURE)).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(
        balances(&pool, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    assert_books_are_whole(&pool).await?;
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_create_killed_while_it_waits_for_its_key_can_be_sent_again(pool: PgPool) -> TestResult {
    // A create has no lock to queue on, so it waits in the key's unique index, behind a first
    // attempt that has not committed and then rolls back.
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let fingerprint = Fingerprint::of("POST", "/payments", CREATE.as_bytes());
    let mut first = tenant_transaction(&pool, merchant.get()).await?;
    sqlx::query!(
        "INSERT INTO idempotency_records (merchant_id, key, fingerprint, status, body) \
         VALUES ($1, 'k', $2, 201, '{}')",
        merchant.get(),
        fingerprint.as_bytes()
    )
    .execute(&mut *first)
    .await?;

    let request = post("/payments", &secret, "k", CREATE);
    let (reply, killed) = join(
        send(router(pools.state()), request),
        kill_the_waiter(&pool, "transactionid"),
    )
    .await;
    killed?;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    first.rollback().await?;
    assert_eq!(rows(&pool).await?.0[0], 0);

    let reply = send(
        router(pools.state()),
        post("/payments", &secret, "k", CREATE),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(rows(&pool).await?.0[0], 1);
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_capture_whose_commit_commits_and_loses_its_session_gets_its_answer(
    pool: PgPool,
) -> TestResult {
    // The hardest point. The server loses its session for a transaction that committed, so it
    // cannot tell, and it answers 503. Only the key's record says what happened: the retry
    // finds the answer and moves nothing. A kill that lands too early leaves nothing, and the
    // retry applies. Every attempt, hit or miss, must end with the money moved exactly once.
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let app = slow_commit_pool(&pool, Role::Tenant).await?;
    let state = AppState::new(
        app.clone(),
        pools.system.clone(),
        pools.operator.digest(),
        unused_webhooks(),
    );
    let answer = r#"{"seq":2,"from":"authorized","to":"captured","refundable":8000}"#;
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let payment = Start::Authorized.payment(&pools, merchant, n).await?;
        let path = format!("/payments/{payment}/capture");
        let key = format!("capture-{n}");
        // The last kill may have ended the session, and a new one has a new ID.
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let request = post(&path, &secret, &key, CAPTURE);
        let (reply, killed) = join(
            send(router(state.clone()), request),
            kill_during_commit(&pool, pid),
        )
        .await;
        killed?;
        if reply.status == StatusCode::SERVICE_UNAVAILABLE {
            if steps(&pool, payment).await? == 3 {
                hits += 1;
            }
            let reply = send(router(state.clone()), post(&path, &secret, &key, CAPTURE)).await;
            assert_eq!(reply.status, StatusCode::OK, "attempt {n}: {reply:?}");
            assert_eq!(reply.body, answer, "attempt {n}");
        } else {
            assert_eq!(reply.status, StatusCode::OK, "attempt {n}: {reply:?}");
            assert_eq!(reply.body, answer, "attempt {n}");
        }
        assert_eq!(steps(&pool, payment).await?, 3, "attempt {n}");
    }
    assert!(
        hits > 0,
        "no kill landed inside a commit in {ATTEMPTS} attempts"
    );
    let attempts = i64::from(ATTEMPTS);
    assert_eq!(
        balances(&pool, merchant).await?,
        [
            attempts * AUTHORIZED,
            attempts * (FEE - AUTHORIZED),
            -attempts * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    app.close().await;
    pools.close().await;
    Ok(())
}

/// The pools, a pool of one session as the system role, and the crash points.
async fn operator_side(pool: &PgPool) -> Result<(Pools, PgPool), Box<dyn Error>> {
    let pools = Pools::login(pool).await?;
    install_crash_points(pool).await?;
    let system = runtime_pool_of_one(pool).await?;
    Ok((pools, system))
}

#[sqlx::test]
async fn a_registration_killed_anywhere_is_a_503_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    let (pools, system) = operator_side(&pool).await?;
    let kills = Kills {
        pool: &pool,
        session: &system,
        state: AppState::new(
            pools.tenant.clone(),
            system.clone(),
            pools.operator.digest(),
            unused_webhooks(),
        ),
    };
    let points = ["die_before merchants", "die_after merchants"]
        .into_iter()
        .chain(OPERATOR_KEY_POINTS)
        .chain([OPERATOR_AT_COMMIT]);
    let mut registered = 0;
    for (n, point) in (1_u32..).zip(points) {
        let key = format!("register-{n}");
        let settled = kills
            .killed_then_settled(point, "/merchants", &pools.operator, &key, REGISTER)
            .await?;
        assert_eq!(
            settled.first.status,
            StatusCode::CREATED,
            "{point}: {:?}",
            settled.first
        );
        assert_eq!(settled.late.body, settled.first.body, "{point}");
        registered += 1;
    }
    assert_eq!(rows(&pool).await?.1, [registered, 0, 0, registered]);
    system.close().await;
    pools.close().await;
    Ok(())
}

/// Whether this secret opens a merchant's routes: reading no payment is a 404 if it does, and a
/// 401 if it does not.
async fn opens(pools: &Pools, value: &str) -> bool {
    let path = format!("/payments/{}", payment_id(999));
    let reply = send(router(pools.state()), get_with_authorization(&path, value)).await;
    reply.status == StatusCode::NOT_FOUND
}

#[sqlx::test]
async fn an_issue_killed_anywhere_is_a_503_and_the_retry_shows_the_secret_once(
    pool: PgPool,
) -> TestResult {
    let (pools, system) = operator_side(&pool).await?;
    let (merchant, _) = a_merchant(&pools, 1).await?;
    let kills = Kills {
        pool: &pool,
        session: &system,
        state: AppState::new(
            pools.tenant.clone(),
            system.clone(),
            pools.operator.digest(),
            unused_webhooks(),
        ),
    };
    let path = format!("/merchants/{merchant}/api-keys");
    let points = ["die_before api_keys", "die_after api_keys"]
        .into_iter()
        .chain(OPERATOR_KEY_POINTS)
        .chain([OPERATOR_AT_COMMIT]);
    for (n, point) in (1_u32..).zip(points) {
        let key = format!("killed-issue-{n}");
        let settled = kills
            .killed_then_settled(point, &path, &pools.operator, &key, "")
            .await?;
        assert_eq!(
            settled.first.status,
            StatusCode::CREATED,
            "{point}: {:?}",
            settled.first
        );
        // The attempt that died left nothing, so this answer is the first, and shows it.
        let first: serde_json::Value = serde_json::from_str(&settled.first.body)?;
        let secret = first["secret"].as_str().expect("the secret, once");
        assert!(opens(&pools, &format!("Bearer {secret}")).await, "{point}");
        assert!(!settled.late.body.contains("pck_"), "{point}");
    }
    system.close().await;
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_revocation_killed_anywhere_is_a_503_that_leaves_the_key_working_and_the_retry_settles_it(
    pool: PgPool,
) -> TestResult {
    let (pools, system) = operator_side(&pool).await?;
    let (merchant, _) = a_merchant(&pools, 1).await?;
    let kills = Kills {
        pool: &pool,
        session: &system,
        state: AppState::new(
            pools.tenant.clone(),
            system.clone(),
            pools.operator.digest(),
            unused_webhooks(),
        ),
    };
    let points = [
        "die_before api_key_revocations",
        "die_after api_key_revocations",
    ]
    .into_iter()
    .chain(OPERATOR_KEY_POINTS)
    .chain([OPERATOR_AT_COMMIT]);
    for (n, point) in (10_u32..).zip(points) {
        let secret = issued_key(&pools.system, merchant, n).await?;
        let path = format!("/api-keys/{}/revoke", api_key_id(n));
        let key = format!("revoke-{n}");
        kills
            .killed(point, &path, &pools.operator, &key, "")
            .await?;
        assert!(opens(&pools, &bearer(&secret)).await, "{point}");

        let settled = kills
            .settled(point, &path, &pools.operator, &key, "")
            .await?;
        assert_eq!(
            settled.first.status,
            StatusCode::OK,
            "{point}: {:?}",
            settled.first
        );
        assert_eq!(settled.late.body, settled.first.body, "{point}");
        assert!(!opens(&pools, &bearer(&secret)).await, "{point}");
    }
    system.close().await;
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_registration_whose_commit_commits_and_loses_its_session_gets_its_answer(
    pool: PgPool,
) -> TestResult {
    // The hardest point again, as the operator meets it. Every attempt, hit or miss, must end
    // with exactly one merchant, and with one answer, the same to every retry.
    let pools = Pools::login(&pool).await?;
    let app = slow_commit_pool(&pool, Role::System).await?;
    let state = AppState::new(
        pools.tenant.clone(),
        app.clone(),
        pools.operator.digest(),
        unused_webhooks(),
    );
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let key = format!("register-{n}");
        let before = rows(&pool).await?.1;
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let request = post("/merchants", &pools.operator, &key, REGISTER);
        let (reply, killed) = join(
            send(router(state.clone()), request),
            kill_during_commit(&pool, pid),
        )
        .await;
        killed?;
        let first = if reply.status == StatusCode::SERVICE_UNAVAILABLE {
            if rows(&pool).await?.1 != before {
                hits += 1;
            }
            let request = post("/merchants", &pools.operator, &key, REGISTER);
            send(router(state.clone()), request).await
        } else {
            reply
        };
        assert_eq!(first.status, StatusCode::CREATED, "attempt {n}: {first:?}");
        let request = post("/merchants", &pools.operator, &key, REGISTER);
        let again = send(router(state.clone()), request).await;
        assert_eq!(again.body, first.body, "attempt {n}");
        assert_eq!(
            rows(&pool).await?.1,
            [before[0] + 1, 0, 0, before[3] + 1],
            "attempt {n}"
        );
    }
    assert!(
        hits > 0,
        "no kill landed inside a commit in {ATTEMPTS} attempts"
    );
    app.close().await;
    pools.close().await;
    Ok(())
}
