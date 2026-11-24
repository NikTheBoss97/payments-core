// Author: Nicola Antonello

//! Database tests for the tables that booking a settlement batch writes (ADR-0066 to
//! ADR-0069, ADR-0072): the terms agreed with a PSP, batches with their postings, lines
//! and matches, and the chains of the movements that they settle, with the guards of I11
//! in PostgreSQL (ADR-0071). The account kinds that reconciliation posts to are tested
//! with the rest of the chart, in `account_kinds.rs`.
//!
//! Rows are written with raw SQL, as the system role, `payments_app`, where it may
//! write them, and as the tables' owner where only migrations may. Each rejected write
//! breaks a single rule. The test asserts the SQLSTATE and, where PostgreSQL reports
//! one, the constraint or trigger that rejects the write; privilege errors carry none.
//! Transactions commit through the one function that reads a `COMMIT`'s error (ADR-0014),
//! so a deferred check that refuses one must read as a rollback.

mod common;

use common::payments::VERSION_4;
use common::settlement::{
    Accounts, Movement, MovementStep, SETTLED_POSITION, accounts, insert_batch,
    insert_capture_match, insert_line, insert_refund_match, later_movement, link_posting, post,
    settled_capture,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_transaction, sqlstate,
};
use payments_core::db::{CommitError, commit};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Terms of `psp` in `currency` from `starts_at`, an RFC 3339 time, written as the
/// tables' owner, which is how a migration writes them.
async fn insert_terms(
    pool: &PgPool,
    psp: &str,
    currency: &str,
    starts_at: &str,
    (fee_bps, fee_fixed, settle_within): (i32, i64, i64),
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO psp_terms (psp, currency, starts_at, fee_bps, fee_fixed, settle_within) \
         VALUES ($1, $2, $3::text::timestamptz, $4, $5, $6)",
        psp,
        currency,
        starts_at,
        fee_bps,
        fee_fixed,
        settle_within
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The mock's terms: 140 basis points and 25 minor units on a capture, settling
/// within two days (ADR-0072).
const MOCK_TERMS: (i32, i64, i64) = (140, 25, 172_800);

#[sqlx::test]
async fn the_mocks_terms_hold_in_every_currency_from_year_zero(pool: PgPool) -> TestResult {
    let rows = sqlx::query!(
        r#"SELECT currency::text AS "currency!", fee_bps, fee_fixed, settle_within,
                  starts_at = '0001-01-01 00:00:00+00 BC'::timestamptz AS "from_year_zero!"
             FROM psp_terms
            WHERE psp = 'mock'
            ORDER BY currency"#
    )
    .fetch_all(&pool)
    .await?;
    let terms: Vec<_> = rows
        .into_iter()
        .map(|r| {
            (
                r.currency,
                (r.fee_bps, r.fee_fixed, r.settle_within),
                r.from_year_zero,
            )
        })
        .collect();
    let expected: Vec<_> = ["EUR", "GBP", "ISK", "JPY", "KWD", "USD"]
        .into_iter()
        .map(|currency| (currency.to_owned(), MOCK_TERMS, true))
        .collect();
    assert_eq!(terms, expected);
    Ok(())
}

#[sqlx::test]
async fn only_migrations_write_the_terms(pool: PgPool) -> TestResult {
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!(
        "INSERT INTO psp_terms (psp, currency, starts_at, fee_bps, fee_fixed, settle_within) \
         VALUES ('mock', 'EUR', '2026-01-01 00:00:00+00', 100, 10, 86400)"
    )
    .execute(&mut *tx)
    .await
    .expect_err("the system role only reads the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

#[sqlx::test]
async fn the_terms_never_change(pool: PgPool) -> TestResult {
    // As the owner, the triggers refuse.
    let err = sqlx::query!("UPDATE psp_terms SET fee_bps = 0")
        .execute(&pool)
        .await
        .expect_err("terms are append-only");
    assert_guard(err, APPEND_ONLY, "psp_terms_append_only", "psp_terms");
    let err = sqlx::query!("DELETE FROM psp_terms")
        .execute(&pool)
        .await
        .expect_err("terms are append-only");
    assert_guard(err, APPEND_ONLY, "psp_terms_append_only", "psp_terms");
    let err = sqlx::query!("TRUNCATE psp_terms")
        .execute(&pool)
        .await
        .expect_err("terms are append-only");
    assert_guard(err, APPEND_ONLY, "psp_terms_no_truncate", "psp_terms");

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("UPDATE psp_terms SET fee_bps = 0")
        .execute(&mut *tx)
        .await
        .expect_err("the system role cannot change the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM psp_terms")
        .execute(&mut *tx)
        .await
        .expect_err("the system role cannot change the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("TRUNCATE psp_terms")
        .execute(&mut *tx)
        .await
        .expect_err("the system role cannot change the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

#[sqlx::test]
async fn the_terms_keep_to_their_bounds(pool: PgPool) -> TestResult {
    let later = "2026-01-01T00:00:00Z";
    for (terms, constraint) in [
        ((-1, 25, 172_800), "psp_terms_fee_bps_check"),
        ((10_001, 25, 172_800), "psp_terms_fee_bps_check"),
        ((140, -1, 172_800), "psp_terms_fee_fixed_check"),
        (
            (140, 1_000_000_000_000_001, 172_800),
            "psp_terms_fee_fixed_check",
        ),
        ((140, 25, 0), "psp_terms_settle_within_check"),
        ((140, 25, 31_622_401), "psp_terms_settle_within_check"),
    ] {
        let err = insert_terms(&pool, "mock", "EUR", later, terms)
            .await
            .expect_err("terms out of bounds");
        assert_violation(err, CHECK_VIOLATION, constraint);
    }
    let err = insert_terms(&pool, "other", "EUR", later, MOCK_TERMS)
        .await
        .expect_err("a PSP that the service does not know");
    assert_violation(err, CHECK_VIOLATION, "psp_terms_psp_check");
    let err = insert_terms(&pool, "mock", "EUR", "10000-01-01T00:00:00Z", MOCK_TERMS)
        .await
        .expect_err("a time past RFC 3339's years");
    assert_violation(err, CHECK_VIOLATION, "psp_terms_starts_at_check");
    let err = insert_terms(&pool, "mock", "XXX", later, MOCK_TERMS)
        .await
        .expect_err("a currency that the service does not know");
    assert_violation(err, FOREIGN_KEY_VIOLATION, "psp_terms_currency_fkey");
    let err = insert_terms(
        &pool,
        "mock",
        "EUR",
        "0001-01-01 00:00:00+00 BC",
        MOCK_TERMS,
    )
    .await
    .expect_err("terms from the same start");
    assert_violation(err, UNIQUE_VIOLATION, "psp_terms_pkey");

    // Within every bound, later terms join the earlier ones.
    insert_terms(&pool, "mock", "EUR", later, (0, 0, 1)).await?;
    insert_terms(
        &pool,
        "mock",
        "EUR",
        "2027-01-01T00:00:00Z",
        (10_000, 1, 31_622_400),
    )
    .await?;
    Ok(())
}

/// The mock's fee on a capture of 10,000: 140 basis points and 25 (ADR-0066).
const FEE: i64 = 165;
/// The capture that most tests settle.
const CAPTURED: i64 = 10_000;
/// A refund of part of it.
const REFUNDED: i64 = 2_500;
/// SQLSTATE of a batch whose posting is not exactly its lines (ADR-0072).
const NOT_ITS_LINES: &str = "LG009";
/// SQLSTATE of a step without the record that it needs (ADR-0069).
const NO_RECORD: &str = "LG011";

/// Commits through the one function that reads a `COMMIT`'s error (ADR-0014), and returns
/// the error of the deferred check that refused it.
async fn commit_refused(tx: Transaction<'static, Postgres>) -> sqlx::Error {
    match commit(tx).await {
        Err(CommitError::Rejected(err)) => err,
        other => panic!("expected a deferred check to refuse the commit, got {other:?}"),
    }
}

/// Commits through the one `COMMIT` function, as a test's setup, which must succeed.
async fn committed(tx: Transaction<'static, Postgres>) -> sqlx::Result<()> {
    commit(tx)
        .await
        .map_err(|err| sqlx::Error::Protocol(err.to_string()))
}

/// A captured payment of 10,000 that the PSP's history settled, and a refund of 2,500 on
/// it, with the accounts of a batch in EUR.
async fn capture_and_refund(pool: &PgPool) -> sqlx::Result<(Movement, Movement, Accounts)> {
    let capture = settled_capture(pool, "EUR", CAPTURED).await?;
    let refund = later_movement(pool, &capture, "refund", 3, REFUNDED).await?;
    Ok((capture, refund, accounts(pool, "EUR").await?))
}

/// Writes the first two steps of `movement`'s chain, open and settle. Returns the settle
/// step.
async fn settle(conn: &mut PgConnection, movement: &Movement) -> sqlx::Result<MovementStep> {
    let open = MovementStep::open(movement);
    let settle = open.then("settle", "settled");
    open.insert(conn).await?;
    settle.insert(conn).await?;
    Ok(settle)
}

/// Writes batch `number` with lines settling each of `movements` from line 2 on, at their
/// amounts with the given fees, their steps and matches, but no posting.
async fn batch_of(
    conn: &mut PgConnection,
    (batch, number, payout): (Uuid, i64, i64),
    movements: &[(&Movement, i64)],
) -> sqlx::Result<()> {
    insert_batch(conn, batch, "EUR", number, payout).await?;
    for (line_no, &(movement, fee)) in (2_i32..).zip(movements) {
        let gross = if movement.kind == "capture" {
            movement.amount
        } else {
            movement.amount.checked_neg().expect("a positive amount")
        };
        let net = gross.checked_sub(fee).expect("small amounts");
        insert_line(conn, batch, line_no, movement.kind, (gross, fee, net)).await?;
        let step = settle(conn, movement).await?;
        if movement.kind == "capture" {
            insert_capture_match(conn, batch, line_no, movement, SETTLED_POSITION, step.id).await?;
        } else {
            insert_refund_match(conn, batch, line_no, movement, step.id).await?;
        }
    }
    Ok(())
}

/// Writes batch 1, settling the capture of 10,000 at its fee and the refund of 2,500, for a
/// payout of 7,335, but no posting.
async fn batch_of_two(
    conn: &mut PgConnection,
    batch: Uuid,
    capture: &Movement,
    refund: &Movement,
) -> sqlx::Result<()> {
    batch_of(conn, (batch, 1, 7_335), &[(capture, FEE), (refund, 0)]).await
}

/// The entries that ADR-0067 derives for [`batch_of_two`].
fn entries_of_two(accounts: &Accounts) -> [(Uuid, i64); 3] {
    [
        (accounts.payouts, 7_335),
        (accounts.fees, FEE),
        (accounts.receivable, -(CAPTURED - REFUNDED)),
    ]
}

/// Books [`batch_of_two`] with its posting, and commits. Returns the batch and the capture.
async fn book_two(pool: &PgPool) -> sqlx::Result<(Uuid, Movement)> {
    let (capture, refund, accounts) = capture_and_refund(pool).await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(pool).await?;
    batch_of_two(&mut tx, batch, &capture, &refund).await?;
    let posting = post(&mut tx, "EUR", &entries_of_two(&accounts)).await?;
    link_posting(&mut tx, batch, posting).await?;
    committed(tx).await?;
    Ok((batch, capture))
}

#[sqlx::test]
async fn a_batch_books_the_capture_and_the_refund_that_it_settles(pool: PgPool) -> TestResult {
    let (batch, _) = book_two(&pool).await?;
    let matched = sqlx::query_scalar!(
        r#"SELECT (SELECT count(*) FROM settlement_capture_matches WHERE batch_id = $1)
                + (SELECT count(*) FROM settlement_refund_matches WHERE batch_id = $1)
                  AS "matched!""#,
        batch
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(matched, 2);
    Ok(())
}

#[sqlx::test]
async fn a_line_that_settles_nothing_of_ours_puts_its_net_in_suspense(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 1, 468).await?;
    insert_line(&mut tx, batch, 2, "capture", (500, 32, 468)).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 468), (accounts.suspense, -468)],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_payout_that_differs_from_its_nets_puts_the_difference_in_suspense(
    pool: PgPool,
) -> TestResult {
    let capture = settled_capture(&pool, "EUR", CAPTURED).await?;
    let accounts = accounts(&pool, "EUR").await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    // The PSP pays 9,800 for a net of 9,835: 35 that nothing explains.
    batch_of(&mut tx, (batch, 1, 9_800), &[(&capture, FEE)]).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (accounts.payouts, 9_800),
            (accounts.fees, FEE),
            (accounts.receivable, -CAPTURED),
            (accounts.suspense, 35),
        ],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_batch_whose_entries_are_all_zero_books_no_posting(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    // A capture of 1,000 with no fee, refunded in full in the same batch.
    let capture = settled_capture(&pool, "EUR", 1_000).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, 1_000).await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    batch_of(&mut tx, (batch, 1, 0), &[(&capture, 0), (&refund, 0)]).await?;
    commit(tx).await?;

    // With a posting, however small, the same lines are refused.
    let capture = settled_capture(&pool, "EUR", 1_000).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, 1_000).await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    batch_of(&mut tx, (batch, 2, 0), &[(&capture, 0), (&refund, 0)]).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 1), (accounts.suspense, -1)],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_batches_commit_check",
        "settlement_batches",
    );
    Ok(())
}

