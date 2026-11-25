// Author: Nicola Antonello

//! Database tests for discrepancies (ADR-0068, ADR-0069, ADR-0072): one row for each, the
//! subject that it is about in a table of its kind, and its chain's first step, with the
//! guards of I12 in PostgreSQL (ADR-0071).
//!
//! Rows are written with raw SQL as the system role, `payments_app`, the way a buggy code
//! path or a manual session could write them. Each rejected write breaks a single rule.
//! The test asserts the SQLSTATE and, where PostgreSQL reports one, the constraint or
//! trigger that rejects the write; privilege errors carry none. Transactions commit
//! through the one function that reads a `COMMIT`'s error (ADR-0014). A refused
//! transaction is rolled back before the next one begins: each test's pool holds five
//! connections, and a shadowed transaction keeps its own until the test ends.

mod common;

use common::payments::VERSION_4;
use common::settlement::{
    MovementStep, SETTLED_POSITION, accounts, batch_with_a_fee_off_the_terms,
    batch_with_a_stray_line, find, insert_batch, insert_capture_match, insert_discrepancy,
    insert_discrepancy_step, insert_fee_difference, insert_line, insert_missing_batch,
    insert_overdue_movement, insert_payout_difference, insert_refund_match,
    insert_uncaused_posting, insert_unmatched_line, later_movement, link_posting, overdue, post,
    settled_capture, uncaused,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_transaction, sqlstate,
};
use payments_core::db::{CommitError, commit};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// SQLSTATE of a discrepancy without its subject or its first step (ADR-0072).
const NOT_WHOLE: &str = "LG012";
/// SQLSTATE of a step without the record that it needs (ADR-0069).
const NO_RECORD: &str = "LG011";
/// SQLSTATE of a fee's difference on a line that no match names (ADR-0072).
const NO_MATCH: &str = "LG013";
/// A time as of which a movement is judged overdue.
const AS_OF: &str = "2026-11-22T00:00:00Z";

/// Commits through the one function that reads a `COMMIT`'s error (ADR-0014), and returns
/// the error of the deferred check that refused it.
async fn commit_refused(tx: Transaction<'static, Postgres>) -> sqlx::Error {
    match commit(tx).await {
        Err(CommitError::Rejected(err)) => err,
        other => panic!("expected a deferred check to refuse the commit, got {other:?}"),
    }
}

