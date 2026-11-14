// Author: Nicola Antonello

//! What the merchant's role may write of a payment's history (ADR-0034, invariant I8):
//! a payment, its first step and a cancel, and no step, record or ledger line of the
//! PSP's, which only the PSP's facts bring (ADR-0031).
//!
//! Each row is written with raw SQL as the tenant role, `payments_tenant`, for the
//! merchant's own payment, so that row-level isolation lets it through (ADR-0025) and
//! only these guards are left to refuse it. The system role, `payments_app`, writes what
//! comes before the row. The merchant's own steps commit in `tenant_isolation.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, REQUESTED, books, insert_authorization, insert_capture, insert_refund,
    payment_at, write_posting,
};
use common::{INSUFFICIENT_PRIVILEGE, name_merchant, runtime_transaction, tenant_transaction};
use payments_core::lifecycle::PaymentStatus;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// The restrictive policy that keeps the merchant's role to its own steps (ADR-0034).
const STEPS_POLICY: &str = "payment_transitions_tenant_steps";

/// Acts as the merchant's role, for `merchant`, from here to the end of a transaction
/// that the system role began, so that the rows before this point are the system role's.
async fn as_the_merchant(tx: &mut Transaction<'_, Postgres>, merchant: Uuid) -> sqlx::Result<()> {
    sqlx::query!("SET LOCAL ROLE payments_tenant")
        .execute(&mut **tx)
        .await?;
    name_merchant(tx, merchant).await
}

/// Asserts that row-level security refused a new row under `policy`. PostgreSQL names a
/// restrictive policy that a row fails, and no permissive one.
fn assert_refused_by(err: &sqlx::Error, policy: &str) {
    let db = err.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some(INSUFFICIENT_PRIVILEGE), "{db}");
    assert!(
        db.message().contains(&format!("policy \"{policy}\"")),
        "{db}"
    );
}

/// Asserts that the role may not write into `table` at all, which PostgreSQL checks
/// before it reads any row.
fn assert_denied(err: &sqlx::Error, table: &str) {
    let db = err.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some(INSUFFICIENT_PRIVILEGE), "{db}");
    assert_eq!(db.message(), format!("permission denied for table {table}"));
}

#[sqlx::test]
async fn no_step_of_the_psps_passes_the_policy(pool: PgPool) -> TestResult {
    // Each step that only a fact of the PSP's brings, from a status where the table of
    // moves allows it and with the amounts that it carries, on the merchant's own
    // payment. The system role then takes the same row, which shows that nothing but the
    // policy refused it.
    let books = books(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let created = payment_at(&mut tx, &books, PaymentStatus::Created).await?;
    let authorized = payment_at(&mut tx, &books, PaymentStatus::Authorized).await?;
    let captured = payment_at(&mut tx, &books, PaymentStatus::Captured).await?;
    tx.commit().await?;
    for step in [
        created.then("authorize", "authorized", AUTHORIZED, 0),
        created.then("decline", "failed", 0, 0),
        authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED),
        authorized.then("void", "voided", 0, 0),
        authorized.then("expire", "voided", 0, 0),
        captured.then("settle", "settled", 0, AUTHORIZED),
        captured.then("refund", "captured", 1_000, AUTHORIZED - 1_000),
    ] {
        let mut tx = tenant_transaction(&pool, books.merchant).await?;
        let err = step.insert_rejected(&mut tx).await;
        assert_refused_by(&err, STEPS_POLICY);
        tx.rollback().await?;

        let mut tx = runtime_transaction(&pool).await?;
        step.insert(&mut tx).await?;
        tx.rollback().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn the_merchants_role_writes_no_ledger_line(pool: PgPool) -> TestResult {
    // A posting's header, and a line of a header that the system role wrote first in the
    // same transaction: rows that the role could write before ADR-0034. It holds no
    // privilege on the ledger's tables now, so nothing else is asked. Nothing commits.
    let books = books(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    as_the_merchant(&mut tx, books.merchant).await?;
    let err = write_posting(&mut tx, &books.refund_lines(1_000))
        .await
        .unwrap_err();
    assert_denied(&err, "ledger_transactions");
    tx.rollback().await?;

    let mut tx = runtime_transaction(&pool).await?;
    let posting = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
        posting,
        2,
        "EUR"
    )
    .execute(&mut *tx)
    .await?;
    as_the_merchant(&mut tx, books.merchant).await?;
    let err = sqlx::query!(
        "INSERT INTO ledger_entries (transaction_id, line_no, entry_count, account_id, currency, amount) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        posting,
        1,
        2,
        books.payable,
        "EUR",
        1_000_i64
    )
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_denied(&err, "ledger_entries");
    tx.rollback().await?;
    Ok(())
}

#[sqlx::test]
async fn the_merchants_role_writes_no_record_of_a_step(pool: PgPool) -> TestResult {
    // The record of each step that carries one, after the system role wrote the step and
    // its posting in the same transaction, so that the record is one the role could write
    // before ADR-0034 and only the missing privilege refuses it. Nothing commits.
    let books = books(&pool).await?;

    let mut tx = runtime_transaction(&pool).await?;
    let created = payment_at(&mut tx, &books, PaymentStatus::Created).await?;
    let authorized = created.then("authorize", "authorized", AUTHORIZED, 0);
    authorized.insert(&mut tx).await?;
    as_the_merchant(&mut tx, books.merchant).await?;
    let err = insert_authorization(&mut tx, &authorized, REQUESTED)
        .await
        .unwrap_err();
    assert_denied(&err, "payment_authorizations");
    tx.rollback().await?;

    let mut tx = runtime_transaction(&pool).await?;
    let authorized = payment_at(&mut tx, &books, PaymentStatus::Authorized).await?;
    let captured = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    captured.insert(&mut tx).await?;
    let posting = write_posting(&mut tx, &books.capture_lines(AUTHORIZED, FEE)).await?;
    as_the_merchant(&mut tx, books.merchant).await?;
    let err = insert_capture(&mut tx, &captured, AUTHORIZED, FEE, posting)
        .await
        .unwrap_err();
    assert_denied(&err, "payment_captures");
    tx.rollback().await?;

    let mut tx = runtime_transaction(&pool).await?;
    let captured = payment_at(&mut tx, &books, PaymentStatus::Captured).await?;
    let refund = captured.then("refund", "captured", 1_000, AUTHORIZED - 1_000);
    refund.insert(&mut tx).await?;
    let posting = write_posting(&mut tx, &books.refund_lines(1_000)).await?;
    as_the_merchant(&mut tx, books.merchant).await?;
    let err = insert_refund(&mut tx, &refund, posting).await.unwrap_err();
    assert_denied(&err, "payment_refunds");
    tx.rollback().await?;
    Ok(())
}