#[sqlx::test]
async fn a_batch_with_entries_left_needs_its_posting(pool: PgPool) -> TestResult {
    let (capture, refund, _) = capture_and_refund(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    batch_of_two(&mut tx, Uuid::now_v7(), &capture, &refund).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_batches_commit_check",
        "settlement_batches",
    );
    Ok(())
}

#[sqlx::test]
async fn a_batchs_posting_is_exactly_its_lines(pool: PgPool) -> TestResult {
    let (capture, refund, accounts) = capture_and_refund(&pool).await?;
    // Each posting balances, so only LG009 can refuse it.
    for entries in [
        // A fee one too low, and a payout one too high.
        vec![
            (accounts.payouts, 7_336),
            (accounts.fees, FEE - 1),
            (accounts.receivable, -7_500),
        ],
        // The receivable's entry split in two.
        vec![
            (accounts.payouts, 7_335),
            (accounts.fees, FEE),
            (accounts.receivable, -7_000),
            (accounts.receivable, -500),
        ],
        // Two entries more.
        vec![
            (accounts.payouts, 7_335),
            (accounts.fees, FEE),
            (accounts.receivable, -7_500),
            (accounts.suspense, 1),
            (accounts.payouts, -1),
        ],
        // The matched lines' gross in suspense, as if they settled nothing of ours.
        vec![
            (accounts.payouts, 7_335),
            (accounts.fees, FEE),
            (accounts.suspense, -7_500),
        ],
    ] {
        let batch = Uuid::now_v7();
        let mut tx = runtime_transaction(&pool).await?;
        batch_of_two(&mut tx, batch, &capture, &refund).await?;
        let posting = post(&mut tx, "EUR", &entries).await?;
        link_posting(&mut tx, batch, posting).await?;
        let err = commit_refused(tx).await;
        assert_guard(
            err,
            NOT_ITS_LINES,
            "settlement_batches_commit_check",
            "settlement_batches",
        );
    }
    Ok(())
}

