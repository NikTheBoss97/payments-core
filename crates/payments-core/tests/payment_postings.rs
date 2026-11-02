// Author: Nicola Antonello

//! Database tests for the check of a record against its posting (ADR-0021).
//!
//! Every bad posting here balances, so the ledger's own check passes and only
//! this one can refuse it. Rows are written with raw SQL as the runtime role,
//! `payments_app`, unless a test says otherwise. Each test asserts the code,
//! the trigger and the table that reject the commit (ADR-0013).

mod common;

use common::payments::{
    AUTHORIZED, Books, FEE, NO_GENESIS, NO_RECORD, NewPayment, STEP_COMMIT_CHECK, Step,
    authorized_payment, captured_payment, insert_capture, insert_refund, open_account,
    open_account_in, open_books, payment_with_first_step, write_posting, write_posting_in,
};
use common::{assert_guard, runtime_connection, runtime_transaction};
use payments_core::db::{CommitError, commit};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// SQLSTATE of a record whose posting is not exactly its lines (ADR-0013).
const BAD_POSTING: &str = "LG006";
/// The trigger that checks each capture's posting at COMMIT.
const CAPTURE_COMMIT_CHECK: &str = "payment_captures_commit_check";
/// The trigger that checks each refund's posting at COMMIT.
const REFUND_COMMIT_CHECK: &str = "payment_refunds_commit_check";

/// Writes a capture of an authorized payment whose posting is in this
/// currency, with these lines, and leaves the transaction open.
async fn capture_with_posting_in(
    tx: &mut Transaction<'_, Postgres>,
    books: &Books,
    currency: &str,
    lines: &[(Uuid, i64)],
) -> sqlx::Result<Step> {
    let authorized = authorized_payment(tx, books).await?;
    let step = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    step.insert(tx).await?;
    let posting = write_posting_in(tx, currency, lines).await?;
    insert_capture(tx, &step, AUTHORIZED, FEE, posting).await?;
    Ok(step)
}

/// Writes a refund of 1,000 of a captured payment, with this posting, and
/// leaves the transaction open.
async fn refund_with_posting(
    tx: &mut Transaction<'_, Postgres>,
    books: &Books,
    posting: Uuid,
) -> sqlx::Result<Step> {
    let captured = captured_payment(tx, books).await?;
    let step = captured.then("refund", "captured", 1_000, AUTHORIZED - 1_000);
    step.insert(tx).await?;
    insert_refund(tx, &step, posting).await?;
    Ok(step)
}

#[sqlx::test]
async fn a_capture_posting_must_be_exactly_its_lines(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let other_merchant = Uuid::now_v7().hyphenated().to_string();
    let other_payable =
        open_account(&mut app, "merchant_payable", &other_merchant, "credit").await?;
    let other_receivable = open_account(&mut app, "psp_receivable", "other_psp", "debit").await?;
    let Books {
        receivable,
        payable,
        fees,
        ..
    } = books;

    // The record says 8,000 captured with a fee of 200. Every posting balances,
    // so the ledger's own check passes and only this one can refuse it.
    for lines in [
        // The PSP owes less than was captured.
        vec![(receivable, 7_999), (payable, -7_799), (fees, -200)],
        // A fee other than the record's.
        vec![(receivable, 8_000), (payable, -7_900), (fees, -100)],
        // No fee line.
        vec![(receivable, 8_000), (payable, -8_000)],
        // The right totals, split over an extra line.
        vec![
            (receivable, 8_000),
            (payable, -7_800),
            (fees, -100),
            (fees, -100),
        ],
        // Another merchant's account.
        vec![(receivable, 8_000), (other_payable, -7_800), (fees, -200)],
        // Another PSP's account.
        vec![(other_receivable, 8_000), (payable, -7_800), (fees, -200)],
        // Every sign reversed, which still sums to zero.
        vec![(receivable, -8_000), (payable, 7_800), (fees, 200)],
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        capture_with_posting_in(&mut tx, &books, "EUR", &lines).await?;
        let err = tx.commit().await.unwrap_err();
        assert_guard(err, BAD_POSTING, CAPTURE_COMMIT_CHECK, "payment_captures");
    }
    Ok(())
}

