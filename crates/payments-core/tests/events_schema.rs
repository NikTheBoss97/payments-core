// Author: Nicola Antonello

//! Database tests for `payment_events`, `webhook_deliveries` and
//! `webhook_delivery_successes` (ADR-0049, ADR-0052, ADR-0053, ADR-0055), with the
//! guards of I7 and I10 in PostgreSQL (ADR-0054).
//!
//! Rows are written with raw SQL as the system role, `payments_app`, which runs the
//! catch-up and the relay, the way a buggy code path or a manual session could write
//! them. Each rejected write breaks a single rule. The test asserts the SQLSTATE and,
//! where PostgreSQL reports one, the constraint or trigger that rejects the write;
//! privilege errors carry none.

mod common;

use std::error::Error;

use common::payments::{
    NewPayment, Step, VERSION_4, back_to_savepoint, register_merchant, savepoint,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, runtime_transaction, sqlstate,
    tenant_login,
};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// The address of a merchant's handler, as the server would store it.
const URL: &str = "http://localhost:8080/webhooks";

/// The PSP's fact at this position of a payment's history. The table does not judge
/// the order of kinds (ADR-0037), so every fact here is an authorization. A fact
/// commits only with its event (ADR-0049, ADR-0057), so each test writes it in the
/// transaction of the event that announces it.
async fn insert_fact(conn: &mut PgConnection, payment: Uuid, position: i32) -> sqlx::Result<()> {
    let prev_position = position
        .checked_sub(1)
        .expect("a fact's position is at least 1")
        .max(1);
    sqlx::query!(
        "INSERT INTO payment_facts \
             (payment_id, position, prev_position, psp, psp_fact_id, kind, amount) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        payment,
        position,
        prev_position,
        "mock",
        format!("fct_{}", Uuid::now_v7().simple()),
        "authorize",
        100_i64
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A new merchant's payment, committed with its first step. Returns the merchant and
/// the payment.
async fn new_payment(pool: &PgPool) -> sqlx::Result<(Uuid, Uuid)> {
    let mut tx = runtime_transaction(pool).await?;
    let merchant = register_merchant(&mut tx).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    Step::first(payment.id).insert(&mut tx).await?;
    tx.commit().await?;
    Ok((merchant, payment.id))
}

async fn insert_endpoint(conn: &mut PgConnection, id: Uuid, merchant: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
        id,
        merchant,
        URL
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn insert_event(
    conn: &mut PgConnection,
    id: Uuid,
    merchant: Uuid,
    payment: Uuid,
    position: i32,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_events (id, merchant_id, payment_id, position) \
         VALUES ($1, $2, $3, $4)",
        id,
        merchant,
        payment,
        position
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn insert_delivery(
    conn: &mut PgConnection,
    id: Uuid,
    event: Uuid,
    endpoint: Uuid,
    merchant: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO webhook_deliveries (id, event_id, endpoint_id, merchant_id) \
         VALUES ($1, $2, $3, $4)",
        id,
        event,
        endpoint,
        merchant
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn succeed(conn: &mut PgConnection, delivery: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO webhook_delivery_successes (delivery_id) VALUES ($1)",
        delivery
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The PSP's first fact about a new payment, its event, an endpoint of its merchant and
/// the delivery between them, committed together as the catch-up writes them. Returns
/// the merchant, the event and the delivery.
async fn delivery(pool: &PgPool) -> sqlx::Result<(Uuid, Uuid, Uuid)> {
    let (merchant, payment) = new_payment(pool).await?;
    let (endpoint, event, delivery) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(pool).await?;
    insert_fact(&mut tx, payment, 1).await?;
    insert_endpoint(&mut tx, endpoint, merchant).await?;
    insert_event(&mut tx, event, merchant, payment, 1).await?;
    insert_delivery(&mut tx, delivery, event, endpoint, merchant).await?;
    tx.commit().await?;
    Ok((merchant, event, delivery))
}

#[sqlx::test]
async fn an_event_has_a_uuidv7_and_announces_one_fact_of_its_payment(pool: PgPool) -> TestResult {
    let (merchant, payment) = new_payment(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    insert_fact(&mut tx, payment, 1).await?;
    insert_fact(&mut tx, payment, 2).await?;

    for id in [Uuid::nil(), Uuid::max(), VERSION_4] {
        savepoint(&mut tx).await?;
        let err = insert_event(&mut tx, id, merchant, payment, 1)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "payment_events_id_check");
        back_to_savepoint(&mut tx).await?;
    }
    // A position that the payment's history does not hold.
    savepoint(&mut tx).await?;
    let err = insert_event(&mut tx, Uuid::now_v7(), merchant, payment, 3)
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_events_fact_fkey");
    back_to_savepoint(&mut tx).await?;

    let event = Uuid::now_v7();
    insert_event(&mut tx, event, merchant, payment, 1).await?;
    // A second event for one fact, and one event for a second fact.
    savepoint(&mut tx).await?;
    let err = insert_event(&mut tx, Uuid::now_v7(), merchant, payment, 1)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_events_fact_key");
    back_to_savepoint(&mut tx).await?;
    savepoint(&mut tx).await?;
    let err = insert_event(&mut tx, event, merchant, payment, 2)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_events_pkey");
    back_to_savepoint(&mut tx).await?;
    insert_event(&mut tx, Uuid::now_v7(), merchant, payment, 2).await?;
    tx.commit().await?;
    Ok(())
}

#[sqlx::test]
async fn an_events_merchant_is_its_payments(pool: PgPool) -> TestResult {
    // I7 (ADR-0054): an event names the merchant whose endpoints it goes to, so that
    // merchant must be its payment's.
    let (_, payment) = new_payment(&pool).await?;
    let (other, _) = new_payment(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    insert_fact(&mut tx, payment, 1).await?;
    for merchant in [other, Uuid::now_v7()] {
        savepoint(&mut tx).await?;
        let err = insert_event(&mut tx, Uuid::now_v7(), merchant, payment, 1)
            .await
            .unwrap_err();
        assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_events_payment_fkey");
        back_to_savepoint(&mut tx).await?;
    }
    Ok(())
}

#[sqlx::test]
async fn only_the_database_stamps_the_transaction_of_an_event_and_its_delivery(
    pool: PgPool,
) -> TestResult {
    let (merchant, payment) = new_payment(&pool).await?;
    let (endpoint, event, delivery) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_fact(&mut tx, payment, 1).await?;
    insert_endpoint(&mut tx, endpoint, merchant).await?;
    insert_event(&mut tx, event, merchant, payment, 1).await?;
    insert_delivery(&mut tx, delivery, event, endpoint, merchant).await?;
    let stamped = sqlx::query!(
        r#"SELECT pg_current_xact_id()::text AS "current!",
                  (SELECT xact_id::text FROM payment_events WHERE id = $1) AS "event!",
                  (SELECT xact_id::text FROM webhook_deliveries WHERE id = $2) AS "delivery!""#,
        event,
        delivery
    )
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!(
        (stamped.event.as_str(), stamped.delivery.as_str()),
        (stamped.current.as_str(), stamped.current.as_str())
    );
    tx.commit().await?;

    let mut app = runtime_connection(&pool).await?;
    for err in [
        // Not even the value that the default would choose (ADR-0052).
        sqlx::query!(
            "INSERT INTO payment_events (id, merchant_id, payment_id, position, xact_id) \
             VALUES ($1, $2, $3, 1, pg_current_xact_id())",
            Uuid::now_v7(),
            merchant,
            payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO webhook_deliveries (id, event_id, endpoint_id, merchant_id, xact_id) \
             VALUES ($1, $2, $3, $4, pg_current_xact_id())",
            Uuid::now_v7(),
            event,
            endpoint,
            merchant
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        // Only the database chooses a recorded_at (ADR-0012).
        sqlx::query!(
            "INSERT INTO payment_events (id, merchant_id, payment_id, position, recorded_at) \
             VALUES ($1, $2, $3, 1, now())",
            Uuid::now_v7(),
            merchant,
            payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO webhook_delivery_successes (delivery_id, recorded_at) \
             VALUES ($1, now())",
            delivery
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    Ok(())
}

#[sqlx::test]
async fn a_delivery_goes_only_to_an_endpoint_of_its_events_merchant(pool: PgPool) -> TestResult {
    // I7 (ADR-0054), each foreign key broken alone, inside the event's own transaction
    // so that the key to its transaction holds.
    let (a, payment) = new_payment(&pool).await?;
    let (b, _) = new_payment(&pool).await?;
    let (of_a, of_b) = (Uuid::now_v7(), Uuid::now_v7());
    let mut app = runtime_connection(&pool).await?;
    insert_endpoint(&mut app, of_a, a).await?;
    insert_endpoint(&mut app, of_b, b).await?;

    let event = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_fact(&mut tx, payment, 1).await?;
    insert_event(&mut tx, event, a, payment, 1).await?;
    // Another merchant's endpoint, under the event's merchant.
    savepoint(&mut tx).await?;
    let err = insert_delivery(&mut tx, Uuid::now_v7(), event, of_b, a)
        .await
        .unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "webhook_deliveries_endpoint_fkey",
    );
    back_to_savepoint(&mut tx).await?;
    // Another merchant's endpoint, under that merchant, which the event is not of.
    savepoint(&mut tx).await?;
    let err = insert_delivery(&mut tx, Uuid::now_v7(), event, of_b, b)
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "webhook_deliveries_event_fkey");
    back_to_savepoint(&mut tx).await?;
    // Its own merchant's endpoint.
    insert_delivery(&mut tx, Uuid::now_v7(), event, of_a, a).await?;
    tx.commit().await?;
    Ok(())
}

#[sqlx::test]
async fn a_delivery_is_written_in_its_events_transaction_or_not_at_all(pool: PgPool) -> TestResult {
    // I10 (ADR-0052): once a transaction has ended, the relay's mark may pass it, so a
    // delivery written later for its event could sit below the mark and never be sent.
    let (merchant, payment) = new_payment(&pool).await?;
    let endpoint = Uuid::now_v7();
    let mut app = runtime_connection(&pool).await?;
    insert_endpoint(&mut app, endpoint, merchant).await?;
    let first = Uuid::now_v7();
    let mut earlier = runtime_transaction(&pool).await?;
    insert_fact(&mut earlier, payment, 1).await?;
    insert_event(&mut earlier, first, merchant, payment, 1).await?;
    earlier.commit().await?;

    // In a later transaction, alone, and after an event of that transaction's own.
    let err = insert_delivery(&mut app, Uuid::now_v7(), first, endpoint, merchant)
        .await
        .unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "webhook_deliveries_transaction_fkey",
    );
    let second = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_fact(&mut tx, payment, 2).await?;
    insert_event(&mut tx, second, merchant, payment, 2).await?;
    savepoint(&mut tx).await?;
    let err = insert_delivery(&mut tx, Uuid::now_v7(), first, endpoint, merchant)
        .await
        .unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "webhook_deliveries_transaction_fkey",
    );
    back_to_savepoint(&mut tx).await?;

    // The delivery of this transaction's own event, even written inside a savepoint,
    // which keeps the top-level transaction's ID, unlike a row's xmin (ADR-0055).
    savepoint(&mut tx).await?;
    let delivery = Uuid::now_v7();
    insert_delivery(&mut tx, delivery, second, endpoint, merchant).await?;
    tx.commit().await?;
    let written = sqlx::query_scalar!(
        r#"SELECT d.xact_id = e.xact_id AS "same!"
             FROM webhook_deliveries d JOIN payment_events e ON e.id = d.event_id
            WHERE d.id = $1"#,
        delivery
    )
    .fetch_one(&mut app)
    .await?;
    assert!(written);
    Ok(())
}

#[sqlx::test]
async fn a_delivery_has_a_uuidv7_and_is_one_per_event_and_endpoint(pool: PgPool) -> TestResult {
    let (merchant, payment) = new_payment(&pool).await?;
    let (endpoint, other) = (Uuid::now_v7(), Uuid::now_v7());
    let mut app = runtime_connection(&pool).await?;
    insert_endpoint(&mut app, endpoint, merchant).await?;
    insert_endpoint(&mut app, other, merchant).await?;

    let (event, delivery) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_fact(&mut tx, payment, 1).await?;
    insert_event(&mut tx, event, merchant, payment, 1).await?;
    for id in [Uuid::nil(), Uuid::max(), VERSION_4] {
        savepoint(&mut tx).await?;
        let err = insert_delivery(&mut tx, id, event, endpoint, merchant)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "webhook_deliveries_id_check");
        back_to_savepoint(&mut tx).await?;
    }
    insert_delivery(&mut tx, delivery, event, endpoint, merchant).await?;
    // A second delivery of the event to one endpoint, and one delivery to a second.
    savepoint(&mut tx).await?;
    let err = insert_delivery(&mut tx, Uuid::now_v7(), event, endpoint, merchant)
        .await
        .unwrap_err();
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "webhook_deliveries_event_endpoint_key",
    );
    back_to_savepoint(&mut tx).await?;
    savepoint(&mut tx).await?;
    let err = insert_delivery(&mut tx, delivery, event, other, merchant)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "webhook_deliveries_pkey");
    back_to_savepoint(&mut tx).await?;
    // Every endpoint of the merchant gets a delivery of its own.
    insert_delivery(&mut tx, Uuid::now_v7(), event, other, merchant).await?;
    tx.commit().await?;
    Ok(())
}

#[sqlx::test]
async fn a_success_names_a_delivery_once(pool: PgPool) -> TestResult {
    let (_, _, delivery) = delivery(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    let err = succeed(&mut app, Uuid::now_v7()).await.unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "webhook_delivery_successes_delivery_fkey",
    );
    succeed(&mut app, delivery).await?;
    // The relay reads this as a success recorded already (ADR-0053).
    let err = succeed(&mut app, delivery).await.unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "webhook_delivery_successes_pkey");
    Ok(())
}

#[sqlx::test]
async fn a_merchants_role_neither_reads_nor_writes_events_or_deliveries(
    pool: PgPool,
) -> TestResult {
    // A merchant reads the state through the API, and its endpoints receive its events
    // (ADR-0049), so its credential has no privilege on these tables, even for its own
    // rows (ADR-0054).
    let (merchant, event, delivery) = delivery(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        sqlx::query_scalar!("SELECT count(*) FROM payment_events")
            .fetch_one(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query_scalar!("SELECT count(*) FROM webhook_deliveries")
            .fetch_one(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query_scalar!("SELECT count(*) FROM webhook_delivery_successes")
            .fetch_one(&mut tenant)
            .await
            .unwrap_err(),
        insert_event(&mut tenant, Uuid::now_v7(), merchant, Uuid::now_v7(), 1)
            .await
            .unwrap_err(),
        insert_delivery(&mut tenant, Uuid::now_v7(), event, Uuid::now_v7(), merchant)
            .await
            .unwrap_err(),
        succeed(&mut tenant, delivery).await.unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    tenant.close().await?;
    Ok(())
}

#[sqlx::test]
async fn neither_role_changes_an_event_a_delivery_or_a_success(pool: PgPool) -> TestResult {
    let (_, _, delivery) = delivery(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    succeed(&mut app, delivery).await?;
    let mut tenant = tenant_login(&pool).await?;
    for conn in [&mut app, &mut tenant] {
        for err in [
            sqlx::query!("UPDATE payment_events SET position = position")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_events")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_events CASCADE")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE webhook_deliveries SET endpoint_id = endpoint_id")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM webhook_deliveries")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE webhook_deliveries CASCADE")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE webhook_delivery_successes SET delivery_id = delivery_id")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM webhook_delivery_successes")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE webhook_delivery_successes")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
        }
    }
    tenant.close().await?;
    Ok(())
}

#[sqlx::test]
async fn events_deliveries_and_successes_are_append_only_even_for_the_owner(
    pool: PgPool,
) -> TestResult {
    // An event or a delivery taken back would lose what a merchant is owed after its
    // fact committed (I10), and a success taken back would make the relay send the
    // event again.
    let (_, _, delivery) = delivery(&pool).await?;
    let mut owner = pool.acquire().await?;
    succeed(&mut owner, delivery).await?;

    let table = "payment_events";
    let update = sqlx::query!("UPDATE payment_events SET position = position")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "payment_events_append_only", table);
    let delete = sqlx::query!("DELETE FROM payment_events")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "payment_events_append_only", table);
    // CASCADE, because a foreign key refers to this table: without it, PostgreSQL
    // fails with 0A000 before any trigger runs.
    let truncate = sqlx::query!("TRUNCATE payment_events CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "payment_events_no_truncate", table);

    let table = "webhook_deliveries";
    let update = sqlx::query!("UPDATE webhook_deliveries SET endpoint_id = endpoint_id")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "webhook_deliveries_append_only", table);
    let delete = sqlx::query!("DELETE FROM webhook_deliveries")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "webhook_deliveries_append_only", table);
    let truncate = sqlx::query!("TRUNCATE webhook_deliveries CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "webhook_deliveries_no_truncate",
        table,
    );

    let table = "webhook_delivery_successes";
    let update = sqlx::query!("UPDATE webhook_delivery_successes SET delivery_id = delivery_id")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "webhook_delivery_successes_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM webhook_delivery_successes")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "webhook_delivery_successes_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE webhook_delivery_successes")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "webhook_delivery_successes_no_truncate",
        table,
    );
    Ok(())
}