#[sqlx::test]
async fn a_batchs_posting_is_written_with_it_in_its_currency(pool: PgPool) -> TestResult {
    let (capture, refund, euros) = capture_and_refund(&pool).await?;
    // The right entries, committed before the batch.
    let mut tx = runtime_transaction(&pool).await?;
    let earlier = post(&mut tx, "EUR", &entries_of_two(&euros)).await?;
    tx.commit().await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    batch_of_two(&mut tx, batch, &capture, &refund).await?;
    link_posting(&mut tx, batch, earlier).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_batches_commit_check",
        "settlement_batches",
    );

    // The right amounts, in another currency.
    let pounds = accounts(&pool, "GBP").await?;
    let mut tx = runtime_transaction(&pool).await?;
    batch_of_two(&mut tx, batch, &capture, &refund).await?;
    let posting = post(&mut tx, "GBP", &entries_of_two(&pounds)).await?;
    link_posting(&mut tx, batch, posting).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_batches_commit_check",
        "settlement_batches",
    );
    Ok(())
}

/// Inserts a batch in a transaction of its own and returns the error, which a test expects.
async fn batch_refused(
    pool: &PgPool,
    (id, psp, currency, number): (Uuid, &str, &str, i64),
    settled_at: &str,
    payout: i64,
) -> sqlx::Result<sqlx::Error> {
    let mut tx = runtime_transaction(pool).await?;
    Ok(sqlx::query!(
        "INSERT INTO settlement_batches (id, psp, currency, number, settled_at, payout) \
         VALUES ($1, $2, $3, $4, $5::text::timestamptz, $6)",
        id,
        psp,
        currency,
        number,
        settled_at,
        payout
    )
    .execute(&mut *tx)
    .await
    .expect_err("a batch that breaks a rule"))
}