#[sqlx::test]
async fn a_zero_line_is_left_out_of_a_capture_posting(pool: PgPool) -> sqlx::Result<()> {
    // The ledger has no zero entries, so a fee of nothing or of everything
    // gives two lines (ADR-0020).
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    for fee in [0, AUTHORIZED] {
        let mut tx = runtime_transaction(&pool).await?;
        let authorized = authorized_payment(&mut tx, &books).await?;
        let step = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
        step.insert(&mut tx).await?;
        let posting = write_posting(&mut tx, &books.capture_lines(AUTHORIZED, fee)).await?;
        insert_capture(&mut tx, &step, AUTHORIZED, fee, posting).await?;
        tx.commit().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn a_refund_posting_must_be_exactly_its_lines(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let other_merchant = Uuid::now_v7().hyphenated().to_string();
    let other_payable =
        open_account(&mut app, "merchant_payable", &other_merchant, "credit").await?;
    let Books {
        receivable,
        payable,
        ..
    } = books;

    // The record says 1,000 refunded, which the merchant pays (ADR-0020).
    for lines in [
        // A smaller amount.
        vec![(payable, 999), (receivable, -999)],
        // Another merchant pays.
        vec![(other_payable, 1_000), (receivable, -1_000)],
        // Every sign reversed.
        vec![(payable, -1_000), (receivable, 1_000)],
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let posting = write_posting(&mut tx, &lines).await?;
        refund_with_posting(&mut tx, &books, posting).await?;
        let err = tx.commit().await.unwrap_err();
        assert_guard(err, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    }
    Ok(())
}

#[sqlx::test]
async fn a_posting_must_be_in_the_payments_currency(pool: PgPool) -> sqlx::Result<()> {
    // The same kinds, parties and amounts, in USD for a EUR payment: only the
    // currency is wrong.
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let party = books.merchant.hyphenated().to_string();
    let lines = [
        (
            open_account_in(&mut app, "USD", "psp_receivable", "mock", "debit").await?,
            AUTHORIZED,
        ),
        (
            open_account_in(&mut app, "USD", "merchant_payable", &party, "credit").await?,
            FEE - AUTHORIZED,
        ),
        (
            open_account_in(&mut app, "USD", "fee_revenue", "platform", "credit").await?,
            -FEE,
        ),
    ];
    let mut tx = runtime_transaction(&pool).await?;
    capture_with_posting_in(&mut tx, &books, "USD", &lines).await?;
    let err = tx.commit().await.unwrap_err();
    let detail = assert_guard(err, BAD_POSTING, CAPTURE_COMMIT_CHECK, "payment_captures");
    assert!(detail.contains("currency USD against EUR"), "{detail}");

    // pg_catalog has no operator for the currency_code domain, so one in public
    // would decide a direct comparison (ADR-0004). The check compares as text.
    let mut owner = pool.acquire().await?;
    sqlx::query!(
        "CREATE FUNCTION public.never_differs(currency_code, currency_code) RETURNS boolean \
         LANGUAGE sql IMMUTABLE AS 'SELECT false'"
    )
    .execute(&mut *owner)
    .await?;
    sqlx::query!(
        "CREATE OPERATOR public.<> \
         (LEFTARG = currency_code, RIGHTARG = currency_code, FUNCTION = public.never_differs)"
    )
    .execute(&mut *owner)
    .await?;
    drop(owner);

    let mut tx = runtime_transaction(&pool).await?;
    capture_with_posting_in(&mut tx, &books, "USD", &lines).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, BAD_POSTING, CAPTURE_COMMIT_CHECK, "payment_captures");
    Ok(())
}

#[sqlx::test]
async fn one_posting_cannot_serve_a_capture_and_a_refund(pool: PgPool) -> sqlx::Result<()> {
    // The first attack the reviewers found (ADR-0021): the same posting linked
    // as one payment's capture and as another's refund. Each record table holds
    // it once, so no key refuses this. A capture debits the PSP's receivable
    // and a refund credits it, so a posting cannot match both.
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let lines = books.capture_lines(AUTHORIZED, FEE);
    let posting = write_posting(&mut tx, &lines).await?;
    let authorized = authorized_payment(&mut tx, &books).await?;
    let captured = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    captured.insert(&mut tx).await?;
    insert_capture(&mut tx, &captured, AUTHORIZED, FEE, posting).await?;
    refund_with_posting(&mut tx, &books, posting).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    Ok(())
}

#[sqlx::test]
async fn a_record_cannot_link_an_older_posting(pool: PgPool) -> sqlx::Result<()> {
    // The second attack (ADR-0021): a posting that nothing caused, committed
    // earlier, linked to a later refund. Its lines are exactly right, so only
    // the test that this transaction wrote it refuses the link.
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let posting = write_posting(&mut tx, &books.refund_lines(1_000)).await?;
    tx.commit().await?;

    let mut tx = runtime_transaction(&pool).await?;
    refund_with_posting(&mut tx, &books, posting).await?;
    let err = tx.commit().await.unwrap_err();
    let detail = assert_guard(err, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    assert!(
        detail.contains("written by this transaction: f"),
        "{detail}"
    );
    assert!(detail.contains(", 0 lines differ"), "{detail}");
    Ok(())
}

#[sqlx::test]
async fn a_posting_written_in_a_savepoint_is_refused(pool: PgPool) -> sqlx::Result<()> {
    // The cost of the xmin test (ADR-0017): a savepoint's rows carry its own
    // transaction ID. Application code uses no savepoints (ADR-0006).
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    sqlx::query!("SAVEPOINT posting").execute(&mut *tx).await?;
    let posting = write_posting(&mut tx, &books.refund_lines(1_000)).await?;
    sqlx::query!("RELEASE SAVEPOINT posting")
        .execute(&mut *tx)
        .await?;
    refund_with_posting(&mut tx, &books, posting).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    Ok(())
}

#[sqlx::test]
async fn a_temporary_table_cannot_stand_in_for_a_postings_entries(
    pool: PgPool,
) -> sqlx::Result<()> {
    // The attack the pinned search_path stops (ADR-0004): a temporary table is
    // found before public by unqualified names, so without the pin the check
    // would read this decoy holding the record's exact lines.
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let posting = write_posting(&mut tx, &[(books.payable, 999), (books.receivable, -999)]).await?;
    refund_with_posting(&mut tx, &books, posting).await?;
    sqlx::query!(
        "CREATE TEMPORARY TABLE ledger_entries (LIKE public.ledger_entries) ON COMMIT DROP"
    )
    .execute(&mut *tx)
    .await?;
    // Checked against public.ledger_entries when the test is compiled; at run
    // time the unqualified name finds the temporary table.
    sqlx::query!(
        "INSERT INTO ledger_entries \
         (transaction_id, line_no, entry_count, account_id, currency, amount) \
         VALUES ($1, 1, 2, $2, 'EUR', 1000), ($1, 2, 2, $3, 'EUR', -1000)",
        posting,
        books.payable,
        books.receivable
    )
    .execute(&mut *tx)
    .await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    Ok(())
}

#[sqlx::test]
async fn the_library_reads_a_bad_posting_as_a_proven_rollback(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let posting = write_posting(&mut tx, &[(books.payable, 999), (books.receivable, -999)]).await?;
    refund_with_posting(&mut tx, &books, posting).await?;
    let CommitError::Rejected(source) = commit(tx).await.unwrap_err() else {
        panic!("expected a proven rollback");
    };
    assert_guard(source, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    let stored: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 0);
    Ok(())
}

#[sqlx::test]
async fn the_commit_checks_fire_in_replica_mode(pool: PgPool) -> sqlx::Result<()> {
    // session_replication_role = replica stops ordinary triggers, and the
    // internal triggers behind foreign keys. ENABLE ALWAYS keeps these firing
    // (ADR-0004). The test pool connects as the bootstrap superuser, which may
    // set it (ADR-0008).
    let mut app = runtime_connection(&pool).await?;
    let books = open_books(&mut app).await?;

    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    NewPayment::valid(books.merchant).insert(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_GENESIS, "payments_commit_check", "payments");

    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    let first = payment_with_first_step(&mut tx).await?;
    first
        .then("authorize", "authorized", AUTHORIZED, 0)
        .insert(&mut tx)
        .await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, "payment_transitions");

    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    let posting = write_posting(&mut tx, &[(books.payable, 999), (books.receivable, -999)]).await?;
    refund_with_posting(&mut tx, &books, posting).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, BAD_POSTING, REFUND_COMMIT_CHECK, "payment_refunds");
    Ok(())
}