#[sqlx::test]
async fn every_kind_of_discrepancy_commits_whole(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    let paid = settled_capture(&pool, "EUR", 10_000).await?;

    // A line that settles nothing of ours, and a fee off the terms on a line that matches.
    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_fee_off_the_terms(&mut tx, &accounts, (1, 10_298), &paid).await?;
    let (line, fee) = (Uuid::now_v7(), Uuid::now_v7());
    insert_discrepancy(&mut tx, line, "unmatched_line").await?;
    insert_unmatched_line(&mut tx, line, (batch, 2, 468)).await?;
    find(&mut tx, line, "unmatched_line").await?;
    insert_discrepancy(&mut tx, fee, "fee_difference").await?;
    insert_fee_difference(&mut tx, fee, (batch, 3), (170, 165)).await?;
    find(&mut tx, fee, "fee_difference").await?;
    commit(tx).await?;

    // A payout of 100 with no lines to explain it.
    let mut tx = runtime_transaction(&pool).await?;
    let batch = Uuid::now_v7();
    insert_batch(&mut tx, batch, "EUR", 2, 100).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 100), (accounts.suspense, -100)],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    let payout = Uuid::now_v7();
    insert_discrepancy(&mut tx, payout, "payout_difference").await?;
    insert_payout_difference(&mut tx, payout, batch, 100).await?;
    find(&mut tx, payout, "payout_difference").await?;
    commit(tx).await?;

    // A missing batch, and a posting that nothing causes.
    let posting = uncaused(&pool, &accounts).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let (missing, stray) = (Uuid::now_v7(), Uuid::now_v7());
    insert_discrepancy(&mut tx, missing, "missing_batch").await?;
    insert_missing_batch(&mut tx, missing, ("mock", "EUR", 5), "not_listed").await?;
    find(&mut tx, missing, "missing_batch").await?;
    insert_discrepancy(&mut tx, stray, "uncaused_posting").await?;
    insert_uncaused_posting(&mut tx, stray, posting).await?;
    find(&mut tx, stray, "uncaused_posting").await?;
    commit(tx).await?;

    // An overdue movement, whose chain is its movement's.
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let step = overdue(&mut tx, &capture).await?;
    let late = Uuid::now_v7();
    insert_discrepancy(&mut tx, late, "overdue_movement").await?;
    insert_overdue_movement(&mut tx, late, &step, AS_OF).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_discrepancy_needs_its_subject_and_its_first_step(pool: PgPool) -> TestResult {
    // Its first step, without its subject, for each kind whose chain is its own.
    for kind in [
        "unmatched_line",
        "payout_difference",
        "fee_difference",
        "missing_batch",
        "uncaused_posting",
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let id = Uuid::now_v7();
        insert_discrepancy(&mut tx, id, kind).await?;
        find(&mut tx, id, kind).await?;
        let err = commit_refused(tx).await;
        assert_guard(
            err,
            NOT_WHOLE,
            "settlement_discrepancies_commit_check",
            "settlement_discrepancies",
        );
    }

    // Its subject, without its first step.
    let mut tx = runtime_transaction(&pool).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "missing_batch").await?;
    insert_missing_batch(&mut tx, id, ("mock", "EUR", 5), "not_listed").await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_WHOLE,
        "settlement_discrepancies_commit_check",
        "settlement_discrepancies",
    );

    // An overdue movement without its subject.
    let mut tx = runtime_transaction(&pool).await?;
    insert_discrepancy(&mut tx, Uuid::now_v7(), "overdue_movement").await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_WHOLE,
        "settlement_discrepancies_commit_check",
        "settlement_discrepancies",
    );
    Ok(())
}