#[sqlx::test]
async fn a_batch_keeps_to_its_rules(pool: PgPool) -> TestResult {
    let when = "2026-11-20T00:00:00Z";
    let id = Uuid::now_v7();
    for (key, settled_at, payout, code, constraint) in [
        (
            (VERSION_4, "mock", "EUR", 1),
            when,
            0,
            CHECK_VIOLATION,
            "settlement_batches_id_check",
        ),
        (
            (id, "other", "EUR", 1),
            when,
            0,
            CHECK_VIOLATION,
            "settlement_batches_psp_check",
        ),
        (
            (id, "mock", "EUR", 0),
            when,
            0,
            CHECK_VIOLATION,
            "settlement_batches_number_check",
        ),
        (
            (id, "mock", "EUR", 1),
            "10000-01-01T00:00:00Z",
            0,
            CHECK_VIOLATION,
            "settlement_batches_settled_at_check",
        ),
        (
            (id, "mock", "EUR", 1),
            when,
            1_000_000_000_000_001,
            CHECK_VIOLATION,
            "settlement_batches_payout_check",
        ),
        (
            (id, "mock", "XXX", 1),
            when,
            0,
            FOREIGN_KEY_VIOLATION,
            "settlement_batches_currency_fkey",
        ),
    ] {
        let err = batch_refused(&pool, key, settled_at, payout).await?;
        assert_violation(err, code, constraint);
    }

    // A batch with no lines and no payout has no entries, so it needs no posting.
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, Uuid::now_v7(), "EUR", 7, 0).await?;
    commit(tx).await?;
    let err = batch_refused(&pool, (Uuid::now_v7(), "mock", "EUR", 7), when, 0).await?;
    assert_violation(err, UNIQUE_VIOLATION, "settlement_batches_number_key");
    Ok(())
}

