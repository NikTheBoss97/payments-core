// Author: Nicola Antonello

//! Database tests for the step records: `payment_authorizations`,
//! `payment_captures` and `payment_refunds` (ADR-0017).
//!
//! Rows are written with raw SQL, bypassing the domain types, the way a buggy
//! code path or a manual session could write them. They run as the runtime
//! role, `payments_app`, unless a test says otherwise. Each rejected write
//! breaks a single rule. The test asserts the SQLSTATE and, where PostgreSQL
//! reports one, the constraint or trigger that rejects the write; privilege
//! errors carry none. Writes that must succeed run in a transaction that rolls
//! back, so that the deferred checks do not fire unless a test wants them to.

mod common;

use common::payments::{
    AUTHORIZED, FEE, NO_RECORD, REQUESTED, STEP_COMMIT_CHECK, Step, authorized_payment,
    back_to_savepoint, captured_payment, insert_authorization, insert_capture, insert_refund,
    open_books, payment_with_first_step, refund_step, savepoint, write_posting,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, runtime_transaction, sqlstate,
};
use payments_core::db::{CommitError, commit};
use payments_core::lifecycle::PaymentStatus;
use sqlx::PgPool;
use sqlx::postgres::PgQueryResult;
use uuid::Uuid;

#[sqlx::test]
async fn a_whole_lifecycle_commits_and_its_newest_step_is_its_state(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let captured = captured_payment(&mut tx, &books).await?;
    let partial = refund_step(&mut tx, &books, &captured, "captured", 3_000).await?;
    let settled = partial.then("settle", "settled", 0, partial.refundable);
    settled.insert_with_fact(&mut tx).await?;
    refund_step(&mut tx, &books, &settled, "refunded", 5_000).await?;
    tx.commit().await?;

    let newest = sqlx::query_scalar!(
        "SELECT to_status FROM payment_transitions \
         WHERE payment_id = $1 ORDER BY seq DESC LIMIT 1",
        captured.payment_id
    )
    .fetch_one(&mut app)
    .await?;
    assert_eq!(PaymentStatus::parse(&newest), Ok(PaymentStatus::Refunded));
    Ok(())
}

#[sqlx::test]
async fn a_step_that_carries_data_needs_its_record(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let table = "payment_transitions";

    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    first
        .then("authorize", "authorized", AUTHORIZED, 0)
        .insert_with_fact(&mut tx)
        .await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, table);

    let mut tx = runtime_transaction(&pool).await?;
    let authorized = authorized_payment(&mut tx, &books).await?;
    authorized
        .then("capture", "captured", AUTHORIZED, AUTHORIZED)
        .insert_with_fact(&mut tx)
        .await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, table);

    // The library reads this code as a proven rollback (ADR-0014).
    let mut tx = runtime_transaction(&pool).await?;
    let captured = captured_payment(&mut tx, &books).await?;
    captured
        .then("refund", "captured", 1, AUTHORIZED - 1)
        .insert_with_fact(&mut tx)
        .await?;
    let CommitError::Rejected(source) = commit(tx).await.unwrap_err() else {
        panic!("expected a proven rollback");
    };
    assert_guard(source, NO_RECORD, STEP_COMMIT_CHECK, table);

    let stored: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 0);
    Ok(())
}

