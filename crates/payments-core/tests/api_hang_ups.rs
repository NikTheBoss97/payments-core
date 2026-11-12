// Author: Nicola Antonello

//! A client that hangs up in the middle of a real command (ADR-0014, ADR-0023).
//!
//! hyper drops the future that it holds when its client goes away, and a command running
//! in that future would be dropped with it, which is a lost `COMMIT`. In these tests the
//! command is waiting on something that the test holds, so it is certain to be in flight
//! when the client goes. It must land anyway, once, and a retry must get its answer.

mod common;

use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use common::api::{
    Pools, a_merchant, a_payment, accepted, bearer, payments_stored, post, records, send,
};
use common::crashes::balances;
use common::payments::{AUTHORIZED, FEE, requests_of, take_lock};
use common::psp::{applied, caught_up, sender_of, step};
use common::{tenant_transaction, wait_for_lock_wait};
use mock_psp::MockPsp;
use payments_core::api::router;
use payments_core::idempotency::Fingerprint;
use payments_core::money::PositiveAmount;
use payments_core::requests::Ask;
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};

type TestResult = Result<(), Box<dyn Error>>;

/// Signals when it is dropped.
struct SignalOnDrop(Arc<Notify>);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Outside the real router, the future that hyper holds: it is dropped when hyper has
/// noticed that the client went away. A test waits for that instead of sleeping.
async fn reports_the_hang_up(
    State(hung_up): State<Arc<Notify>>,
    request: Request,
    next: Next,
) -> Response {
    let _reports = SignalOnDrop(hung_up);
    next.run(request).await
}