#[sqlx::test]
async fn a_discrepancy_keeps_to_its_rules(pool: PgPool) -> TestResult {
    let mut tx = runtime_transaction(&pool).await?;
    let err = insert_discrepancy(&mut tx, VERSION_4, "missing_batch")
        .await
        .expect_err("a UUID that is not a v7");
    assert_violation(err, CHECK_VIOLATION, "settlement_discrepancies_id_check");
    tx.rollback().await?;
    let mut tx = runtime_transaction(&pool).await?;
    let err = insert_discrepancy(&mut tx, Uuid::now_v7(), "chargeback")
        .await
        .expect_err("a kind that reconciliation does not record");
    assert_violation(err, CHECK_VIOLATION, "settlement_discrepancies_kind_check");
    tx.rollback().await?;

    // A subject in the table of another kind, for each table: the subject's key carries
    // its discrepancy's kind.
    let accounts = accounts(&pool, "EUR").await?;
    let posting = uncaused(&pool, &accounts).await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    for (kind, constraint) in [
        (
            "unmatched_line",
            "settlement_unmatched_lines_discrepancy_fkey",
        ),
        (
            "payout_difference",
            "settlement_payout_differences_discrepancy_fkey",
        ),
        (
            "fee_difference",
            "settlement_fee_differences_discrepancy_fkey",
        ),
        (
            "missing_batch",
            "settlement_missing_batches_discrepancy_fkey",
        ),
        (
            "overdue_movement",
            "settlement_overdue_movements_discrepancy_fkey",
        ),
        (
            "uncaused_posting",
            "settlement_uncaused_postings_discrepancy_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let batch = batch_with_a_stray_line(&mut tx, &accounts, 1).await?;
        let step = overdue(&mut tx, &capture).await?;
        let id = Uuid::now_v7();
        let other = if kind == "missing_batch" {
            "uncaused_posting"
        } else {
            "missing_batch"
        };
        insert_discrepancy(&mut tx, id, other).await?;
        let written = match kind {
            "unmatched_line" => insert_unmatched_line(&mut tx, id, (batch, 2, 468)).await,
            "payout_difference" => insert_payout_difference(&mut tx, id, batch, 7).await,
            "fee_difference" => insert_fee_difference(&mut tx, id, (batch, 2), (32, 25)).await,
            "missing_batch" => {
                insert_missing_batch(&mut tx, id, ("mock", "EUR", 5), "not_listed").await
            }
            "overdue_movement" => insert_overdue_movement(&mut tx, id, &step, AS_OF).await,
            _ => insert_uncaused_posting(&mut tx, id, posting).await,
        };
        let err = written.expect_err("a subject of another kind");
        assert_violation(err, FOREIGN_KEY_VIOLATION, constraint);
    }
    Ok(())
}

#[sqlx::test]
async fn each_subject_is_recorded_once(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());

    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_stray_line(&mut tx, &accounts, 1).await?;
    for (id, kind) in [(first, "unmatched_line"), (second, "unmatched_line")] {
        insert_discrepancy(&mut tx, id, kind).await?;
    }
    insert_unmatched_line(&mut tx, first, (batch, 2, 468)).await?;
    let err = insert_unmatched_line(&mut tx, second, (batch, 2, 468))
        .await
        .expect_err("one line, two discrepancies");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_unmatched_lines_line_key");
    tx.rollback().await?;

    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_stray_line(&mut tx, &accounts, 1).await?;
    for (id, kind) in [(first, "payout_difference"), (second, "payout_difference")] {
        insert_discrepancy(&mut tx, id, kind).await?;
    }
    insert_payout_difference(&mut tx, first, batch, 7).await?;
    let err = insert_payout_difference(&mut tx, second, batch, 7)
        .await
        .expect_err("one payout, two differences");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_payout_differences_batch_key",
    );
    tx.rollback().await?;

    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_stray_line(&mut tx, &accounts, 1).await?;
    for (id, kind) in [(first, "fee_difference"), (second, "fee_difference")] {
        insert_discrepancy(&mut tx, id, kind).await?;
    }
    insert_fee_difference(&mut tx, first, (batch, 2), (32, 25)).await?;
    let err = insert_fee_difference(&mut tx, second, (batch, 2), (32, 25))
        .await
        .expect_err("one fee, two differences");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_fee_differences_line_key");
    tx.rollback().await?;

    let mut tx = runtime_transaction(&pool).await?;
    for (id, kind) in [(first, "missing_batch"), (second, "missing_batch")] {
        insert_discrepancy(&mut tx, id, kind).await?;
    }
    insert_missing_batch(&mut tx, first, ("mock", "EUR", 5), "not_listed").await?;
    let err = insert_missing_batch(&mut tx, second, ("mock", "EUR", 5), "unreadable")
        .await
        .expect_err("one number, missing twice");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_missing_batches_number_key",
    );
    tx.rollback().await?;

    let posting = uncaused(&pool, &accounts).await?;
    let mut tx = runtime_transaction(&pool).await?;
    for (id, kind) in [(first, "uncaused_posting"), (second, "uncaused_posting")] {
        insert_discrepancy(&mut tx, id, kind).await?;
    }
    insert_uncaused_posting(&mut tx, first, posting).await?;
    let err = insert_uncaused_posting(&mut tx, second, posting)
        .await
        .expect_err("one posting, recorded twice");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_uncaused_postings_ledger_transaction_id_key",
    );
    tx.rollback().await?;

    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let step = overdue(&mut tx, &capture).await?;
    for (id, kind) in [(first, "overdue_movement"), (second, "overdue_movement")] {
        insert_discrepancy(&mut tx, id, kind).await?;
    }
    insert_overdue_movement(&mut tx, first, &step, AS_OF).await?;
    let err = insert_overdue_movement(&mut tx, second, &step, AS_OF)
        .await
        .expect_err("one movement, overdue twice");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_overdue_movements_movement_key",
    );
    Ok(())
}