#[sqlx::test]
async fn no_role_writes_a_batchs_transaction(pool: PgPool) -> TestResult {
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!(
        "INSERT INTO settlement_batches \
             (id, psp, currency, number, settled_at, payout, xact_id) \
         VALUES ($1, 'mock', 'EUR', 1, '2026-11-20 00:00:00+00', 0, pg_current_xact_id())",
        Uuid::now_v7()
    )
    .execute(&mut *tx)
    .await
    .expect_err("only the default fills xact_id");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

/// A batch with no lines, committed, so that a later transaction can try to add to it.
async fn empty_batch(pool: &PgPool, number: i64) -> sqlx::Result<Uuid> {
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(pool).await?;
    insert_batch(&mut tx, batch, "EUR", number, 0).await?;
    committed(tx).await?;
    Ok(batch)
}

#[sqlx::test]
async fn a_line_keeps_to_its_rules(pool: PgPool) -> TestResult {
    // The CHECKs run when the row is written, before the key that ties the line to its
    // batch's transaction, so a committed batch serves for each of them.
    let batch = empty_batch(&pool, 1).await?;
    for ((line_no, kind, amounts), constraint) in [
        ((2, "fee", (10, 0, 10)), "settlement_lines_kind_check"),
        ((2, "capture", (-1, 0, -1)), "settlement_lines_gross_check"),
        ((2, "refund", (1, 0, 1)), "settlement_lines_gross_check"),
        ((2, "capture", (100, 10, 91)), "settlement_lines_net_check"),
        (
            (
                2,
                "capture",
                (1_000_000_000_000_001, 0, 1_000_000_000_000_001),
            ),
            "settlement_lines_amounts_check",
        ),
        (
            (1, "capture", (100, 10, 90)),
            "settlement_lines_line_no_check",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let err = insert_line(&mut tx, batch, line_no, kind, amounts)
            .await
            .expect_err("a line that breaks a rule");
        assert_violation(err, CHECK_VIOLATION, constraint);
    }
    Ok(())
}

#[sqlx::test]
async fn a_batchs_rows_are_written_in_its_own_transaction(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    let batch = empty_batch(&pool, 1).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let err = insert_line(&mut tx, batch, 2, "capture", (100, 10, 90))
        .await
        .expect_err("a line after its batch's transaction");
    assert_violation(err, FOREIGN_KEY_VIOLATION, "settlement_lines_batch_fkey");
    let mut tx = runtime_transaction(&pool).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 1), (accounts.suspense, -1)],
    )
    .await?;
    let err = link_posting(&mut tx, batch, posting)
        .await
        .expect_err("a posting after its batch's transaction");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_batch_postings_batch_fkey",
    );

    // A capture line booked in suspense, then a match for it in a later transaction.
    let capture = settled_capture(&pool, "EUR", CAPTURED).await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 2, 9_835).await?;
    insert_line(&mut tx, batch, 2, "capture", (CAPTURED, FEE, 9_835)).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 9_835), (accounts.suspense, -9_835)],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let step = settle(&mut tx, &capture).await?;
    let err = insert_capture_match(&mut tx, batch, 2, &capture, SETTLED_POSITION, step.id)
        .await
        .expect_err("a match after its batch's transaction");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_batch_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_posting_belongs_to_one_batch(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, first, "EUR", 1, 1).await?;
    insert_batch(&mut tx, second, "EUR", 2, 1).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 1), (accounts.suspense, -1)],
    )
    .await?;
    link_posting(&mut tx, first, posting).await?;
    let err = link_posting(&mut tx, second, posting)
        .await
        .expect_err("one posting for two batches");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_batch_postings_ledger_transaction_id_key",
    );
    Ok(())
}