#[sqlx::test]
async fn a_step_without_data_needs_no_record(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    for (kind, to_status) in [("decline", "failed"), ("cancel", "voided")] {
        let mut tx = runtime_transaction(&pool).await?;
        let first = payment_with_first_step(&mut tx).await?;
        let step = first.then(kind, to_status, 0, 0);
        // A cancel is the merchant's own step, and a decline comes with its fact.
        if kind == "cancel" {
            step.insert(&mut tx).await?;
        } else {
            step.insert_with_fact(&mut tx).await?;
        }
        tx.commit().await?;
    }
    for kind in ["void", "expire"] {
        let mut tx = runtime_transaction(&pool).await?;
        let authorized = authorized_payment(&mut tx, &books).await?;
        authorized
            .then(kind, "voided", 0, 0)
            .insert_with_fact(&mut tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn checking_early_refuses_a_step_at_once(pool: PgPool) -> sqlx::Result<()> {
    // A session can run the deferred check early, but cannot skip it.
    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    first
        .then("authorize", "authorized", AUTHORIZED, 0)
        .insert_with_fact(&mut tx)
        .await?;
    let err = sqlx::query!("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, "payment_transitions");
    Ok(())
}

#[sqlx::test]
async fn an_authorization_is_at_most_the_request_and_matches_its_step(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    let step = first.then("authorize", "authorized", AUTHORIZED, 0);
    step.insert(&mut tx).await?;

    // The amount and the request are compared on the row.
    let above = Step {
        amount: REQUESTED + 1,
        ..step
    };
    savepoint(&mut tx).await?;
    let err = insert_authorization(&mut tx, &above, REQUESTED)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, CHECK_VIOLATION, "payment_authorizations_amount_check");

    // A request that is not the payment's, the way to get past that CHECK.
    savepoint(&mut tx).await?;
    let err = insert_authorization(&mut tx, &step, AUTHORIZED)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_authorizations_payment_fkey",
    );

    // An amount that is not the step's.
    let other_amount = Step {
        amount: AUTHORIZED - 1,
        ..step
    };
    savepoint(&mut tx).await?;
    let err = insert_authorization(&mut tx, &other_amount, REQUESTED)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_authorizations_step_fkey",
    );

    // A step of another kind, with the right amount.
    let voided = Step {
        id: Uuid::now_v7(),
        seq: 2,
        prev_seq: 1,
        kind: "void",
        from_status: "authorized",
        to_status: "voided",
        amount: 0,
        ..step
    };
    voided.insert(&mut tx).await?;
    savepoint(&mut tx).await?;
    let err = insert_authorization(&mut tx, &voided, REQUESTED)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_authorizations_step_fkey",
    );
    tx.rollback().await
}

#[sqlx::test]
async fn a_capture_is_at_most_the_authorization_and_links_one_posting(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let authorized = authorized_payment(&mut tx, &books).await?;
    let step = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    step.insert(&mut tx).await?;
    let posting = write_posting(&mut tx, &books.capture_lines(AUTHORIZED, FEE)).await?;

    // The amount and the authorization are compared on the row.
    let above = Step {
        amount: AUTHORIZED + 1,
        ..step
    };
    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, &above, AUTHORIZED, FEE, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, CHECK_VIOLATION, "payment_captures_amount_check");

    // An authorization that is not the payment's, the way to get past that
    // CHECK.
    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, &step, REQUESTED, FEE, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_captures_authorization_fkey",
    );

    // A fee below zero or above the capture.
    for fee in [-1, AUTHORIZED + 1] {
        savepoint(&mut tx).await?;
        let err = insert_capture(&mut tx, &step, AUTHORIZED, fee, posting)
            .await
            .unwrap_err();
        back_to_savepoint(&mut tx).await?;
        assert_violation(err, CHECK_VIOLATION, "payment_captures_fee_check");
    }

    // An amount that is not the step's.
    let other_amount = Step {
        amount: AUTHORIZED - 1,
        ..step
    };
    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, &other_amount, AUTHORIZED, FEE, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_captures_step_fkey");

    // A posting that does not exist.
    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, &step, AUTHORIZED, FEE, Uuid::now_v7())
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_captures_ledger_transaction_fkey",
    );

    // A posting that another capture already links.
    insert_capture(&mut tx, &step, AUTHORIZED, FEE, posting).await?;
    let second_authorized = authorized_payment(&mut tx, &books).await?;
    let second = second_authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    second.insert(&mut tx).await?;
    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, &second, AUTHORIZED, FEE, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "payment_captures_ledger_transaction_id_key",
    );
    tx.rollback().await
}

#[sqlx::test]
async fn a_refund_matches_its_step_and_links_one_posting(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let captured = captured_payment(&mut tx, &books).await?;
    let step = captured.then("refund", "captured", 1_000, AUTHORIZED - 1_000);
    step.insert(&mut tx).await?;
    let posting = write_posting(&mut tx, &books.refund_lines(1_000)).await?;

    let other_amount = Step {
        amount: 999,
        ..step
    };
    savepoint(&mut tx).await?;
    let err = insert_refund(&mut tx, &other_amount, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_refunds_step_fkey");

    savepoint(&mut tx).await?;
    let err = insert_refund(&mut tx, &step, Uuid::now_v7())
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_refunds_ledger_transaction_fkey",
    );

    insert_refund(&mut tx, &step, posting).await?;
    let second = step.then("refund", "captured", 1_000, AUTHORIZED - 2_000);
    second.insert(&mut tx).await?;
    savepoint(&mut tx).await?;
    let err = insert_refund(&mut tx, &second, posting).await.unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "payment_refunds_ledger_transaction_id_key",
    );
    tx.rollback().await
}