#[sqlx::test]
async fn each_subject_agrees_with_what_it_is_about(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;

    // A line's discrepancy carries the line's net, and is found in its batch's transaction.
    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_stray_line(&mut tx, &accounts, 1).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "unmatched_line").await?;
    let err = insert_unmatched_line(&mut tx, id, (batch, 2, 500))
        .await
        .expect_err("a net that is not the line's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_unmatched_lines_line_fkey",
    );
    tx.rollback().await?;
    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_stray_line(&mut tx, &accounts, 1).await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "unmatched_line").await?;
    let err = insert_unmatched_line(&mut tx, id, (batch, 2, 468))
        .await
        .expect_err("a line's discrepancy after its batch's transaction");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_unmatched_lines_batch_fkey",
    );
    tx.rollback().await?;

    // A payout's difference is never zero, and is found in its batch's transaction.
    let mut tx = runtime_transaction(&pool).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "payout_difference").await?;
    let err = insert_payout_difference(&mut tx, id, batch, 0)
        .await
        .expect_err("a difference of nothing");
    assert_violation(
        err,
        CHECK_VIOLATION,
        "settlement_payout_differences_amount_check",
    );
    tx.rollback().await?;
    let mut tx = runtime_transaction(&pool).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "payout_difference").await?;
    let err = insert_payout_difference(&mut tx, id, batch, 7)
        .await
        .expect_err("a payout's difference after its batch's transaction");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_payout_differences_batch_fkey",
    );
    tx.rollback().await?;

    // A fee's difference carries the fee charged on the line, which differs from the
    // agreed one.
    for ((charged, agreed), code, constraint) in [
        (
            (31, 25),
            FOREIGN_KEY_VIOLATION,
            "settlement_fee_differences_line_fkey",
        ),
        (
            (32, 32),
            CHECK_VIOLATION,
            "settlement_fee_differences_differs_check",
        ),
        (
            (32, -1),
            CHECK_VIOLATION,
            "settlement_fee_differences_agreed_check",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let id = Uuid::now_v7();
        insert_discrepancy(&mut tx, id, "fee_difference").await?;
        let err = insert_fee_difference(&mut tx, id, (batch, 2), (charged, agreed))
            .await
            .expect_err("a fee's difference that breaks a rule");
        assert_violation(err, code, constraint);
    }

    // A missing batch names a PSP, a currency and a number that the service knows, and a
    // cause.
    for ((key, cause), code, constraint) in [
        (
            (("mock", "EUR", 5), "gone"),
            CHECK_VIOLATION,
            "settlement_missing_batches_cause_check",
        ),
        (
            (("mock", "EUR", 0), "not_listed"),
            CHECK_VIOLATION,
            "settlement_missing_batches_number_check",
        ),
        (
            (("other", "EUR", 5), "not_listed"),
            CHECK_VIOLATION,
            "settlement_missing_batches_psp_check",
        ),
        (
            (("mock", "XXX", 5), "not_listed"),
            FOREIGN_KEY_VIOLATION,
            "settlement_missing_batches_currency_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let id = Uuid::now_v7();
        insert_discrepancy(&mut tx, id, "missing_batch").await?;
        let err = insert_missing_batch(&mut tx, id, key, cause)
            .await
            .expect_err("a missing batch that breaks a rule");
        assert_violation(err, code, constraint);
    }

    // A posting that nothing causes is one that the ledger holds.
    let mut tx = runtime_transaction(&pool).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "uncaused_posting").await?;
    let err = insert_uncaused_posting(&mut tx, id, Uuid::now_v7())
        .await
        .expect_err("a posting that the ledger does not hold");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_uncaused_postings_ledger_transaction_fkey",
    );
    tx.rollback().await?;

    // An overdue movement names its overdue step, as of a time within RFC 3339's years.
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let open = MovementStep::open(&capture);
    open.insert(&mut tx).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "overdue_movement").await?;
    let err = insert_overdue_movement(&mut tx, id, &open, AS_OF)
        .await
        .expect_err("a step that is not an overdue step");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_overdue_movements_step_fkey",
    );
    tx.rollback().await?;
    let mut tx = runtime_transaction(&pool).await?;
    let step = overdue(&mut tx, &capture).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "overdue_movement").await?;
    let err = insert_overdue_movement(&mut tx, id, &step, "10000-01-01T00:00:00Z")
        .await
        .expect_err("a time past RFC 3339's years");
    assert_violation(
        err,
        CHECK_VIOLATION,
        "settlement_overdue_movements_as_of_check",
    );
    Ok(())
}