/// In a transaction of its own: batch 1 in `currency` with a capture line of `capture`'s
/// amount on line 2 and a refund line of -2,500 on line 3, and the open and settle steps of
/// `capture`'s movement. Returns the transaction, the batch and the settle step.
async fn ready_to_match(
    pool: &PgPool,
    currency: &str,
    capture: &Movement,
) -> sqlx::Result<(Transaction<'static, Postgres>, Uuid, MovementStep)> {
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(pool).await?;
    insert_batch(&mut tx, batch, currency, 1, 0).await?;
    let net = capture.amount.checked_sub(FEE).expect("small amounts");
    insert_line(&mut tx, batch, 2, "capture", (capture.amount, FEE, net)).await?;
    insert_line(&mut tx, batch, 3, "refund", (-REFUNDED, 0, -REFUNDED)).await?;
    let step = settle(&mut tx, capture).await?;
    Ok((tx, batch, step))
}

#[sqlx::test]
async fn a_capture_match_agrees_with_its_line_fact_request_and_step(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", CAPTURED).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, REFUNDED).await?;

    // A capture's match on the refund's line.
    let (mut tx, batch, step) = ready_to_match(&pool, "EUR", &capture).await?;
    let err = insert_capture_match(&mut tx, batch, 3, &capture, SETTLED_POSITION, step.id)
        .await
        .expect_err("a capture's match on a refund's line");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_line_fkey",
    );

    // A fact that the PSP's history does not hold.
    let (mut tx, batch, step) = ready_to_match(&pool, "EUR", &capture).await?;
    let unknown = Movement {
        fact_id: "fct_unknown".to_owned(),
        ..capture.clone()
    };
    let err = insert_capture_match(&mut tx, batch, 2, &unknown, SETTLED_POSITION, step.id)
        .await
        .expect_err("a fact that we do not hold");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_fact_fkey",
    );

    // The capture's own fact named as its settled fact.
    let (mut tx, batch, step) = ready_to_match(&pool, "EUR", &capture).await?;
    let err = insert_capture_match(&mut tx, batch, 2, &capture, capture.position, step.id)
        .await
        .expect_err("a settled position that holds no settled fact");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_settled_fkey",
    );

    // Another request than the one that the fact decided.
    let (mut tx, batch, step) = ready_to_match(&pool, "EUR", &capture).await?;
    let other_request = Movement {
        request: refund.request,
        ..capture.clone()
    };
    let err = insert_capture_match(&mut tx, batch, 2, &other_request, SETTLED_POSITION, step.id)
        .await
        .expect_err("a request that the fact did not decide");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_decision_fkey",
    );

    // The movement's open step named as its settle step.
    let (mut tx, batch, _) = ready_to_match(&pool, "EUR", &capture).await?;
    let open = sqlx::query_scalar!(
        "SELECT id FROM settlement_movement_steps \
          WHERE payment_id = $1 AND position = $2 AND seq = 0",
        capture.payment,
        capture.position
    )
    .fetch_one(&mut *tx)
    .await?;
    let err = insert_capture_match(&mut tx, batch, 2, &capture, SETTLED_POSITION, open)
        .await
        .expect_err("a step of another kind");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_step_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_capture_match_names_its_batchs_currency_and_its_payments(pool: PgPool) -> TestResult {
    // A payment in pounds, matched in a batch in euros.
    let pounds = settled_capture(&pool, "GBP", CAPTURED).await?;
    let (mut tx, batch, step) = ready_to_match(&pool, "EUR", &pounds).await?;
    let err = insert_capture_match(&mut tx, batch, 2, &pounds, SETTLED_POSITION, step.id)
        .await
        .expect_err("a currency that is not the batch's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_currency_fkey",
    );

    // A payment in euros, matched as pounds in a batch in pounds.
    let euros = settled_capture(&pool, "EUR", CAPTURED).await?;
    let (mut tx, batch, step) = ready_to_match(&pool, "GBP", &euros).await?;
    let as_pounds = Movement {
        currency: "GBP",
        ..euros.clone()
    };
    let err = insert_capture_match(&mut tx, batch, 2, &as_pounds, SETTLED_POSITION, step.id)
        .await
        .expect_err("a currency that is not the payment's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_capture_matches_payment_currency_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_fact_settles_once_and_a_settled_fact_backs_one_line(pool: PgPool) -> TestResult {
    // The capture settled by batch 1, then named again by batch 2.
    let (_, capture) = book_two(&pool).await?;
    let step = sqlx::query_scalar!(
        "SELECT id FROM settlement_movement_steps \
          WHERE payment_id = $1 AND position = $2 AND kind = 'settle'",
        capture.payment,
        capture.position
    )
    .fetch_one(&pool)
    .await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 2, 9_835).await?;
    insert_line(&mut tx, batch, 2, "capture", (CAPTURED, FEE, 9_835)).await?;
    let err = insert_capture_match(&mut tx, batch, 2, &capture, SETTLED_POSITION, step)
        .await
        .expect_err("a fact settled twice");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_capture_matches_fact_key");

    // Two captures of one payment, both backed by one settled fact.
    let first = settled_capture(&pool, "EUR", CAPTURED).await?;
    let second = later_movement(&pool, &first, "capture", 3, CAPTURED).await?;
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 3, 19_670).await?;
    insert_line(&mut tx, batch, 2, "capture", (CAPTURED, FEE, 9_835)).await?;
    insert_line(&mut tx, batch, 3, "capture", (CAPTURED, FEE, 9_835)).await?;
    let step = settle(&mut tx, &first).await?;
    insert_capture_match(&mut tx, batch, 2, &first, SETTLED_POSITION, step.id).await?;
    let step = settle(&mut tx, &second).await?;
    let err = insert_capture_match(&mut tx, batch, 3, &second, SETTLED_POSITION, step.id)
        .await
        .expect_err("one settled fact for two lines");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_capture_matches_settled_key",
    );
    Ok(())
}