#[sqlx::test]
async fn a_step_has_one_record(pool: PgPool) -> sqlx::Result<()> {
    // `LG004` asks only that a record exists, so these keys alone stop a second
    // one, which could link a second posting to one step. A second capture or
    // refund here has a posting of its own, so that no other key refuses it. A
    // second authorization also repeats its payment and amount, since its step
    // fixes both, and PostgreSQL reports the primary key.
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;

    let authorized = authorized_payment(&mut tx, &books).await?;
    savepoint(&mut tx).await?;
    let err = insert_authorization(&mut tx, &authorized, REQUESTED)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, UNIQUE_VIOLATION, "payment_authorizations_pkey");

    let captured = captured_payment(&mut tx, &books).await?;
    let posting = write_posting(&mut tx, &books.capture_lines(AUTHORIZED, FEE)).await?;
    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, &captured, AUTHORIZED, FEE, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, UNIQUE_VIOLATION, "payment_captures_pkey");

    let refund = refund_step(&mut tx, &books, &captured, "captured", 1_000).await?;
    let posting = write_posting(&mut tx, &books.refund_lines(1_000)).await?;
    savepoint(&mut tx).await?;
    let err = insert_refund(&mut tx, &refund, posting).await.unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_violation(err, UNIQUE_VIOLATION, "payment_refunds_pkey");
    tx.rollback().await
}

/// Times that a `timestamptz` holds and Rust cannot read: sqlx panics when it
/// decodes one (ADR-0022). They are text here for the same reason.
const UNREADABLE_TIMES: [&str; 3] = ["infinity", "-infinity", "262143-01-01 00:00:00+00"];
/// The first and the last time that the business-time type accepts.
const BOUND_TIMES: [&str; 2] = [
    "4714-11-24 00:00:00+00 BC",
    "262142-12-31 23:59:59.999999+00",
];

/// Asserts that a record's insert held a bound time and refused any other.
#[track_caller]
fn assert_time_guard(result: sqlx::Result<PgQueryResult>, time: &str, constraint: &str) {
    if BOUND_TIMES.contains(&time) {
        result.unwrap_or_else(|err| panic!("{time}: {err}"));
    } else {
        assert_violation(result.expect_err(time), CHECK_VIOLATION, constraint);
    }
}

#[sqlx::test]
async fn a_records_time_is_one_that_rust_can_read(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(&pool).await?;
    let books = open_books(&mut tx).await?;
    let authorization =
        payment_with_first_step(&mut tx)
            .await?
            .then("authorize", "authorized", AUTHORIZED, 0);
    authorization.insert(&mut tx).await?;
    let capture = authorized_payment(&mut tx, &books)
        .await?
        .then("capture", "captured", AUTHORIZED, AUTHORIZED);
    capture.insert(&mut tx).await?;
    let capture_posting = write_posting(&mut tx, &books.capture_lines(AUTHORIZED, FEE)).await?;
    let refund = captured_payment(&mut tx, &books)
        .await?
        .then("refund", "refunded", AUTHORIZED, 0);
    refund.insert(&mut tx).await?;
    let refund_posting = write_posting(&mut tx, &books.refund_lines(AUTHORIZED)).await?;
    for time in UNREADABLE_TIMES.into_iter().chain(BOUND_TIMES) {
        savepoint(&mut tx).await?;
        let result = sqlx::query!(
            "INSERT INTO payment_authorizations \
                 (transition_id, payment_id, amount, payment_amount, expires_at) \
             VALUES ($1, $2, $3, $4, $5::text::timestamptz)",
            authorization.id,
            authorization.payment_id,
            authorization.amount,
            REQUESTED,
            time
        )
        .execute(&mut *tx)
        .await;
        assert_time_guard(result, time, "payment_authorizations_expires_at_check");
        back_to_savepoint(&mut tx).await?;

        savepoint(&mut tx).await?;
        let result = sqlx::query!(
            "INSERT INTO payment_captures \
                 (transition_id, payment_id, amount, authorized_amount, fee, captured_at, \
                  ledger_transaction_id) \
             VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz, $7)",
            capture.id,
            capture.payment_id,
            capture.amount,
            AUTHORIZED,
            FEE,
            time,
            capture_posting
        )
        .execute(&mut *tx)
        .await;
        assert_time_guard(result, time, "payment_captures_captured_at_check");
        back_to_savepoint(&mut tx).await?;

        savepoint(&mut tx).await?;
        let result = sqlx::query!(
            "INSERT INTO payment_refunds \
                 (transition_id, payment_id, amount, refunded_at, ledger_transaction_id) \
             VALUES ($1, $2, $3, $4::text::timestamptz, $5)",
            refund.id,
            refund.payment_id,
            refund.amount,
            time,
            refund_posting
        )
        .execute(&mut *tx)
        .await;
        assert_time_guard(result, time, "payment_refunds_refunded_at_check");
        back_to_savepoint(&mut tx).await?;
    }
    tx.rollback().await
}