#[sqlx::test]
async fn a_fee_difference_is_on_a_line_that_matches(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;

    // On line 2 of a batch, a refund line that matches, where the terms charge nothing.
    // The difference is written before its match: the check runs at COMMIT.
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, 2_500).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let batch = Uuid::now_v7();
    insert_batch(&mut tx, batch, "EUR", 1, -2_505).await?;
    insert_line(&mut tx, batch, 2, "refund", (-2_500, 5, -2_505)).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "fee_difference").await?;
    find(&mut tx, id, "fee_difference").await?;
    insert_fee_difference(&mut tx, id, (batch, 2), (5, 0)).await?;
    let open = MovementStep::open(&refund);
    let settle = open.then("settle", "settled");
    open.insert(&mut tx).await?;
    settle.insert(&mut tx).await?;
    insert_refund_match(&mut tx, batch, 2, &refund, settle.id).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (accounts.payouts, -2_505),
            (accounts.fees, 5),
            (accounts.receivable, 2_500),
        ],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    commit(tx).await?;

    // On a line that settles nothing of ours, whose fee is neither booked nor checked
    // (ADR-0068), though the next line of its batch matches, and so does line 2 of the
    // batch above.
    let paid = settled_capture(&pool, "EUR", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_fee_off_the_terms(&mut tx, &accounts, (2, 10_298), &paid).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "fee_difference").await?;
    find(&mut tx, id, "fee_difference").await?;
    insert_fee_difference(&mut tx, id, (batch, 2), (32, 25)).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_MATCH,
        "settlement_fee_differences_commit_check",
        "settlement_fee_differences",
    );
    Ok(())
}

#[sqlx::test]
async fn an_overdue_step_needs_its_subject_and_may_settle_later(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    overdue(&mut tx, &capture).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_movement_steps_record_check",
        "settlement_movement_steps",
    );

    // Found overdue, then settled by a later batch's line.
    let accounts = accounts(&pool, "EUR").await?;
    let mut tx = runtime_transaction(&pool).await?;
    let step = overdue(&mut tx, &capture).await?;
    let id = Uuid::now_v7();
    insert_discrepancy(&mut tx, id, "overdue_movement").await?;
    insert_overdue_movement(&mut tx, id, &step, AS_OF).await?;
    commit(tx).await?;
    let settle = step.then("settle", "settled");
    let batch = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 1, 9_835).await?;
    insert_line(&mut tx, batch, 2, "capture", (10_000, 165, 9_835)).await?;
    settle.insert(&mut tx).await?;
    insert_capture_match(&mut tx, batch, 2, &capture, SETTLED_POSITION, settle.id).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (accounts.payouts, 9_835),
            (accounts.fees, 165),
            (accounts.receivable, -10_000),
        ],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_movement_goes_overdue_once_and_never_after_it_settles(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let open = MovementStep::open(&capture);
    for before in [
        open.then("overdue", "overdue"),
        open.then("settle", "settled"),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        open.insert(&mut tx).await?;
        before.insert(&mut tx).await?;
        let err = before
            .then("overdue", "overdue")
            .insert(&mut tx)
            .await
            .expect_err("a movement found overdue again, or after it settled");
        assert_violation(err, CHECK_VIOLATION, "settlement_movement_steps_edge_check");
    }
    Ok(())
}