#[sqlx::test]
async fn a_refund_match_agrees_with_its_line_and_fact(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", CAPTURED).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, REFUNDED).await?;

    // A refund's match on the capture's line.
    let (mut tx, batch, _) = ready_to_match(&pool, "EUR", &capture).await?;
    let step = settle(&mut tx, &refund).await?;
    let err = insert_refund_match(&mut tx, batch, 2, &refund, step.id)
        .await
        .expect_err("a refund's match on a capture's line");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_refund_matches_line_fkey",
    );

    // A refund line of 2,400 for a refund of 2,500.
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 1, -2_400).await?;
    insert_line(&mut tx, batch, 2, "refund", (-2_400, 0, -2_400)).await?;
    let step = settle(&mut tx, &refund).await?;
    let smaller = Movement {
        amount: 2_400,
        ..refund.clone()
    };
    let err = insert_refund_match(&mut tx, batch, 2, &smaller, step.id)
        .await
        .expect_err("an amount that is not the fact's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_refund_matches_fact_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_movements_chain_takes_only_legal_moves(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", CAPTURED).await?;
    let open = MovementStep::open(&capture);
    for (step, code, constraint) in [
        // A first step that is not open.
        (
            MovementStep {
                kind: "settle",
                to: "settled",
                ..open.clone()
            },
            CHECK_VIOLATION,
            "settlement_movement_steps_edge_check",
        ),
        // A move that no chain takes.
        (
            open.then("claim", "claimed"),
            CHECK_VIOLATION,
            "settlement_movement_steps_edge_check",
        ),
        (
            MovementStep {
                prev_seq: 5,
                ..open.then("settle", "settled")
            },
            CHECK_VIOLATION,
            "settlement_movement_steps_seq_check",
        ),
        (
            MovementStep {
                fact_kind: "authorize",
                ..open.clone()
            },
            CHECK_VIOLATION,
            "settlement_movement_steps_fact_kind_check",
        ),
        (
            MovementStep {
                id: VERSION_4,
                ..open.clone()
            },
            CHECK_VIOLATION,
            "settlement_movement_steps_id_check",
        ),
        (
            MovementStep {
                amount: 9_999,
                ..open.clone()
            },
            FOREIGN_KEY_VIOLATION,
            "settlement_movement_steps_fact_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        if step.seq > 0 {
            open.insert(&mut tx).await?;
        }
        let err = step.insert(&mut tx).await.expect_err("an illegal step");
        assert_violation(err, code, constraint);
    }

    // A legal move from a state that the predecessor does not hold.
    let settled = open.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    open.insert(&mut tx).await?;
    settled.insert(&mut tx).await?;
    let err = MovementStep {
        from: "open",
        ..settled.then("settle", "settled")
    }
    .insert(&mut tx)
    .await
    .expect_err("a step from a state that its predecessor does not hold");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_movement_steps_prev_fkey",
    );

    // Two writers of one step.
    let mut tx = runtime_transaction(&pool).await?;
    open.insert(&mut tx).await?;
    let err = MovementStep {
        id: Uuid::now_v7(),
        ..open.clone()
    }
    .insert(&mut tx)
    .await
    .expect_err("a second first step");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_movement_steps_pkey");
    Ok(())
}