#[sqlx::test]
async fn the_runtime_role_cannot_write_a_records_kind(pool: PgPool) -> sqlx::Result<()> {
    // Naming the generated column fails, even as DEFAULT.
    let mut app = runtime_connection(&pool).await?;
    let (id, payment) = (Uuid::now_v7(), Uuid::now_v7());
    for err in [
        sqlx::query!(
            "INSERT INTO payment_authorizations \
                 (transition_id, payment_id, kind, amount, payment_amount, expires_at) \
             VALUES ($1, $2, DEFAULT, 1, 1, transaction_timestamp())",
            id,
            payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_captures \
                 (transition_id, payment_id, kind, amount, authorized_amount, fee, captured_at, \
                  ledger_transaction_id) \
             VALUES ($1, $2, DEFAULT, 1, 1, 0, transaction_timestamp(), $2)",
            id,
            payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_refunds \
                 (transition_id, payment_id, kind, amount, refunded_at, ledger_transaction_id) \
             VALUES ($1, $2, DEFAULT, 1, transaction_timestamp(), $2)",
            id,
            payment
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
async fn the_runtime_role_cannot_change_the_records(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    for err in [
        sqlx::query!("UPDATE payment_authorizations SET amount = 1")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM payment_authorizations")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE payment_authorizations CASCADE")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("UPDATE payment_captures SET amount = 1")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM payment_captures")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE payment_captures")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("UPDATE payment_refunds SET amount = 1")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM payment_refunds")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE payment_refunds")
            .execute(&mut app)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    Ok(())
}

#[sqlx::test]
async fn the_records_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let captured = captured_payment(&mut tx, &books).await?;
    refund_step(&mut tx, &books, &captured, "captured", 1_000).await?;
    tx.commit().await?;

    for (err, trigger, table) in [
        (
            sqlx::query!("UPDATE payment_authorizations SET amount = 1")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_authorizations_append_only",
            "payment_authorizations",
        ),
        (
            sqlx::query!("DELETE FROM payment_authorizations")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_authorizations_append_only",
            "payment_authorizations",
        ),
        (
            // CASCADE, because the captures' foreign key refers to this table.
            sqlx::query!("TRUNCATE payment_authorizations CASCADE")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_authorizations_no_truncate",
            "payment_authorizations",
        ),
        (
            sqlx::query!("UPDATE payment_captures SET amount = 1")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_captures_append_only",
            "payment_captures",
        ),
        (
            sqlx::query!("DELETE FROM payment_captures")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_captures_append_only",
            "payment_captures",
        ),
        (
            sqlx::query!("TRUNCATE payment_captures")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_captures_no_truncate",
            "payment_captures",
        ),
        (
            sqlx::query!("UPDATE payment_refunds SET amount = 1")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_refunds_append_only",
            "payment_refunds",
        ),
        (
            sqlx::query!("DELETE FROM payment_refunds")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_refunds_append_only",
            "payment_refunds",
        ),
        (
            sqlx::query!("TRUNCATE payment_refunds")
                .execute(&pool)
                .await
                .unwrap_err(),
            "payment_refunds_no_truncate",
            "payment_refunds",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, table);
    }
    Ok(())
}