#[sqlx::test]
async fn a_discrepancys_chain_starts_with_its_finding(pool: PgPool) -> TestResult {
    // Each in a transaction of its own, after a discrepancy of `kind`.
    for (kind, (seq, prev_seq, id), step_kind, discrepancy_kind, code, constraint) in [
        // A first step that is not a finding.
        (
            "missing_batch",
            (0, 0, Uuid::now_v7()),
            "accept",
            "missing_batch",
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_edge_check",
        ),
        // A step for an overdue movement, whose chain is its movement's.
        (
            "overdue_movement",
            (0, 0, Uuid::now_v7()),
            "find",
            "overdue_movement",
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_edge_check",
        ),
        (
            "missing_batch",
            (0, 3, Uuid::now_v7()),
            "find",
            "missing_batch",
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_seq_check",
        ),
        (
            "missing_batch",
            (0, 0, VERSION_4),
            "find",
            "missing_batch",
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_id_check",
        ),
        // A kind that is not its discrepancy's.
        (
            "missing_batch",
            (0, 0, Uuid::now_v7()),
            "find",
            "uncaused_posting",
            FOREIGN_KEY_VIOLATION,
            "settlement_discrepancy_steps_discrepancy_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let discrepancy = Uuid::now_v7();
        insert_discrepancy(&mut tx, discrepancy, kind).await?;
        let err = insert_discrepancy_step(
            &mut tx,
            discrepancy,
            (seq, prev_seq, id),
            (step_kind, "open", "open"),
            discrepancy_kind,
        )
        .await
        .expect_err("a step that breaks a rule");
        assert_violation(err, code, constraint);
    }

    // A second finding, and two writers of the first step.
    let mut tx = runtime_transaction(&pool).await?;
    let discrepancy = Uuid::now_v7();
    insert_discrepancy(&mut tx, discrepancy, "missing_batch").await?;
    find(&mut tx, discrepancy, "missing_batch").await?;
    let err = insert_discrepancy_step(
        &mut tx,
        discrepancy,
        (1, 0, Uuid::now_v7()),
        ("find", "open", "open"),
        "missing_batch",
    )
    .await
    .expect_err("a finding after the first step");
    assert_violation(
        err,
        CHECK_VIOLATION,
        "settlement_discrepancy_steps_edge_check",
    );
    let mut tx = runtime_transaction(&pool).await?;
    insert_discrepancy(&mut tx, discrepancy, "missing_batch").await?;
    find(&mut tx, discrepancy, "missing_batch").await?;
    let err = find(&mut tx, discrepancy, "missing_batch")
        .await
        .expect_err("a second first step");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_discrepancy_steps_pkey");
    Ok(())
}

#[sqlx::test]
async fn a_discrepancy_never_changes(pool: PgPool) -> TestResult {
    let accounts = accounts(&pool, "EUR").await?;
    let posting = uncaused(&pool, &accounts).await?;
    let paid = settled_capture(&pool, "EUR", 10_000).await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let late = Uuid::now_v7();

    // A discrepancy of each kind, with a batch whose payout is 7 more than its nets.
    let mut tx = runtime_transaction(&pool).await?;
    let batch = batch_with_a_fee_off_the_terms(&mut tx, &accounts, (1, 10_305), &paid).await?;
    for (kind, id) in [
        ("unmatched_line", Uuid::now_v7()),
        ("payout_difference", Uuid::now_v7()),
        ("fee_difference", Uuid::now_v7()),
        ("missing_batch", Uuid::now_v7()),
        ("uncaused_posting", Uuid::now_v7()),
    ] {
        insert_discrepancy(&mut tx, id, kind).await?;
        find(&mut tx, id, kind).await?;
        match kind {
            "unmatched_line" => insert_unmatched_line(&mut tx, id, (batch, 2, 468)).await?,
            "payout_difference" => insert_payout_difference(&mut tx, id, batch, 7).await?,
            "fee_difference" => insert_fee_difference(&mut tx, id, (batch, 3), (170, 165)).await?,
            "missing_batch" => {
                insert_missing_batch(&mut tx, id, ("mock", "EUR", 5), "not_listed").await?;
            }
            _ => insert_uncaused_posting(&mut tx, id, posting).await?,
        }
    }
    let step = overdue(&mut tx, &capture).await?;
    insert_discrepancy(&mut tx, late, "overdue_movement").await?;
    insert_overdue_movement(&mut tx, late, &step, AS_OF).await?;
    commit(tx).await?;

    // As the owner, each table's triggers refuse.
    for (err, trigger, table) in [
        (
            sqlx::query!("UPDATE settlement_discrepancies SET kind = kind")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancies_append_only",
            "settlement_discrepancies",
        ),
        (
            sqlx::query!("TRUNCATE settlement_discrepancies CASCADE")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancies_no_truncate",
            "settlement_discrepancies",
        ),
        (
            sqlx::query!("UPDATE settlement_unmatched_lines SET net = net")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_unmatched_lines_append_only",
            "settlement_unmatched_lines",
        ),
        (
            sqlx::query!("TRUNCATE settlement_unmatched_lines")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_unmatched_lines_no_truncate",
            "settlement_unmatched_lines",
        ),
        (
            sqlx::query!("UPDATE settlement_payout_differences SET amount = amount")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_payout_differences_append_only",
            "settlement_payout_differences",
        ),
        (
            sqlx::query!("TRUNCATE settlement_payout_differences")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_payout_differences_no_truncate",
            "settlement_payout_differences",
        ),
        (
            sqlx::query!("UPDATE settlement_fee_differences SET agreed = agreed")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_fee_differences_append_only",
            "settlement_fee_differences",
        ),
        (
            sqlx::query!("TRUNCATE settlement_fee_differences")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_fee_differences_no_truncate",
            "settlement_fee_differences",
        ),
        (
            sqlx::query!("UPDATE settlement_missing_batches SET cause = cause")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_missing_batches_append_only",
            "settlement_missing_batches",
        ),
        (
            sqlx::query!("TRUNCATE settlement_missing_batches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_missing_batches_no_truncate",
            "settlement_missing_batches",
        ),
        (
            sqlx::query!("UPDATE settlement_overdue_movements SET as_of = as_of")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_overdue_movements_append_only",
            "settlement_overdue_movements",
        ),
        (
            sqlx::query!("TRUNCATE settlement_overdue_movements")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_overdue_movements_no_truncate",
            "settlement_overdue_movements",
        ),
        (
            sqlx::query!(
                "UPDATE settlement_uncaused_postings \
                    SET ledger_transaction_id = ledger_transaction_id"
            )
            .execute(&pool)
            .await
            .expect_err("append-only"),
            "settlement_uncaused_postings_append_only",
            "settlement_uncaused_postings",
        ),
        (
            sqlx::query!("TRUNCATE settlement_uncaused_postings")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_uncaused_postings_no_truncate",
            "settlement_uncaused_postings",
        ),
        (
            sqlx::query!("UPDATE settlement_discrepancy_steps SET kind = kind")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_steps_append_only",
            "settlement_discrepancy_steps",
        ),
        (
            sqlx::query!("TRUNCATE settlement_discrepancy_steps")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_steps_no_truncate",
            "settlement_discrepancy_steps",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, table);
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_discrepancy_steps")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}