/// Serves the real router on a port of its own, until the runtime ends with the test.
async fn serve(router: Router, hung_up: &Arc<Notify>) -> Result<SocketAddr, Box<dyn Error>> {
    let router = router.layer(middleware::from_fn_with_state(
        hung_up.clone(),
        reports_the_hang_up,
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(address)
}

/// Fails the test rather than hanging it, if something that must happen does not.
async fn within<T>(what: &str, happens: impl Future<Output = T>) -> T {
    timeout(Duration::from_secs(10), happens)
        .await
        .unwrap_or_else(|_| panic!("{what} did not happen within 10 seconds"))
}

/// Polls until this holds. Nothing announces that a command, cut off from its client, has
/// finished, so there is no signal to wait for.
async fn until(what: &str, holds: impl AsyncFn() -> sqlx::Result<bool>) -> TestResult {
    within(what, async {
        while !holds().await? {
            sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    })
    .await
}

#[sqlx::test]
async fn a_capture_whose_client_hangs_up_still_lands_once_and_a_retry_gets_its_answer(
    pool: PgPool,
) -> TestResult {
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let payment = a_payment(&pools, merchant, 1).await?;
    let fake = MockPsp::start().await?;
    let mut sender = sender_of(&fake);
    let me = Tenant::new(&pools.tenant, merchant);
    let ask = Ask::Authorize(PositiveAmount::new(AUTHORIZED)?);
    applied(&fake, &mut sender, me, &pools.system, payment, ask).await;
    let path = format!("/payments/{payment}/capture");
    let body = r#"{"amount":8000}"#;

    let hung_up = Arc::new(Notify::new());
    let address = serve(router(pools.state()), &hung_up).await?;

    // The test holds the payment's lock, so the capture waits, as it would behind another
    // command. It cannot have reached its COMMIT.
    let mut holder = pool.begin().await?;
    take_lock(&mut holder, payment).await?;
    let mut client = TcpStream::connect(address).await?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: test\r\nAuthorization: {}\r\n\
         Idempotency-Key: \"k\"\r\nContent-Length: {}\r\n\r\n{body}",
        bearer(&secret),
        body.len()
    );
    client.write_all(request.as_bytes()).await?;
    wait_for_lock_wait(&pool, "advisory").await?;

    drop(client);
    within("hyper noticing the hang-up", hung_up.notified()).await;
    assert_eq!(
        requests_of(&pool, payment).await?,
        1,
        "it ran before the lock"
    );

    // Only now can the command go on, with nobody left to answer.
    holder.rollback().await?;
    until("the capture landing", || async {
        Ok(requests_of(&pool, payment).await? == 2)
    })
    .await?;

    // The client's retry finds the answer that nobody received, and records nothing more.
    let reply = send(
        router(pools.state()),
        post(&path, &secret, "k", body.to_owned()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{reply:?}");
    let capture = accepted(&reply, payment);
    assert_eq!(requests_of(&pool, payment).await?, 2);

    // Through the PSP, the one capture moves the money once.
    step(&mut sender, &pools.system).await;
    fake.approve(capture);
    caught_up(&pools.system, &fake, payment).await;
    assert_eq!(
        balances(&pools.system, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    pools.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_create_whose_client_hangs_up_still_lands_once_and_a_retry_gets_its_answer(
    pool: PgPool,
) -> TestResult {
    // A create has no lock, so it waits in the key's unique index instead, behind a first
    // attempt that has not committed (ADR-0026). That attempt then rolls back.
    let pools = Pools::login(&pool).await?;
    let (merchant, secret) = a_merchant(&pools, 1).await?;
    let body = r#"{"currency":"EUR","amount":10000}"#;
    let fingerprint = Fingerprint::of("POST", "/payments", body.as_bytes());

    let hung_up = Arc::new(Notify::new());
    let address = serve(router(pools.state()), &hung_up).await?;

    let mut first = tenant_transaction(&pool, merchant.get()).await?;
    sqlx::query!(
        "INSERT INTO idempotency_records (merchant_id, key, fingerprint, status, body) \
         VALUES ($1, 'k', $2, 201, '{}')",
        merchant.get(),
        fingerprint.as_bytes()
    )
    .execute(&mut *first)
    .await?;

    let mut client = TcpStream::connect(address).await?;
    let request = format!(
        "POST /payments HTTP/1.1\r\nHost: test\r\nAuthorization: {}\r\n\
         Idempotency-Key: \"k\"\r\nContent-Length: {}\r\n\r\n{body}",
        bearer(&secret),
        body.len()
    );
    client.write_all(request.as_bytes()).await?;
    wait_for_lock_wait(&pool, "transactionid").await?;

    drop(client);
    within("hyper noticing the hang-up", hung_up.notified()).await;
    assert_eq!(payments_stored(&pool).await?, 0);

    first.rollback().await?;
    until("the create landing", || async {
        Ok(records(&pool).await? == 1)
    })
    .await?;
    assert_eq!(payments_stored(&pool).await?, 1);

    let reply = send(
        router(pools.state()),
        post("/payments", &secret, "k", body.to_owned()),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(payments_stored(&pool).await?, 1);
    pools.close().await;
    Ok(())
}

/// How many merchants are stored. Pass the test's own pool.
async fn merchants(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM merchants"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn a_registration_whose_client_hangs_up_still_lands_once_and_a_retry_gets_its_answer(
    pool: PgPool,
) -> TestResult {
    // The operator's routes run under the one middleware too. Like a create, a registration
    // has no lock, so it waits in its key's unique index, behind an attempt that then rolls
    // back.
    let pools = Pools::login(&pool).await?;
    let body = r#"{"fee_bps":250}"#;
    let fingerprint = Fingerprint::of("POST", "/merchants", body.as_bytes());

    let hung_up = Arc::new(Notify::new());
    let address = serve(router(pools.state()), &hung_up).await?;

    let mut first = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO operator_idempotency_records (key, fingerprint, status, body) \
         VALUES ('k', $1, 201, '{}')",
        fingerprint.as_bytes()
    )
    .execute(&mut *first)
    .await?;

    let mut client = TcpStream::connect(address).await?;
    let request = format!(
        "POST /merchants HTTP/1.1\r\nHost: test\r\nAuthorization: {}\r\n\
         Idempotency-Key: \"k\"\r\nContent-Length: {}\r\n\r\n{body}",
        bearer(&pools.operator),
        body.len()
    );
    client.write_all(request.as_bytes()).await?;
    wait_for_lock_wait(&pool, "transactionid").await?;

    drop(client);
    within("hyper noticing the hang-up", hung_up.notified()).await;
    assert_eq!(merchants(&pool).await?, 0);

    first.rollback().await?;
    until("the registration landing", || async {
        Ok(merchants(&pool).await? == 1)
    })
    .await?;

    let reply = send(
        router(pools.state()),
        post("/merchants", &pools.operator, "k", body),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{reply:?}");
    assert_eq!(merchants(&pool).await?, 1);
    pools.close().await;
    Ok(())
}
