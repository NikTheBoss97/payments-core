// Author: Nicola Antonello

//! Database tests for resolving discrepancies (ADR-0069, ADR-0072): an operator's outcome on
//! a discrepancy whose chain is its own, and on an overdue movement, each with its posting,
//! and the guards of I13 in PostgreSQL (ADR-0071).
//!
//! Rows are written with raw SQL as the system role, `payments_app`, the way a buggy code
//! path or a manual session could write them. Each rejected write breaks a single rule.
//! The test asserts the SQLSTATE and, where PostgreSQL reports one, the constraint or
//! trigger that rejects the write; privilege errors carry none. Transactions commit
//! through the one function that reads a `COMMIT`'s error (ADR-0014). A refused
//! transaction is rolled back before the next one begins: each test's pool holds five
//! connections, and a shadowed transaction keeps its own until the test ends.

mod common;

use common::settlement::{
    Accounts, Movement, MovementStep, accounts, batch_with_a_fee_off_the_terms, find, insert_batch,
    insert_discrepancy, insert_discrepancy_step, insert_fee_difference, insert_line,
    insert_missing_batch, insert_overdue_movement, insert_payout_difference,
    insert_uncaused_posting, insert_unmatched_line, later_movement, overdue, post, settled_capture,
    uncaused,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_transaction, sqlstate,
};
use payments_core::db::{CommitError, commit};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// SQLSTATE of a record whose posting is not exactly its lines (ADR-0072).
const NOT_ITS_LINES: &str = "LG010";
/// SQLSTATE of a step without the record that it needs (ADR-0069).
const NO_RECORD: &str = "LG011";
/// A reason, as an operator gives one.
const REASON: &str = "The PSP confirmed the charge in writing.";
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

