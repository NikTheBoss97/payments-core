// Author: Nicola Antonello

//! Recording the PSP's notifications (ADR-0031, ADR-0032, ADR-0043): once, by the PSP
//! and the event ID, in a short transaction of the system role's.
//!
//! A copy is never an error: one that committed first refuses the insert with a `23505`,
//! and the copy is found. These tests hold a copy's transaction open, so that the
//! recording waits on its key, and then commit it or roll it back.

mod common;

use std::error::Error;

use common::crashes::{NOTIFICATION_AT_COMMIT, install_crash_points, plan_death};
use common::payments::{create_command, fee_rate};
use common::{
    INSUFFICIENT_PRIVILEGE, merchant_id, runtime_pool, runtime_pool_of_one, sqlstate, tenant_pool,
    test_uuid, wait_for_lock_wait,
};
use payments_core::accounts::Psp;
use payments_core::db::CommitError;
use payments_core::ids::PaymentId;
use payments_core::notifications::{Notification, RecordError, Recorded, record};
use payments_core::payments::{create_payment, register_merchant};
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// A notification's row: its PSP, its event ID and the payment that it names.
type Row = (String, String, Uuid);

/// A payment that the service holds.
async fn held_payment(system: &PgPool, n: u32) -> Result<PaymentId, Box<dyn Error>> {
    let merchant = merchant_id(n);
    register_merchant(system, merchant, fee_rate()).await?;
    let command = create_command(n, merchant);
    create_payment(system, &command).await?;
    Ok(command.id)
}

fn notification(event_id: &str, payment: Uuid) -> Notification {
    Notification {
        event_id: event_id.to_owned(),
        payment,
    }
}

async fn rows(pool: &PgPool) -> sqlx::Result<Vec<Row>> {
    let rows =
        sqlx::query!("SELECT psp, event_id, payment_id FROM psp_notifications ORDER BY event_id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.psp, row.event_id, row.payment_id))
        .collect())
}

fn row(event_id: &str, payment: Uuid) -> Row {
    ("mock".to_owned(), event_id.to_owned(), payment)
}

#[sqlx::test]
async fn a_notification_is_recorded_once_with_its_psp_its_event_and_its_payment(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let payment = held_payment(&system, 1).await?;
    let recorded = record(&system, Psp::Mock, &notification("msg_1", payment.get())).await?;
    assert_eq!(recorded, Recorded::Known(payment));
    assert_eq!(rows(&pool).await?, [row("msg_1", payment.get())]);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_copy_is_recorded_once_and_names_the_payment_of_the_first(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let first = held_payment(&system, 1).await?;
    let other = held_payment(&system, 2).await?;
    let copy = notification("msg_1", first.get());
    assert_eq!(
        record(&system, Psp::Mock, &copy).await?,
        Recorded::Known(first)
    );
    assert_eq!(
        record(&system, Psp::Mock, &copy).await?,
        Recorded::Known(first)
    );
    // A copy that names another payment, which only a broken PSP would send, keeps the
    // first: what was recorded is what the service acts on.
    let changed = notification("msg_1", other.get());
    assert_eq!(
        record(&system, Psp::Mock, &changed).await?,
        Recorded::Known(first)
    );
    assert_eq!(rows(&pool).await?, [row("msg_1", first.get())]);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_copy_that_commits_first_is_found_after_its_key_refuses_the_insert(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool_of_one(&pool).await?;
    let first = held_payment(&system, 1).await?;
    let other = held_payment(&system, 2).await?;
    // The copy's transaction holds its row, uncommitted.
    let mut copy = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO psp_notifications (psp, event_id, payment_id) VALUES ('mock', 'msg_1', $1)",
        first.get()
    )
    .execute(&mut *copy)
    .await?;
    let recording = {
        let system = system.clone();
        tokio::spawn(async move {
            record(&system, Psp::Mock, &notification("msg_1", other.get())).await
        })
    };
    // The insert waits on the copy's transaction, which then commits: the key refuses
    // the insert, and the copy is found on the same one connection.
    wait_for_lock_wait(&pool, "transactionid").await?;
    copy.commit().await?;
    assert_eq!(recording.await??, Recorded::Known(first));
    assert_eq!(rows(&pool).await?, [row("msg_1", first.get())]);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_copy_that_rolls_back_leaves_the_key_to_the_one_waiting(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    let first = held_payment(&system, 1).await?;
    let other = held_payment(&system, 2).await?;
    let mut copy = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO psp_notifications (psp, event_id, payment_id) VALUES ('mock', 'msg_1', $1)",
        first.get()
    )
    .execute(&mut *copy)
    .await?;
    let recording = {
        let system = system.clone();
        tokio::spawn(async move {
            record(&system, Psp::Mock, &notification("msg_1", other.get())).await
        })
    };
    wait_for_lock_wait(&pool, "transactionid").await?;
    copy.rollback().await?;
    assert_eq!(recording.await??, Recorded::Known(other));
    assert_eq!(rows(&pool).await?, [row("msg_1", other.get())]);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_notification_about_a_payment_that_the_service_does_not_hold_is_kept(
    pool: PgPool,
) -> TestResult {
    let system = runtime_pool(&pool).await?;
    // A UUIDv7 that no payment has, and a UUIDv4, which no payment can have.
    let unknown = test_uuid(7);
    let random = Uuid::parse_str("4a7f5e3c-2b1d-4c9e-8f6a-0d1e2f3a4b5c")?;
    for (event_id, payment) in [("msg_1", unknown), ("msg_2", random)] {
        assert_eq!(
            record(&system, Psp::Mock, &notification(event_id, payment)).await?,
            Recorded::Unknown(payment)
        );
        // A copy of it finds the same.
        assert_eq!(
            record(&system, Psp::Mock, &notification(event_id, payment)).await?,
            Recorded::Unknown(payment)
        );
    }
    assert_eq!(
        rows(&pool).await?,
        [row("msg_1", unknown), row("msg_2", random)]
    );
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_merchants_role_cannot_record_a_notification(pool: PgPool) -> TestResult {
    let tenant = tenant_pool(&pool).await?;
    let err = record(&tenant, Psp::Mock, &notification("msg_1", test_uuid(1)))
        .await
        .unwrap_err();
    let RecordError::Database(source) = err else {
        panic!("not a failed statement: {err:?}");
    };
    assert_eq!(sqlstate(source), INSUFFICIENT_PRIVILEGE);
    assert_eq!(rows(&pool).await?, []);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_commit_that_dies_is_not_confirmed_and_a_copy_records_it_once(
    pool: PgPool,
) -> TestResult {
    install_crash_points(&pool).await?;
    let system = runtime_pool_of_one(&pool).await?;
    let payment = held_payment(&system, 1).await?;
    let sent = notification("msg_1", payment.get());
    plan_death(&system, NOTIFICATION_AT_COMMIT).await?;
    let err = record(&system, Psp::Mock, &sent).await.unwrap_err();
    assert!(
        matches!(err, RecordError::NotConfirmed(CommitError::Unknown(_))),
        "{err:?}"
    );
    // It died in the deferred checks, before the commit record, so nothing was kept, and
    // the copy that the PSP sends after the 503 records it on a new session.
    assert_eq!(rows(&pool).await?, []);
    assert_eq!(
        record(&system, Psp::Mock, &sent).await?,
        Recorded::Known(payment)
    );
    assert_eq!(rows(&pool).await?, [row("msg_1", payment.get())]);
    system.close().await;
    Ok(())
}