#[sqlx::test]
async fn a_settle_step_needs_its_match(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", CAPTURED).await?;
    let mut tx = runtime_transaction(&pool).await?;
    settle(&mut tx, &capture).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_movement_steps_record_check",
        "settlement_movement_steps",
    );

    // The first step alone needs none.
    let mut tx = runtime_transaction(&pool).await?;
    MovementStep::open(&capture).insert(&mut tx).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_booked_batch_never_changes(pool: PgPool) -> TestResult {
    book_two(&pool).await?;

    // As the owner, each table's triggers refuse.
    for (err, trigger, table) in [
        (
            sqlx::query!("UPDATE settlement_batches SET payout = payout")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batches_append_only",
            "settlement_batches",
        ),
        (
            sqlx::query!("TRUNCATE settlement_batches CASCADE")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batches_no_truncate",
            "settlement_batches",
        ),
        (
            sqlx::query!("UPDATE settlement_batch_postings SET batch_id = batch_id")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batch_postings_append_only",
            "settlement_batch_postings",
        ),
        (
            sqlx::query!("TRUNCATE settlement_batch_postings")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batch_postings_no_truncate",
            "settlement_batch_postings",
        ),
        (
            sqlx::query!("UPDATE settlement_lines SET fee = fee")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_lines_append_only",
            "settlement_lines",
        ),
        (
            sqlx::query!("TRUNCATE settlement_lines CASCADE")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_lines_no_truncate",
            "settlement_lines",
        ),
        (
            sqlx::query!("UPDATE settlement_movement_steps SET amount = amount")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_steps_append_only",
            "settlement_movement_steps",
        ),
        (
            sqlx::query!("TRUNCATE settlement_movement_steps CASCADE")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_steps_no_truncate",
            "settlement_movement_steps",
        ),
        (
            sqlx::query!("UPDATE settlement_capture_matches SET gross = gross")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_capture_matches_append_only",
            "settlement_capture_matches",
        ),
        (
            sqlx::query!("TRUNCATE settlement_capture_matches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_capture_matches_no_truncate",
            "settlement_capture_matches",
        ),
        (
            sqlx::query!("UPDATE settlement_refund_matches SET gross = gross")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_refund_matches_append_only",
            "settlement_refund_matches",
        ),
        (
            sqlx::query!("TRUNCATE settlement_refund_matches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_refund_matches_no_truncate",
            "settlement_refund_matches",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, table);
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("UPDATE settlement_batches SET payout = payout")
        .execute(&mut *tx)
        .await
        .expect_err("no UPDATE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_lines")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("TRUNCATE settlement_movement_steps CASCADE")
        .execute(&mut *tx)
        .await
        .expect_err("no TRUNCATE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}