/// An operator's outcome of `kind` on `discrepancy`, the record of its step `step`.
async fn insert_discrepancy_outcome(
    conn: &mut PgConnection,
    step: Uuid,
    discrepancy: Uuid,
    kind: &str,
    reason: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_discrepancy_outcomes (step_id, discrepancy_id, kind, reason) \
         VALUES ($1, $2, $3, $4)",
        step,
        discrepancy,
        kind,
        reason
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The link from the outcome `step`, of `kind`, to its posting.
async fn link_outcome_posting(
    conn: &mut PgConnection,
    step: Uuid,
    kind: &str,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_discrepancy_outcome_postings \
             (step_id, kind, ledger_transaction_id) \
         VALUES ($1, $2, $3)",
        step,
        kind,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The discrepancies of each kind whose chain is their own, found with one batch.
struct Found {
    line: Uuid,
    payout: Uuid,
    fee: Uuid,
    missing: Uuid,
    stray: Uuid,
}

/// Books batch `number` in EUR with a line of 468 that settles nothing of ours, a payout 7
/// more than its nets and a fee of 170 where the terms give 165, and records a discrepancy
/// of each kind whose chain is its own, the missing batch numbered 100 more, each found and
/// committed.
async fn found(pool: &PgPool, accounts: &Accounts, number: i64) -> sqlx::Result<Found> {
    let posting = uncaused(pool, accounts).await?;
    let paid = settled_capture(pool, "EUR", 10_000).await?;
    let found = Found {
        line: Uuid::now_v7(),
        payout: Uuid::now_v7(),
        fee: Uuid::now_v7(),
        missing: Uuid::now_v7(),
        stray: Uuid::now_v7(),
    };
    let mut tx = runtime_transaction(pool).await?;
    let batch = batch_with_a_fee_off_the_terms(&mut tx, accounts, (number, 10_305), &paid).await?;
    for (id, kind) in [
        (found.line, "unmatched_line"),
        (found.payout, "payout_difference"),
        (found.fee, "fee_difference"),
        (found.missing, "missing_batch"),
        (found.stray, "uncaused_posting"),
    ] {
        insert_discrepancy(&mut tx, id, kind).await?;
        find(&mut tx, id, kind).await?;
    }
    insert_unmatched_line(&mut tx, found.line, (batch, 2, 468)).await?;
    insert_payout_difference(&mut tx, found.payout, batch, 7).await?;
    insert_fee_difference(&mut tx, found.fee, (batch, 3), (170, 165)).await?;
    let missing = number.checked_add(100).expect("a small number");
    insert_missing_batch(
        &mut tx,
        found.missing,
        ("mock", "EUR", missing),
        "not_listed",
    )
    .await?;
    insert_uncaused_posting(&mut tx, found.stray, posting).await?;
    tx.commit().await?;
    Ok(found)
}

/// The outcome of `kind`, to state `to`, after the first step of `discrepancy`, a
/// discrepancy of `discrepancy_kind`: its step and its record. Returns the step.
async fn outcome(
    conn: &mut PgConnection,
    (discrepancy, discrepancy_kind): (Uuid, &str),
    (kind, to): (&str, &str),
    reason: &str,
) -> sqlx::Result<Uuid> {
    let step = Uuid::now_v7();
    insert_discrepancy_step(
        conn,
        discrepancy,
        (1, 0, step),
        (kind, "open", to),
        discrepancy_kind,
    )
    .await?;
    insert_discrepancy_outcome(conn, step, discrepancy, kind, reason).await?;
    Ok(step)
}

/// Posts `entries` in `currency` and links them to the outcome `step` of `kind`. Returns
/// the posting.
async fn posts(
    conn: &mut PgConnection,
    (step, kind): (Uuid, &str),
    currency: &str,
    entries: &[(Uuid, i64)],
) -> sqlx::Result<Uuid> {
    let posting = post(conn, currency, entries).await?;
    link_outcome_posting(conn, step, kind, posting).await?;
    Ok(posting)
}

#[sqlx::test]
async fn every_outcome_that_a_kind_allows_commits_whole(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let first = found(&pool, &euros, 1).await?;
    let second = found(&pool, &euros, 2).await?;
    // Each moves its amount from where booking left it (ADR-0069).
    for ((discrepancy, discrepancy_kind), (kind, to), entries) in [
        (
            (first.line, "unmatched_line"),
            ("claim", "claimed"),
            vec![(euros.suspense, 468), (euros.claims, -468)],
        ),
        (
            (second.line, "unmatched_line"),
            ("write_off", "written_off"),
            vec![(euros.suspense, 468), (euros.writeoffs, -468)],
        ),
        (
            (first.payout, "payout_difference"),
            ("claim", "claimed"),
            vec![(euros.suspense, 7), (euros.claims, -7)],
        ),
        (
            (second.payout, "payout_difference"),
            ("write_off", "written_off"),
            vec![(euros.suspense, 7), (euros.writeoffs, -7)],
        ),
        (
            (first.fee, "fee_difference"),
            ("claim", "claimed"),
            vec![(euros.fees, -5), (euros.claims, 5)],
        ),
        (
            (second.fee, "fee_difference"),
            ("accept", "accepted"),
            vec![],
        ),
        (
            (first.missing, "missing_batch"),
            ("accept", "accepted"),
            vec![],
        ),
        (
            (first.stray, "uncaused_posting"),
            ("accept", "accepted"),
            vec![],
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let step = outcome(&mut tx, (discrepancy, discrepancy_kind), (kind, to), REASON).await?;
        if !entries.is_empty() {
            posts(&mut tx, (step, kind), "EUR", &entries).await?;
        }
        commit(tx).await?;
    }
    Ok(())
}

#[sqlx::test]
async fn an_outcome_that_its_kind_does_not_allow_fails_the_edge_check(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    for ((discrepancy, discrepancy_kind), (kind, to)) in [
        ((found.line, "unmatched_line"), ("accept", "accepted")),
        ((found.payout, "payout_difference"), ("accept", "accepted")),
        ((found.fee, "fee_difference"), ("write_off", "written_off")),
        ((found.missing, "missing_batch"), ("claim", "claimed")),
        (
            (found.missing, "missing_batch"),
            ("write_off", "written_off"),
        ),
        ((found.stray, "uncaused_posting"), ("claim", "claimed")),
        (
            (found.stray, "uncaused_posting"),
            ("write_off", "written_off"),
        ),
        // A move that no chain takes.
        ((found.line, "unmatched_line"), ("claim", "accepted")),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let err = insert_discrepancy_step(
            &mut tx,
            discrepancy,
            (1, 0, Uuid::now_v7()),
            (kind, "open", to),
            discrepancy_kind,
        )
        .await
        .expect_err("an outcome that its kind does not allow");
        assert_violation(
            err,
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_edge_check",
        );
    }

    // A second outcome after the first.
    let mut tx = runtime_transaction(&pool).await?;
    let step = outcome(
        &mut tx,
        (found.line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    posts(
        &mut tx,
        (step, "claim"),
        "EUR",
        &[(euros.suspense, 468), (euros.claims, -468)],
    )
    .await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let err = insert_discrepancy_step(
        &mut tx,
        found.line,
        (2, 1, Uuid::now_v7()),
        ("write_off", "claimed", "written_off"),
        "unmatched_line",
    )
    .await
    .expect_err("a second outcome");
    assert_violation(
        err,
        CHECK_VIOLATION,
        "settlement_discrepancy_steps_edge_check",
    );
    Ok(())
}

#[sqlx::test]
async fn an_outcomes_step_needs_its_record(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    let mut tx = runtime_transaction(&pool).await?;
    insert_discrepancy_step(
        &mut tx,
        found.missing,
        (1, 0, Uuid::now_v7()),
        ("accept", "open", "accepted"),
        "missing_batch",
    )
    .await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_discrepancy_steps_record_check",
        "settlement_discrepancy_steps",
    );
    Ok(())
}

#[sqlx::test]
async fn an_outcome_keeps_to_its_rules(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    let too_long = "x".repeat(501);
    for (kind, reason, code, constraint) in [
        (
            "accept",
            "",
            CHECK_VIOLATION,
            "settlement_discrepancy_outcomes_reason_check",
        ),
        (
            "accept",
            too_long.as_str(),
            CHECK_VIOLATION,
            "settlement_discrepancy_outcomes_reason_check",
        ),
        // A record of the finding.
        (
            "find",
            REASON,
            CHECK_VIOLATION,
            "settlement_discrepancy_outcomes_kind_check",
        ),
        // A record that is not its step's kind.
        (
            "claim",
            REASON,
            FOREIGN_KEY_VIOLATION,
            "settlement_discrepancy_outcomes_step_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let step = Uuid::now_v7();
        insert_discrepancy_step(
            &mut tx,
            found.missing,
            (1, 0, step),
            ("accept", "open", "accepted"),
            "missing_batch",
        )
        .await?;
        let err = insert_discrepancy_outcome(&mut tx, step, found.missing, kind, reason)
            .await
            .expect_err("an outcome that breaks a rule");
        assert_violation(err, code, constraint);
    }

    // A reason of 500 characters, however many bytes.
    let mut tx = runtime_transaction(&pool).await?;
    let longest = "é".repeat(500);
    outcome(
        &mut tx,
        (found.missing, "missing_batch"),
        ("accept", "accepted"),
        &longest,
    )
    .await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_claim_or_a_write_off_posts_exactly_its_lines(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let pounds = accounts(&pool, "GBP").await?;
    let found = found(&pool, &euros, 1).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let earlier = post(
        &mut tx,
        "EUR",
        &[(euros.suspense, 468), (euros.claims, -468)],
    )
    .await?;
    tx.commit().await?;

    // Each posting balances, so only LG010 can refuse it.
    for ((discrepancy, discrepancy_kind), (currency, entries)) in [
        // No posting at all.
        ((found.line, "unmatched_line"), ("EUR", vec![])),
        // One too little.
        (
            (found.line, "unmatched_line"),
            ("EUR", vec![(euros.suspense, 467), (euros.claims, -467)]),
        ),
        // A write-off's lines.
        (
            (found.line, "unmatched_line"),
            ("EUR", vec![(euros.suspense, 468), (euros.writeoffs, -468)]),
        ),
        // Two entries more.
        (
            (found.line, "unmatched_line"),
            (
                "EUR",
                vec![
                    (euros.suspense, 468),
                    (euros.claims, -468),
                    (euros.suspense, 1),
                    (euros.claims, -1),
                ],
            ),
        ),
        // The right amounts, in another currency.
        (
            (found.line, "unmatched_line"),
            ("GBP", vec![(pounds.suspense, 468), (pounds.claims, -468)]),
        ),
        // A fee's difference turned the wrong way.
        (
            (found.fee, "fee_difference"),
            ("EUR", vec![(euros.fees, 5), (euros.claims, -5)]),
        ),
        // A payout's difference out of the fees.
        (
            (found.payout, "payout_difference"),
            ("EUR", vec![(euros.fees, 7), (euros.claims, -7)]),
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let step = outcome(
            &mut tx,
            (discrepancy, discrepancy_kind),
            ("claim", "claimed"),
            REASON,
        )
        .await?;
        if !entries.is_empty() {
            posts(&mut tx, (step, "claim"), currency, &entries).await?;
        }
        let err = commit_refused(tx).await;
        assert_guard(
            err,
            NOT_ITS_LINES,
            "settlement_discrepancy_outcomes_commit_check",
            "settlement_discrepancy_outcomes",
        );
    }

    // The right lines, committed before the outcome.
    let mut tx = runtime_transaction(&pool).await?;
    let step = outcome(
        &mut tx,
        (found.line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    link_outcome_posting(&mut tx, step, "claim", earlier).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_discrepancy_outcomes_commit_check",
        "settlement_discrepancy_outcomes",
    );
    Ok(())
}

#[sqlx::test]
async fn an_acceptance_posts_nothing(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    for (kind, code, constraint) in [
        (
            "accept",
            CHECK_VIOLATION,
            "settlement_discrepancy_outcome_postings_kind_check",
        ),
        (
            "claim",
            FOREIGN_KEY_VIOLATION,
            "settlement_discrepancy_outcome_postings_outcome_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let step = outcome(
            &mut tx,
            (found.missing, "missing_batch"),
            ("accept", "accepted"),
            REASON,
        )
        .await?;
        let posting = post(&mut tx, "EUR", &[(euros.suspense, 1), (euros.claims, -1)]).await?;
        let err = link_outcome_posting(&mut tx, step, kind, posting)
            .await
            .expect_err("a posting for an acceptance");
        assert_violation(err, code, constraint);
    }
    Ok(())
}

#[sqlx::test]
async fn a_claim_that_moves_nothing_has_no_posting_and_none_joins_it_later(
    pool: PgPool,
) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    // A line of 25 whose fee is 25, so its net and every entry of its batch are zero.
    let (batch, line) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 1, 0).await?;
    insert_line(&mut tx, batch, 2, "capture", (25, 25, 0)).await?;
    insert_discrepancy(&mut tx, line, "unmatched_line").await?;
    find(&mut tx, line, "unmatched_line").await?;
    insert_unmatched_line(&mut tx, line, (batch, 2, 0)).await?;
    commit(tx).await?;

    // A posting for it is refused at COMMIT.
    let mut tx = runtime_transaction(&pool).await?;
    let step = outcome(
        &mut tx,
        (line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    posts(
        &mut tx,
        (step, "claim"),
        "EUR",
        &[(euros.suspense, 1), (euros.claims, -1)],
    )
    .await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_discrepancy_outcomes_commit_check",
        "settlement_discrepancy_outcomes",
    );

    // Without one it commits, and none joins it in a later transaction.
    let mut tx = runtime_transaction(&pool).await?;
    let step = outcome(
        &mut tx,
        (line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let err = posts(
        &mut tx,
        (step, "claim"),
        "EUR",
        &[(euros.suspense, 1), (euros.claims, -1)],
    )
    .await
    .expect_err("a posting after its outcome's transaction");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_discrepancy_outcome_postings_outcome_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn each_outcome_is_recorded_once(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let claimed = outcome(
        &mut tx,
        (found.line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    let posting = posts(
        &mut tx,
        (claimed, "claim"),
        "EUR",
        &[(euros.suspense, 468), (euros.claims, -468)],
    )
    .await?;
    commit(tx).await?;

    let mut tx = runtime_transaction(&pool).await?;
    let err = insert_discrepancy_outcome(&mut tx, claimed, found.line, "claim", REASON)
        .await
        .expect_err("a second record of one step");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_discrepancy_outcomes_pkey",
    );
    tx.rollback().await?;
    let mut tx = runtime_transaction(&pool).await?;
    let err = link_outcome_posting(&mut tx, claimed, "claim", posting)
        .await
        .expect_err("a second link of one outcome");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_discrepancy_outcome_postings_pkey",
    );
    tx.rollback().await?;

    // One posting linked to two outcomes.
    let mut tx = runtime_transaction(&pool).await?;
    let step = outcome(
        &mut tx,
        (found.payout, "payout_difference"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    let err = link_outcome_posting(&mut tx, step, "claim", posting)
        .await
        .expect_err("one posting for two outcomes");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_discrepancy_outcome_postings_ledger_transaction_key",
    );
    Ok(())
}

#[sqlx::test]
async fn an_outcome_never_changes(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let step = outcome(
        &mut tx,
        (found.line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    posts(
        &mut tx,
        (step, "claim"),
        "EUR",
        &[(euros.suspense, 468), (euros.claims, -468)],
    )
    .await?;
    commit(tx).await?;

    // As the owner, each table's triggers refuse.
    for (err, trigger, table) in [
        (
            sqlx::query!("DELETE FROM settlement_discrepancy_outcomes")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_outcomes_append_only",
            "settlement_discrepancy_outcomes",
        ),
        (
            sqlx::query!("UPDATE settlement_discrepancy_outcomes SET reason = reason")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_outcomes_append_only",
            "settlement_discrepancy_outcomes",
        ),
        (
            sqlx::query!("TRUNCATE settlement_discrepancy_outcomes CASCADE")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_outcomes_no_truncate",
            "settlement_discrepancy_outcomes",
        ),
        (
            sqlx::query!("DELETE FROM settlement_discrepancy_outcome_postings")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_outcome_postings_append_only",
            "settlement_discrepancy_outcome_postings",
        ),
        (
            sqlx::query!("UPDATE settlement_discrepancy_outcome_postings SET kind = kind")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_outcome_postings_append_only",
            "settlement_discrepancy_outcome_postings",
        ),
        (
            sqlx::query!("TRUNCATE settlement_discrepancy_outcome_postings")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_outcome_postings_no_truncate",
            "settlement_discrepancy_outcome_postings",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, table);
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_discrepancy_outcomes")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

/// An operator's outcome of `kind` on a movement, the record of its step `step`, with its
/// posting.
async fn insert_movement_outcome(
    conn: &mut PgConnection,
    step: &MovementStep,
    (kind, reason): (&str, &str),
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_movement_outcomes \
             (step_id, payment_id, position, kind, reason, ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        step.id,
        step.payment,
        step.position,
        kind,
        reason,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Finds `movement` overdue, with its discrepancy, and commits. Returns the overdue step.
async fn found_overdue(pool: &PgPool, movement: &Movement) -> sqlx::Result<MovementStep> {
    let mut tx = runtime_transaction(pool).await?;
    let step = overdue(&mut tx, movement).await?;
    let discrepancy = Uuid::now_v7();
    insert_discrepancy(&mut tx, discrepancy, "overdue_movement").await?;
    insert_overdue_movement(&mut tx, discrepancy, &step, AS_OF).await?;
    tx.commit().await?;
    Ok(step)
}

#[sqlx::test]
async fn an_overdue_movement_is_claimed_or_written_off_whole(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, 2_500).await?;
    // A capture left its amount owed by the PSP, and a refund its amount owed to the PSP
    // (ADR-0067).
    for (movement, (kind, to), entries) in [
        (
            &capture,
            ("claim", "claimed"),
            [(euros.receivable, -10_000), (euros.claims, 10_000)],
        ),
        (
            &other,
            ("write_off", "written_off"),
            [(euros.receivable, -10_000), (euros.writeoffs, 10_000)],
        ),
        (
            &refund,
            ("claim", "claimed"),
            [(euros.receivable, 2_500), (euros.claims, -2_500)],
        ),
    ] {
        let outcome = found_overdue(&pool, movement).await?.then(kind, to);
        let mut tx = runtime_transaction(&pool).await?;
        outcome.insert(&mut tx).await?;
        let posting = post(&mut tx, "EUR", &entries).await?;
        insert_movement_outcome(&mut tx, &outcome, (kind, REASON), posting).await?;
        commit(tx).await?;
    }
    Ok(())
}

#[sqlx::test]
async fn a_movement_outcome_posts_exactly_its_lines(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let pounds = accounts(&pool, "GBP").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = found_overdue(&pool, &capture)
        .await?
        .then("claim", "claimed");
    let mut tx = runtime_transaction(&pool).await?;
    let earlier = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    tx.commit().await?;

    // Each posting balances, so only LG010 can refuse it.
    for (currency, entries) in [
        // One too little.
        (
            "EUR",
            vec![(euros.receivable, -9_999), (euros.claims, 9_999)],
        ),
        // Turned the wrong way.
        (
            "EUR",
            vec![(euros.receivable, 10_000), (euros.claims, -10_000)],
        ),
        // A write-off's lines.
        (
            "EUR",
            vec![(euros.receivable, -10_000), (euros.writeoffs, 10_000)],
        ),
        // Two entries more.
        (
            "EUR",
            vec![
                (euros.receivable, -10_000),
                (euros.claims, 10_000),
                (euros.suspense, 1),
                (euros.claims, -1),
            ],
        ),
        // The right amounts, in another currency.
        (
            "GBP",
            vec![(pounds.receivable, -10_000), (pounds.claims, 10_000)],
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        claim.insert(&mut tx).await?;
        let posting = post(&mut tx, currency, &entries).await?;
        insert_movement_outcome(&mut tx, &claim, ("claim", REASON), posting).await?;
        let err = commit_refused(tx).await;
        assert_guard(
            err,
            NOT_ITS_LINES,
            "settlement_movement_outcomes_commit_check",
            "settlement_movement_outcomes",
        );
    }

    // The right lines, committed before the outcome.
    let mut tx = runtime_transaction(&pool).await?;
    claim.insert(&mut tx).await?;
    insert_movement_outcome(&mut tx, &claim, ("claim", REASON), earlier).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_movement_outcomes_commit_check",
        "settlement_movement_outcomes",
    );
    Ok(())
}

#[sqlx::test]
async fn a_movement_outcomes_step_needs_its_record(pool: PgPool) -> TestResult {
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = found_overdue(&pool, &capture)
        .await?
        .then("claim", "claimed");
    let mut tx = runtime_transaction(&pool).await?;
    claim.insert(&mut tx).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_movement_steps_record_check",
        "settlement_movement_steps",
    );
    Ok(())
}

#[sqlx::test]
async fn a_movement_outcome_keeps_to_its_rules(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = found_overdue(&pool, &capture)
        .await?
        .then("claim", "claimed");
    let too_long = "x".repeat(501);
    for (kind, reason, code, constraint) in [
        (
            "claim",
            "",
            CHECK_VIOLATION,
            "settlement_movement_outcomes_reason_check",
        ),
        (
            "claim",
            too_long.as_str(),
            CHECK_VIOLATION,
            "settlement_movement_outcomes_reason_check",
        ),
        // A record of a step that is no outcome.
        (
            "settle",
            REASON,
            CHECK_VIOLATION,
            "settlement_movement_outcomes_kind_check",
        ),
        // A record that is not its step's kind.
        (
            "write_off",
            REASON,
            FOREIGN_KEY_VIOLATION,
            "settlement_movement_outcomes_step_fkey",
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        claim.insert(&mut tx).await?;
        let posting = post(
            &mut tx,
            "EUR",
            &[(euros.receivable, -10_000), (euros.claims, 10_000)],
        )
        .await?;
        let err = insert_movement_outcome(&mut tx, &claim, (kind, reason), posting)
            .await
            .expect_err("an outcome that breaks a rule");
        assert_violation(err, code, constraint);
    }

    // A reason of 500 characters, however many bytes.
    let longest = "é".repeat(500);
    let mut tx = runtime_transaction(&pool).await?;
    claim.insert(&mut tx).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    insert_movement_outcome(&mut tx, &claim, ("claim", longest.as_str()), posting).await?;
    commit(tx).await?;

    // An outcome on a movement that is not overdue, a second outcome, an outcome after the
    // movement settled, and a movement found overdue after its outcome: each path is legal
    // until its last move.
    let fresh = settled_capture(&pool, "EUR", 10_000).await?;
    let claimed = [("overdue", "overdue"), ("claim", "claimed")];
    let written_off = [("overdue", "overdue"), ("write_off", "written_off")];
    let settled = [("settle", "settled")];
    for (path, (kind, to)) in [
        (&[][..], ("claim", "claimed")),
        (&[][..], ("write_off", "written_off")),
        (&claimed[..], ("claim", "claimed")),
        (&claimed[..], ("write_off", "written_off")),
        (&written_off[..], ("claim", "claimed")),
        (&written_off[..], ("write_off", "written_off")),
        (&settled[..], ("claim", "claimed")),
        (&settled[..], ("write_off", "written_off")),
        (&claimed[..], ("overdue", "overdue")),
        (&written_off[..], ("overdue", "overdue")),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let mut step = MovementStep::open(&fresh);
        step.insert(&mut tx).await?;
        for &(kind, to) in path {
            step = step.then(kind, to);
            step.insert(&mut tx).await?;
        }
        let err = step
            .then(kind, to)
            .insert(&mut tx)
            .await
            .expect_err("a move that the movement's chain does not take");
        assert_violation(err, CHECK_VIOLATION, "settlement_movement_steps_edge_check");
        tx.rollback().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn each_movement_outcome_is_recorded_once(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = found_overdue(&pool, &capture)
        .await?
        .then("claim", "claimed");
    let other_claim = found_overdue(&pool, &other).await?.then("claim", "claimed");
    let entries = [(euros.receivable, -10_000), (euros.claims, 10_000)];
    let mut tx = runtime_transaction(&pool).await?;
    claim.insert(&mut tx).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    insert_movement_outcome(&mut tx, &claim, ("claim", REASON), posting).await?;
    commit(tx).await?;

    let mut tx = runtime_transaction(&pool).await?;
    let again = post(&mut tx, "EUR", &entries).await?;
    let err = insert_movement_outcome(&mut tx, &claim, ("claim", REASON), again)
        .await
        .expect_err("a second record of one step");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_movement_outcomes_pkey");
    tx.rollback().await?;

    // One posting for two outcomes.
    let mut tx = runtime_transaction(&pool).await?;
    other_claim.insert(&mut tx).await?;
    let err = insert_movement_outcome(&mut tx, &other_claim, ("claim", REASON), posting)
        .await
        .expect_err("one posting for two outcomes");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_movement_outcomes_ledger_transaction_id_key",
    );
    Ok(())
}

#[sqlx::test]
async fn a_movement_outcome_never_changes(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = found_overdue(&pool, &capture)
        .await?
        .then("claim", "claimed");
    let mut tx = runtime_transaction(&pool).await?;
    claim.insert(&mut tx).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    insert_movement_outcome(&mut tx, &claim, ("claim", REASON), posting).await?;
    commit(tx).await?;

    // As the owner, the table's triggers refuse.
    for (err, trigger) in [
        (
            sqlx::query!("DELETE FROM settlement_movement_outcomes")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_outcomes_append_only",
        ),
        (
            sqlx::query!("UPDATE settlement_movement_outcomes SET reason = reason")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_outcomes_append_only",
        ),
        (
            sqlx::query!("TRUNCATE settlement_movement_outcomes")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_outcomes_no_truncate",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, "settlement_movement_outcomes");
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_movement_outcomes")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}
