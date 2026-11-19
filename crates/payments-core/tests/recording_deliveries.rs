// Author: Nicola Antonello

//! The relay's records (ADR-0053): a delivery's success and an endpoint's disablement, each
//! written once by the system role and committed through the one function of ADR-0014,
//! and a record killed at its insert, which writes nothing and is made again.

mod common;

use std::error::Error;

use common::outbox::{disable, event, insert_endpoint, new_payment};
use common::{
    delivery_id, endpoint_id, event_id, runtime_pool, runtime_transaction, wait_for_lock_wait,
};
use futures_util::future::join;
use payments_core::deliveries::{RecordError, record};
use payments_core::ids::{DeliveryId, EndpointId};
use payments_core::relay::{Effect, Reason};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// Where the endpoints are. No test here posts to them.
const URL: &str = "http://localhost:8080/webhooks";

/// Delivery `n` to a new merchant's endpoint `n`, committed. Returns both.
async fn delivery(system: &PgPool, n: u32) -> Result<(DeliveryId, EndpointId), Box<dyn Error>> {
    let (merchant, payment) = new_payment(system).await?;
    insert_endpoint(system, endpoint_id(n), merchant, URL).await?;
    event(
        system,
        merchant,
        payment,
        1,
        event_id(n),
        &[(delivery_id(n), endpoint_id(n))],
    )
    .await?;
    Ok((delivery_id(n), endpoint_id(n)))
}

async fn successes(pool: &PgPool, delivery: DeliveryId) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM webhook_delivery_successes WHERE delivery_id = $1"#,
        delivery.get()
    )
    .fetch_one(pool)
    .await
}

async fn reasons(pool: &PgPool, endpoint: EndpointId) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        "SELECT reason FROM webhook_endpoint_disablements WHERE endpoint_id = $1",
        endpoint.get()
    )
    .fetch_all(pool)
    .await
}

/// Ends the session that waits for another transaction to end. Pass the test's own pool:
/// only a superuser may signal another session.
async fn kill_the_waiter(pool: &PgPool) -> sqlx::Result<()> {
    wait_for_lock_wait(pool, "transactionid").await?;
    sqlx::query_scalar!(
        r#"SELECT pg_terminate_backend(pid) AS "sent!" FROM pg_stat_activity
            WHERE datname = current_database()
              AND wait_event_type = 'Lock' AND wait_event = 'transactionid'"#
    )
    .fetch_all(pool)
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_success_is_recorded_once(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (delivery, _) = delivery(&system, 1).await?;

    for _ in 0..2 {
        record(&system, Effect::Record(delivery)).await?;
    }
    assert_eq!(successes(&system, delivery).await?, 1);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_disablement_is_recorded_once_and_keeps_its_first_reason(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (_, gone) = delivery(&system, 1).await?;
    let (_, failing) = delivery(&system, 2).await?;
    let (_, by_the_operator) = delivery(&system, 3).await?;

    // Each of the relay's reasons, under the label that the database's check takes.
    record(&system, Effect::Disable(gone, Reason::Gone)).await?;
    record(&system, Effect::Disable(failing, Reason::Failing)).await?;
    // Again, for the other reason: the first one stays.
    record(&system, Effect::Disable(gone, Reason::Failing)).await?;
    record(&system, Effect::Disable(failing, Reason::Gone)).await?;
    // The operator disabled this endpoint first.
    disable(&system, by_the_operator, "operator").await?;
    record(&system, Effect::Disable(by_the_operator, Reason::Failing)).await?;

    assert_eq!(reasons(&system, gone).await?, ["gone"]);
    assert_eq!(reasons(&system, failing).await?, ["failing"]);
    assert_eq!(reasons(&system, by_the_operator).await?, ["operator"]);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_killed_at_its_insert_writes_nothing_and_is_made_again(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let (delivery, _) = delivery(&system, 1).await?;
    // Another transaction holds the success's key, so the record's insert waits for it,
    // and the record's session is killed while it waits.
    let mut holder = runtime_transaction(&pool).await?;
    sqlx::query!(
        "INSERT INTO webhook_delivery_successes (delivery_id) VALUES ($1)",
        delivery.get()
    )
    .execute(&mut *holder)
    .await?;

    let (recorded, killed) = join(
        record(&system, Effect::Record(delivery)),
        kill_the_waiter(&pool),
    )
    .await;
    killed?;
    assert!(
        matches!(recorded, Err(RecordError::Database(_))),
        "{recorded:?}"
    );
    holder.rollback().await?;
    assert_eq!(successes(&system, delivery).await?, 0);

    record(&system, Effect::Record(delivery)).await?;
    assert_eq!(successes(&system, delivery).await?, 1);
    system.close().await;
    Ok(())
}
