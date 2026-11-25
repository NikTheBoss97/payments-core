// Author: Nicola Antonello

//! Database tests for resolving discrepancies (ADR-0069, ADR-0072): an operator's outcome on
//! a discrepancy whose chain is its own, and on an overdue movement, a line that settles
//! late, the reversal of an outcome that its evidence follows, each with its posting, and a
//! missing batch that arrives, with the guards of I13 in PostgreSQL (ADR-0071).
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
    Accounts, Movement, MovementStep, SETTLED_POSITION, accounts, batch_with_a_fee_off_the_terms,
    find, insert_batch, insert_capture_match, insert_discrepancy, insert_discrepancy_step,
    insert_fee_difference, insert_line, insert_missing_batch, insert_overdue_movement,
    insert_payout_difference, insert_uncaused_posting, insert_unmatched_line, later_movement,
    link_posting, overdue, post, settled_capture, uncaused,
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
/// SQLSTATE of a fee's difference on a line that matches nothing (ADR-0072).
const NO_MATCH: &str = "LG013";
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
        // CASCADE, because reversals refer to the postings' links (ADR-0072).
        (
            sqlx::query!("TRUNCATE settlement_discrepancy_outcome_postings CASCADE")
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
        // CASCADE, because reversals refer to the outcomes (ADR-0072).
        (
            sqlx::query!("TRUNCATE settlement_movement_outcomes CASCADE")
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

/// A late match of the capture line `line_no` of `batch` with `capture`, the record of the
/// movement's `settle_late` step and of the discrepancy's `evidence` step, with its posting.
async fn insert_late_capture_match(
    conn: &mut PgConnection,
    (discrepancy, batch, line_no): (Uuid, Uuid, i32),
    capture: &Movement,
    (step, evidence): (Uuid, Uuid),
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_late_capture_matches \
             (discrepancy_id, batch_id, line_no, gross, psp, currency, payment_id, position, \
              psp_fact_id, request_id, settled_position, step_id, evidence_step_id, \
              ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, 'mock', $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        discrepancy,
        batch,
        line_no,
        capture.amount,
        capture.currency,
        capture.payment,
        capture.position,
        capture.fact_id,
        capture.request,
        SETTLED_POSITION,
        step,
        evidence,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A late match of the refund line `line_no` of `batch` with `refund`, as a capture's.
async fn insert_late_refund_match(
    conn: &mut PgConnection,
    (discrepancy, batch, line_no): (Uuid, Uuid, i32),
    refund: &Movement,
    (step, evidence): (Uuid, Uuid),
    posting: Uuid,
) -> sqlx::Result<()> {
    let gross = refund
        .amount
        .checked_neg()
        .expect("a refund's amount is positive");
    sqlx::query!(
        "INSERT INTO settlement_late_refund_matches \
             (discrepancy_id, batch_id, line_no, gross, psp, currency, payment_id, position, \
              psp_fact_id, request_id, step_id, evidence_step_id, ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, 'mock', $5, $6, $7, $8, $9, $10, $11, $12)",
        discrepancy,
        batch,
        line_no,
        gross,
        refund.currency,
        refund.payment,
        refund.position,
        refund.fact_id,
        refund.request,
        step,
        evidence,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Books batch `number` in EUR with one line of `(gross, fee, net)` of `kind` that settles
/// nothing of ours, its net in suspense, and records its discrepancy. Returns the batch and
/// the discrepancy.
async fn stray(
    pool: &PgPool,
    accounts: &Accounts,
    number: i64,
    (kind, (gross, fee, net)): (&str, (i64, i64, i64)),
) -> sqlx::Result<(Uuid, Uuid)> {
    let (batch, discrepancy) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(pool).await?;
    insert_batch(&mut tx, batch, "EUR", number, net).await?;
    insert_line(&mut tx, batch, 2, kind, (gross, fee, net)).await?;
    let suspense = net.checked_neg().expect("a small net");
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, net), (accounts.suspense, suspense)],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    insert_discrepancy(&mut tx, discrepancy, "unmatched_line").await?;
    find(&mut tx, discrepancy, "unmatched_line").await?;
    insert_unmatched_line(&mut tx, discrepancy, (batch, 2, net)).await?;
    tx.commit().await?;
    Ok((batch, discrepancy))
}

/// The steps of a late match, in the transaction of `conn`: `movement`'s open step, unless
/// `first` is given as its step before, and its `settle_late` step; and the `evidence` step
/// of `discrepancy`. Returns the two steps' IDs.
async fn settle_late(
    conn: &mut PgConnection,
    movement: &Movement,
    first: Option<&MovementStep>,
    discrepancy: Uuid,
) -> sqlx::Result<(Uuid, Uuid)> {
    let step = match first {
        Some(before) => before.then("settle_late", "settled"),
        None => {
            let open = MovementStep::open(movement);
            open.insert(conn).await?;
            open.then("settle_late", "settled")
        }
    };
    step.insert(conn).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        conn,
        discrepancy,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "unmatched_line",
    )
    .await?;
    Ok((step.id, evidence))
}

#[sqlx::test]
async fn a_line_settles_late_once_its_fact_arrives(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, 2_500).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("capture", (10_000, 165, 9_835))).await?;

    // The net leaves suspense, the fee is booked and the gross comes off the receivable
    // (ADR-0069).
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, 9_835),
            (euros.fees, 165),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &capture, steps, posting).await?;
    commit(tx).await?;

    // A refund's line, which owes the PSP, settles the other way.
    let (batch, line) = stray(&pool, &euros, 2, ("refund", (-2_500, 0, -2_500))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &refund, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(euros.suspense, -2_500), (euros.receivable, 2_500)],
    )
    .await?;
    insert_late_refund_match(&mut tx, (line, batch, 2), &refund, steps, posting).await?;
    commit(tx).await?;

    // An overdue movement settles late too.
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let overdue = found_overdue(&pool, &other).await?;
    let (batch, line) = stray(&pool, &euros, 3, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &other, Some(&overdue), line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, 9_835),
            (euros.fees, 165),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &other, steps, posting).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_late_match_posts_exactly_its_lines(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let pounds = accounts(&pool, "GBP").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("capture", (10_000, 165, 9_835))).await?;
    let right = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];
    let mut tx = runtime_transaction(&pool).await?;
    let earlier = post(&mut tx, "EUR", &right).await?;
    tx.commit().await?;

    // Each posting balances, so only LG010 can refuse it.
    for (currency, entries) in [
        // The fee left in suspense.
        (
            "EUR",
            vec![(euros.suspense, 10_000), (euros.receivable, -10_000)],
        ),
        // The net to the payouts.
        (
            "EUR",
            vec![
                (euros.payouts, 9_835),
                (euros.fees, 165),
                (euros.receivable, -10_000),
            ],
        ),
        // Two entries more.
        (
            "EUR",
            vec![
                (euros.suspense, 9_835),
                (euros.fees, 165),
                (euros.receivable, -10_000),
                (euros.suspense, 1),
                (euros.fees, -1),
            ],
        ),
        // The right amounts, in another currency.
        (
            "GBP",
            vec![
                (pounds.suspense, 9_835),
                (pounds.fees, 165),
                (pounds.receivable, -10_000),
            ],
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let steps = settle_late(&mut tx, &capture, None, line).await?;
        let posting = post(&mut tx, currency, &entries).await?;
        insert_late_capture_match(&mut tx, (line, batch, 2), &capture, steps, posting).await?;
        let err = commit_refused(tx).await;
        assert_guard(
            err,
            NOT_ITS_LINES,
            "settlement_late_capture_matches_commit_check",
            "settlement_late_capture_matches",
        );
    }

    // The right lines, committed before the late match.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &capture, steps, earlier).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_late_capture_matches_commit_check",
        "settlement_late_capture_matches",
    );
    Ok(())
}

#[sqlx::test]
async fn each_step_of_a_late_match_needs_it(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let (_, line) = stray(&pool, &euros, 1, ("capture", (10_000, 165, 9_835))).await?;

    // The movement's step alone.
    let mut tx = runtime_transaction(&pool).await?;
    let open = MovementStep::open(&capture);
    open.insert(&mut tx).await?;
    open.then("settle_late", "settled").insert(&mut tx).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_movement_steps_record_check",
        "settlement_movement_steps",
    );

    // The discrepancy's step alone.
    let mut tx = runtime_transaction(&pool).await?;
    insert_discrepancy_step(
        &mut tx,
        line,
        (1, 0, Uuid::now_v7()),
        ("evidence", "open", "closed"),
        "unmatched_line",
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
async fn a_late_match_agrees_with_its_line_and_its_steps(pool: PgPool) -> TestResult {
    use common::settlement::unsettled_capture;

    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("capture", (10_000, 165, 9_835))).await?;
    let (other_batch, other_line) =
        stray(&pool, &euros, 2, ("capture", (10_000, 165, 9_835))).await?;
    let entries = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];

    // The line of another discrepancy.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(&mut tx, (line, other_batch, 2), &capture, steps, posting)
        .await
        .expect_err("the line of another discrepancy");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_unmatched_line_fkey",
    );
    tx.rollback().await?;

    // The evidence of another discrepancy.
    let mut tx = runtime_transaction(&pool).await?;
    let (step, _) = settle_late(&mut tx, &capture, None, line).await?;
    let other_evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        other_line,
        (1, 0, other_evidence),
        ("evidence", "open", "closed"),
        "unmatched_line",
    )
    .await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(
        &mut tx,
        (line, batch, 2),
        &capture,
        (step, other_evidence),
        posting,
    )
    .await
    .expect_err("the evidence of another discrepancy");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_evidence_fkey",
    );
    tx.rollback().await?;

    // A settle step for a settle_late one.
    let mut tx = runtime_transaction(&pool).await?;
    let open = MovementStep::open(&capture);
    open.insert(&mut tx).await?;
    let settle = open.then("settle", "settled");
    settle.insert(&mut tx).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        line,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "unmatched_line",
    )
    .await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(
        &mut tx,
        (line, batch, 2),
        &capture,
        (settle.id, evidence),
        posting,
    )
    .await
    .expect_err("a settle step for a settle_late one");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_step_fkey",
    );
    tx.rollback().await?;

    // A fact that the line does not name: a capture of another amount.
    let smaller = settled_capture(&pool, "EUR", 9_999).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &smaller, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(&mut tx, (line, batch, 2), &smaller, steps, posting)
        .await
        .expect_err("a capture of another amount");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_line_fkey",
    );
    tx.rollback().await?;

    // The line's gross, named with a fact of another amount.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &smaller, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let as_the_line = Movement {
        amount: 10_000,
        ..smaller.clone()
    };
    let err = insert_late_capture_match(&mut tx, (line, batch, 2), &as_the_line, steps, posting)
        .await
        .expect_err("an amount that is not the fact's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_fact_fkey",
    );
    tx.rollback().await?;

    // Another request than the one that the fact decided.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let other_request = Movement {
        request: smaller.request,
        ..capture.clone()
    };
    let err = insert_late_capture_match(&mut tx, (line, batch, 2), &other_request, steps, posting)
        .await
        .expect_err("a request that the fact did not decide");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_decision_fkey",
    );
    tx.rollback().await?;

    // A settled position that holds a refund, not the payment's settlement.
    let unsettled = unsettled_capture(&pool, "EUR", 10_000).await?;
    later_movement(&pool, &unsettled, "refund", SETTLED_POSITION, 2_500).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &unsettled, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(&mut tx, (line, batch, 2), &unsettled, steps, posting)
        .await
        .expect_err("a settled position that holds no settled fact");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_settled_fkey",
    );
    tx.rollback().await?;

    // A payment in pounds, settled late in a batch in euros.
    let pounds = settled_capture(&pool, "GBP", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &pounds, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(&mut tx, (line, batch, 2), &pounds, steps, posting)
        .await
        .expect_err("a currency that is not the batch's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_currency_fkey",
    );
    tx.rollback().await?;

    // The same payment, settled late as euros.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &pounds, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let as_euros = Movement {
        currency: "EUR",
        ..pounds.clone()
    };
    let err = insert_late_capture_match(&mut tx, (line, batch, 2), &as_euros, steps, posting)
        .await
        .expect_err("a currency that is not the payment's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_capture_matches_payment_currency_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_late_refund_match_agrees_with_its_line_and_its_steps(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let refund = later_movement(&pool, &capture, "refund", 3, 2_500).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("refund", (-2_500, 0, -2_500))).await?;
    let (other_batch, other_line) =
        stray(&pool, &euros, 2, ("refund", (-2_500, 0, -2_500))).await?;
    let entries = [(euros.suspense, -2_500), (euros.receivable, 2_500)];

    // The line of another discrepancy.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &refund, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_refund_match(&mut tx, (line, other_batch, 2), &refund, steps, posting)
        .await
        .expect_err("the line of another discrepancy");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_unmatched_line_fkey",
    );
    tx.rollback().await?;

    // A refund of another amount than the line's.
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let smaller = later_movement(&pool, &other, "refund", 3, 2_400).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &smaller, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_refund_match(&mut tx, (line, batch, 2), &smaller, steps, posting)
        .await
        .expect_err("a refund of another amount");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_line_fkey",
    );
    tx.rollback().await?;

    // The line's gross, named with a fact of another amount.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &smaller, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let as_the_line = Movement {
        amount: 2_500,
        ..smaller.clone()
    };
    let err = insert_late_refund_match(&mut tx, (line, batch, 2), &as_the_line, steps, posting)
        .await
        .expect_err("an amount that is not the fact's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_fact_fkey",
    );
    tx.rollback().await?;

    // Another request than the one that the fact decided.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &refund, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let other_request = Movement {
        request: capture.request,
        ..refund.clone()
    };
    let err = insert_late_refund_match(&mut tx, (line, batch, 2), &other_request, steps, posting)
        .await
        .expect_err("a request that the fact did not decide");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_decision_fkey",
    );
    tx.rollback().await?;

    // A refund in pounds, settled late in a batch in euros.
    let pounds_capture = settled_capture(&pool, "GBP", 10_000).await?;
    let pounds = later_movement(&pool, &pounds_capture, "refund", 3, 2_500).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &pounds, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_refund_match(&mut tx, (line, batch, 2), &pounds, steps, posting)
        .await
        .expect_err("a currency that is not the batch's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_currency_fkey",
    );
    tx.rollback().await?;

    // The same refund, settled late as euros.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &pounds, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let as_euros = Movement {
        currency: "EUR",
        ..pounds.clone()
    };
    let err = insert_late_refund_match(&mut tx, (line, batch, 2), &as_euros, steps, posting)
        .await
        .expect_err("a currency that is not the payment's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_payment_currency_fkey",
    );
    tx.rollback().await?;

    // A settle step for a settle_late one.
    let mut tx = runtime_transaction(&pool).await?;
    let open = MovementStep::open(&refund);
    open.insert(&mut tx).await?;
    let settle = open.then("settle", "settled");
    settle.insert(&mut tx).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        line,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "unmatched_line",
    )
    .await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_refund_match(
        &mut tx,
        (line, batch, 2),
        &refund,
        (settle.id, evidence),
        posting,
    )
    .await
    .expect_err("a settle step for a settle_late one");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_step_fkey",
    );
    tx.rollback().await?;

    // The evidence of another discrepancy.
    let mut tx = runtime_transaction(&pool).await?;
    let (step, _) = settle_late(&mut tx, &refund, None, line).await?;
    let other_evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        other_line,
        (1, 0, other_evidence),
        ("evidence", "open", "closed"),
        "unmatched_line",
    )
    .await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_refund_match(
        &mut tx,
        (line, batch, 2),
        &refund,
        (step, other_evidence),
        posting,
    )
    .await
    .expect_err("the evidence of another discrepancy");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_late_refund_matches_evidence_fkey",
    );
    tx.rollback().await?;

    // The refund settles late, and another refund's line names its posting.
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &refund, None, line).await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    insert_late_refund_match(&mut tx, (line, batch, 2), &refund, steps, posting).await?;
    commit(tx).await?;
    let second = later_movement(&pool, &other, "refund", 4, 2_500).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &second, None, other_line).await?;
    let err = insert_late_refund_match(
        &mut tx,
        (other_line, other_batch, 2),
        &second,
        steps,
        posting,
    )
    .await
    .expect_err("one posting for two lines");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_late_refund_matches_ledger_transaction_id_key",
    );
    Ok(())
}

#[sqlx::test]
async fn a_line_settles_late_once(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("capture", (10_000, 165, 9_835))).await?;
    let entries = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];
    let mut tx = runtime_transaction(&pool).await?;
    let open = MovementStep::open(&capture);
    open.insert(&mut tx).await?;
    let settled = open.then("settle_late", "settled");
    settled.insert(&mut tx).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        line,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "unmatched_line",
    )
    .await?;
    let posting = post(&mut tx, "EUR", &entries).await?;
    insert_late_capture_match(
        &mut tx,
        (line, batch, 2),
        &capture,
        (settled.id, evidence),
        posting,
    )
    .await?;
    commit(tx).await?;

    // The movement settles once, and the discrepancy closes once and takes no outcome after.
    let mut tx = runtime_transaction(&pool).await?;
    let err = settled
        .then("settle_late", "settled")
        .insert(&mut tx)
        .await
        .expect_err("a movement settled twice");
    assert_violation(err, CHECK_VIOLATION, "settlement_movement_steps_edge_check");
    tx.rollback().await?;
    for (kind, to) in [("evidence", "closed"), ("claim", "claimed")] {
        let mut tx = runtime_transaction(&pool).await?;
        let err = insert_discrepancy_step(
            &mut tx,
            line,
            (2, 1, Uuid::now_v7()),
            (kind, "closed", to),
            "unmatched_line",
        )
        .await
        .expect_err("a move after the discrepancy closed");
        assert_violation(
            err,
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_edge_check",
        );
        tx.rollback().await?;
    }

    // A second record of the line.
    let mut tx = runtime_transaction(&pool).await?;
    let again = post(&mut tx, "EUR", &entries).await?;
    let err = insert_late_capture_match(
        &mut tx,
        (line, batch, 2),
        &capture,
        (settled.id, evidence),
        again,
    )
    .await
    .expect_err("a line settled late twice");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_late_capture_matches_pkey",
    );
    tx.rollback().await?;

    // Another payment's line, settled late by the first line's posting.
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let (other_batch, other_line) =
        stray(&pool, &euros, 2, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &other, None, other_line).await?;
    let err = insert_late_capture_match(
        &mut tx,
        (other_line, other_batch, 2),
        &other,
        steps,
        posting,
    )
    .await
    .expect_err("one posting for two lines");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_late_capture_matches_ledger_transaction_id_key",
    );
    tx.rollback().await?;

    // A second capture of the first payment, settled late by that line: the payment's
    // settled fact backs the first line already (ADR-0068).
    let second = later_movement(&pool, &capture, "capture", 3, 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &second, None, other_line).await?;
    let again = post(&mut tx, "EUR", &entries).await?;
    let err =
        insert_late_capture_match(&mut tx, (other_line, other_batch, 2), &second, steps, again)
            .await
            .expect_err("a settled fact that backs two lines");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_late_capture_matches_settled_key",
    );
    Ok(())
}

#[sqlx::test]
async fn a_fee_difference_on_a_line_settled_late_commits(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("capture", (10_000, 170, 9_830))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, 9_830),
            (euros.fees, 170),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &capture, steps, posting).await?;
    // The terms give 165 (ADR-0068).
    let fee = Uuid::now_v7();
    insert_discrepancy(&mut tx, fee, "fee_difference").await?;
    find(&mut tx, fee, "fee_difference").await?;
    insert_fee_difference(&mut tx, fee, (batch, 2), (170, 165)).await?;
    commit(tx).await?;

    // On a refund line settled late, where the terms charge nothing.
    let refund = later_movement(&pool, &capture, "refund", 3, 2_500).await?;
    let (batch, line) = stray(&pool, &euros, 2, ("refund", (-2_500, 5, -2_505))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &refund, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, -2_505),
            (euros.fees, 5),
            (euros.receivable, 2_500),
        ],
    )
    .await?;
    insert_late_refund_match(&mut tx, (line, batch, 2), &refund, steps, posting).await?;
    let fee = Uuid::now_v7();
    insert_discrepancy(&mut tx, fee, "fee_difference").await?;
    find(&mut tx, fee, "fee_difference").await?;
    insert_fee_difference(&mut tx, fee, (batch, 2), (5, 0)).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_late_match_on_another_line_leaves_a_fee_difference_unmatched(
    pool: PgPool,
) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;

    // Lines 2 and 3 of batch 1 settle nothing of ours when it is booked.
    let (batch, line) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 1, 19_665).await?;
    insert_line(&mut tx, batch, 2, "capture", (10_000, 165, 9_835)).await?;
    insert_line(&mut tx, batch, 3, "capture", (10_000, 170, 9_830)).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(euros.payouts, 19_665), (euros.suspense, -19_665)],
    )
    .await?;
    link_posting(&mut tx, batch, posting).await?;
    insert_discrepancy(&mut tx, line, "unmatched_line").await?;
    find(&mut tx, line, "unmatched_line").await?;
    insert_unmatched_line(&mut tx, line, (batch, 2, 9_835)).await?;
    commit(tx).await?;

    // Line 2 settles late, and a fee's difference on line 3, which no match names, is
    // neither booked nor checked (ADR-0068).
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, 9_835),
            (euros.fees, 165),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &capture, steps, posting).await?;
    let fee = Uuid::now_v7();
    insert_discrepancy(&mut tx, fee, "fee_difference").await?;
    find(&mut tx, fee, "fee_difference").await?;
    insert_fee_difference(&mut tx, fee, (batch, 3), (170, 165)).await?;
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
async fn a_payout_a_fee_or_a_posting_never_closes_on_evidence(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let found = found(&pool, &euros, 1).await?;
    // From its finding, or after its acceptance.
    for ((discrepancy, discrepancy_kind), accepted) in [
        ((found.payout, "payout_difference"), false),
        ((found.fee, "fee_difference"), false),
        ((found.fee, "fee_difference"), true),
        ((found.stray, "uncaused_posting"), false),
        ((found.stray, "uncaused_posting"), true),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        let (seq, prev_seq, from) = if accepted {
            insert_discrepancy_step(
                &mut tx,
                discrepancy,
                (1, 0, Uuid::now_v7()),
                ("accept", "open", "accepted"),
                discrepancy_kind,
            )
            .await?;
            (2, 1, "accepted")
        } else {
            (1, 0, "open")
        };
        let err = insert_discrepancy_step(
            &mut tx,
            discrepancy,
            (seq, prev_seq, Uuid::now_v7()),
            ("evidence", from, "closed"),
            discrepancy_kind,
        )
        .await
        .expect_err("evidence for a discrepancy that no evidence closes");
        assert_violation(
            err,
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_edge_check",
        );
        tx.rollback().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn a_late_match_never_changes(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line) = stray(&pool, &euros, 1, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &capture, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, 9_835),
            (euros.fees, 165),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &capture, steps, posting).await?;
    commit(tx).await?;
    // A refund's as well, so that each table holds a row for its row trigger to refuse.
    let refund = later_movement(&pool, &capture, "refund", 3, 2_500).await?;
    let (batch, line) = stray(&pool, &euros, 2, ("refund", (-2_500, 5, -2_505))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &refund, None, line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, -2_505),
            (euros.fees, 5),
            (euros.receivable, 2_500),
        ],
    )
    .await?;
    insert_late_refund_match(&mut tx, (line, batch, 2), &refund, steps, posting).await?;
    commit(tx).await?;

    // As the owner, each table's triggers refuse.
    for (err, trigger, table) in [
        (
            sqlx::query!("DELETE FROM settlement_late_capture_matches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_late_capture_matches_append_only",
            "settlement_late_capture_matches",
        ),
        (
            sqlx::query!("UPDATE settlement_late_capture_matches SET gross = gross")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_late_capture_matches_append_only",
            "settlement_late_capture_matches",
        ),
        (
            sqlx::query!("TRUNCATE settlement_late_capture_matches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_late_capture_matches_no_truncate",
            "settlement_late_capture_matches",
        ),
        (
            sqlx::query!("DELETE FROM settlement_late_refund_matches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_late_refund_matches_append_only",
            "settlement_late_refund_matches",
        ),
        (
            sqlx::query!("UPDATE settlement_late_refund_matches SET gross = gross")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_late_refund_matches_append_only",
            "settlement_late_refund_matches",
        ),
        (
            sqlx::query!("TRUNCATE settlement_late_refund_matches")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_late_refund_matches_no_truncate",
            "settlement_late_refund_matches",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, table);
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_late_capture_matches")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

/// The reversal of the movement outcome `outcome`, carried by `movement`'s step `step` of
/// `step_kind`, with its posting.
async fn insert_movement_reversal(
    conn: &mut PgConnection,
    (step, step_kind): (Uuid, &str),
    movement: &Movement,
    outcome: Uuid,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_movement_reversals \
             (step_id, payment_id, position, step_kind, outcome_step_id, ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        step,
        movement.payment,
        movement.position,
        step_kind,
        outcome,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The reversal of the outcome `outcome` of `discrepancy`, carried by its evidence step
/// `step`, with its posting.
async fn insert_discrepancy_reversal(
    conn: &mut PgConnection,
    (step, discrepancy): (Uuid, Uuid),
    outcome: Uuid,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_discrepancy_reversals \
             (step_id, discrepancy_id, outcome_step_id, ledger_transaction_id) \
         VALUES ($1, $2, $3, $4)",
        step,
        discrepancy,
        outcome,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Finds `movement` overdue and records an operator's outcome of `kind` on it, with
/// `entries` as its posting in EUR, and commits. Returns the outcome's step.
async fn decided_overdue(
    pool: &PgPool,
    movement: &Movement,
    (kind, to): (&'static str, &'static str),
    entries: &[(Uuid, i64)],
) -> sqlx::Result<MovementStep> {
    let outcome = found_overdue(pool, movement).await?.then(kind, to);
    let mut tx = runtime_transaction(pool).await?;
    outcome.insert(&mut tx).await?;
    let posting = post(&mut tx, "EUR", entries).await?;
    insert_movement_outcome(&mut tx, &outcome, (kind, REASON), posting).await?;
    tx.commit().await?;
    Ok(outcome)
}

/// Books batch `number` in EUR whose one line, of 10,000 with a fee of 165, settles
/// `capture` through `step`, in the transaction of `conn`, with its posting.
async fn book_on_time(
    conn: &mut PgConnection,
    accounts: &Accounts,
    number: i64,
    (capture, step): (&Movement, &MovementStep),
) -> sqlx::Result<()> {
    let batch = Uuid::now_v7();
    insert_batch(conn, batch, "EUR", number, 9_835).await?;
    insert_line(conn, batch, 2, "capture", (10_000, 165, 9_835)).await?;
    step.insert(conn).await?;
    insert_capture_match(conn, batch, 2, capture, SETTLED_POSITION, step.id).await?;
    let posting = post(
        conn,
        "EUR",
        &[
            (accounts.payouts, 9_835),
            (accounts.fees, 165),
            (accounts.receivable, -10_000),
        ],
    )
    .await?;
    link_posting(conn, batch, posting).await?;
    Ok(())
}

/// Books batch `number` in EUR with a capture line of 10,000 with a fee of 165 that settles
/// nothing of ours, records an operator's outcome of `kind` on it, which moves its net to
/// `account`, with its posting, and commits. Returns the batch, the line's discrepancy and
/// the outcome's step.
async fn decided_line(
    pool: &PgPool,
    accounts: &Accounts,
    number: i64,
    (kind, to, account): (&'static str, &'static str, Uuid),
) -> sqlx::Result<(Uuid, Uuid, Uuid)> {
    let (batch, line) = stray(pool, accounts, number, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(pool).await?;
    let step = outcome(&mut tx, (line, "unmatched_line"), (kind, to), REASON).await?;
    posts(
        &mut tx,
        (step, kind),
        "EUR",
        &[(accounts.suspense, 9_835), (account, -9_835)],
    )
    .await?;
    tx.commit().await?;
    Ok((batch, line, step))
}

/// Closes `line` of `batch`, which its outcome left `from`, by its late match with `capture`
/// and `entries` as the match's posting, in the transaction of `conn`: the movement's `open`
/// and `settle_late` steps, the line's `evidence` step and the late match. Returns the
/// evidence step.
async fn close_late(
    conn: &mut PgConnection,
    (line, batch): (Uuid, Uuid),
    (capture, from): (&Movement, &str),
    entries: &[(Uuid, i64)],
) -> sqlx::Result<Uuid> {
    let open = MovementStep::open(capture);
    open.insert(conn).await?;
    let late = open.then("settle_late", "settled");
    late.insert(conn).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        conn,
        line,
        (2, 1, evidence),
        ("evidence", from, "closed"),
        "unmatched_line",
    )
    .await?;
    let posting = post(conn, "EUR", entries).await?;
    insert_late_capture_match(
        conn,
        (line, batch, 2),
        capture,
        (late.id, evidence),
        posting,
    )
    .await?;
    Ok(evidence)
}

#[sqlx::test]
async fn an_outcome_that_evidence_follows_is_reversed(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;

    // A claimed movement settles on time in a later batch, and the claim is reversed.
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = decided_overdue(
        &pool,
        &capture,
        ("claim", "claimed"),
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    let settle = claim.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, 10_000), (euros.claims, -10_000)],
    )
    .await?;
    insert_movement_reversal(&mut tx, (settle.id, "settle"), &capture, claim.id, reversal).await?;
    commit(tx).await?;

    // A movement written off settles late, and the write-off is reversed.
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let write_off = decided_overdue(
        &pool,
        &other,
        ("write_off", "written_off"),
        &[(euros.receivable, -10_000), (euros.writeoffs, 10_000)],
    )
    .await?;
    let (batch, line) = stray(&pool, &euros, 2, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let (step, evidence) = settle_late(&mut tx, &other, Some(&write_off), line).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[
            (euros.suspense, 9_835),
            (euros.fees, 165),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &other, (step, evidence), posting).await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, 10_000), (euros.writeoffs, -10_000)],
    )
    .await?;
    insert_movement_reversal(
        &mut tx,
        (step, "settle_late"),
        &other,
        write_off.id,
        reversal,
    )
    .await?;
    commit(tx).await?;

    // A claimed line settles late, and the line's claim is reversed.
    let late_entries = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];
    let third = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line, claimed) =
        decided_line(&pool, &euros, 3, ("claim", "claimed", euros.claims)).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = close_late(&mut tx, (line, batch), (&third, "claimed"), &late_entries).await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.suspense, -9_835), (euros.claims, 9_835)],
    )
    .await?;
    insert_discrepancy_reversal(&mut tx, (evidence, line), claimed, reversal).await?;
    commit(tx).await?;

    // A line written off settles late, and the line's write-off is reversed.
    let fourth = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line, written_off) = decided_line(
        &pool,
        &euros,
        4,
        ("write_off", "written_off", euros.writeoffs),
    )
    .await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = close_late(
        &mut tx,
        (line, batch),
        (&fourth, "written_off"),
        &late_entries,
    )
    .await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.suspense, -9_835), (euros.writeoffs, 9_835)],
    )
    .await?;
    insert_discrepancy_reversal(&mut tx, (evidence, line), written_off, reversal).await?;
    commit(tx).await?;

    // A claimed movement settles late, and the claim is reversed.
    let fifth = settled_capture(&pool, "EUR", 10_000).await?;
    let fifth_claim = decided_overdue(
        &pool,
        &fifth,
        ("claim", "claimed"),
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    let (batch, line) = stray(&pool, &euros, 5, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let (step, evidence) = settle_late(&mut tx, &fifth, Some(&fifth_claim), line).await?;
    let posting = post(&mut tx, "EUR", &late_entries).await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &fifth, (step, evidence), posting).await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, 10_000), (euros.claims, -10_000)],
    )
    .await?;
    insert_movement_reversal(
        &mut tx,
        (step, "settle_late"),
        &fifth,
        fifth_claim.id,
        reversal,
    )
    .await?;
    commit(tx).await?;

    // A movement written off settles on time in a later batch, and the write-off is reversed.
    let sixth = settled_capture(&pool, "EUR", 10_000).await?;
    let sixth_write_off = decided_overdue(
        &pool,
        &sixth,
        ("write_off", "written_off"),
        &[(euros.receivable, -10_000), (euros.writeoffs, 10_000)],
    )
    .await?;
    let sixth_settle = sixth_write_off.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 6, (&sixth, &sixth_settle)).await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, 10_000), (euros.writeoffs, -10_000)],
    )
    .await?;
    insert_movement_reversal(
        &mut tx,
        (sixth_settle.id, "settle"),
        &sixth,
        sixth_write_off.id,
        reversal,
    )
    .await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_reversal_is_exactly_its_outcomes_mirror(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let pounds = accounts(&pool, "GBP").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = decided_overdue(
        &pool,
        &capture,
        ("claim", "claimed"),
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    let settle = claim.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    let earlier = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, 10_000), (euros.claims, -10_000)],
    )
    .await?;
    tx.commit().await?;

    // Each posting balances, so only LG010 can refuse it.
    for (currency, entries) in [
        // The outcome's lines again, not mirrored.
        (
            "EUR",
            vec![(euros.receivable, -10_000), (euros.claims, 10_000)],
        ),
        // One too little.
        (
            "EUR",
            vec![(euros.receivable, 9_999), (euros.claims, -9_999)],
        ),
        // Two entries more.
        (
            "EUR",
            vec![
                (euros.receivable, 10_000),
                (euros.claims, -10_000),
                (euros.receivable, 1),
                (euros.claims, -1),
            ],
        ),
        // The right amounts, in another currency.
        (
            "GBP",
            vec![(pounds.receivable, 10_000), (pounds.claims, -10_000)],
        ),
    ] {
        let mut tx = runtime_transaction(&pool).await?;
        book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
        let reversal = post(&mut tx, currency, &entries).await?;
        insert_movement_reversal(&mut tx, (settle.id, "settle"), &capture, claim.id, reversal)
            .await?;
        let err = commit_refused(tx).await;
        assert_guard(
            err,
            NOT_ITS_LINES,
            "settlement_movement_reversals_commit_check",
            "settlement_movement_reversals",
        );
    }

    // The right mirror, committed before.
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    insert_movement_reversal(&mut tx, (settle.id, "settle"), &capture, claim.id, earlier).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NOT_ITS_LINES,
        "settlement_movement_reversals_commit_check",
        "settlement_movement_reversals",
    );
    Ok(())
}

#[sqlx::test]
async fn a_step_after_a_posted_outcome_needs_its_reversal(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;

    // A claimed movement settled without its claim reversed.
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = decided_overdue(
        &pool,
        &capture,
        ("claim", "claimed"),
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(
        &mut tx,
        &euros,
        1,
        (&capture, &claim.then("settle", "settled")),
    )
    .await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_movement_steps_record_check",
        "settlement_movement_steps",
    );

    // A claimed line closed by its late match without its claim reversed.
    let late_entries = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line, _) =
        decided_line(&pool, &euros, 2, ("claim", "claimed", euros.claims)).await?;
    let mut tx = runtime_transaction(&pool).await?;
    close_late(&mut tx, (line, batch), (&other, "claimed"), &late_entries).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_discrepancy_steps_record_check",
        "settlement_discrepancy_steps",
    );

    // A movement written off and settled late without its write-off reversed.
    let third = settled_capture(&pool, "EUR", 10_000).await?;
    let write_off = decided_overdue(
        &pool,
        &third,
        ("write_off", "written_off"),
        &[(euros.receivable, -10_000), (euros.writeoffs, 10_000)],
    )
    .await?;
    let (batch, line) = stray(&pool, &euros, 3, ("capture", (10_000, 165, 9_835))).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let steps = settle_late(&mut tx, &third, Some(&write_off), line).await?;
    let posting = post(&mut tx, "EUR", &late_entries).await?;
    insert_late_capture_match(&mut tx, (line, batch, 2), &third, steps, posting).await?;
    let err = commit_refused(tx).await;
    assert_guard(
        err,
        NO_RECORD,
        "settlement_movement_steps_record_check",
        "settlement_movement_steps",
    );

    // After a claim that moved nothing, so posted nothing, a line closes with nothing to
    // reverse.
    let small = settled_capture(&pool, "EUR", 25).await?;
    let (batch, line) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 4, 0).await?;
    insert_line(&mut tx, batch, 2, "capture", (25, 25, 0)).await?;
    insert_discrepancy(&mut tx, line, "unmatched_line").await?;
    find(&mut tx, line, "unmatched_line").await?;
    insert_unmatched_line(&mut tx, line, (batch, 2, 0)).await?;
    outcome(
        &mut tx,
        (line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    close_late(
        &mut tx,
        (line, batch),
        (&small, "claimed"),
        &[(euros.fees, 25), (euros.receivable, -25)],
    )
    .await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_reversal_names_an_outcome_of_its_own_that_posted(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let entries = [(euros.receivable, -10_000), (euros.claims, 10_000)];
    let claim = decided_overdue(&pool, &capture, ("claim", "claimed"), &entries).await?;
    let other_claim = decided_overdue(&pool, &other, ("claim", "claimed"), &entries).await?;
    let settle = claim.then("settle", "settled");
    let mirror = [(euros.receivable, 10_000), (euros.claims, -10_000)];

    // Another movement's outcome.
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(&mut tx, "EUR", &mirror).await?;
    let err = insert_movement_reversal(
        &mut tx,
        (settle.id, "settle"),
        &capture,
        other_claim.id,
        reversal,
    )
    .await
    .expect_err("another movement's outcome");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_movement_reversals_outcome_fkey",
    );
    tx.rollback().await?;

    // Carried by a step that settles nothing.
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(&mut tx, "EUR", &mirror).await?;
    let err = insert_movement_reversal(&mut tx, (settle.id, "claim"), &capture, claim.id, reversal)
        .await
        .expect_err("a step that settles nothing");
    assert_violation(
        err,
        CHECK_VIOLATION,
        "settlement_movement_reversals_step_kind_check",
    );
    tx.rollback().await?;

    // Named with a kind that is not its step's.
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(&mut tx, "EUR", &mirror).await?;
    let err = insert_movement_reversal(
        &mut tx,
        (settle.id, "settle_late"),
        &capture,
        claim.id,
        reversal,
    )
    .await
    .expect_err("a kind that is not its step's");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_movement_reversals_step_fkey",
    );
    tx.rollback().await?;

    // Carried by another movement's step.
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(&mut tx, "EUR", &mirror).await?;
    let err = insert_movement_reversal(
        &mut tx,
        (settle.id, "settle"),
        &other,
        other_claim.id,
        reversal,
    )
    .await
    .expect_err("another movement's step");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_movement_reversals_step_fkey",
    );
    tx.rollback().await?;

    // A line's claim that moved nothing, so posted nothing to reverse.
    let (batch, line) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, batch, "EUR", 2, 0).await?;
    insert_line(&mut tx, batch, 2, "capture", (25, 25, 0)).await?;
    insert_discrepancy(&mut tx, line, "unmatched_line").await?;
    find(&mut tx, line, "unmatched_line").await?;
    insert_unmatched_line(&mut tx, line, (batch, 2, 0)).await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let claimed = outcome(
        &mut tx,
        (line, "unmatched_line"),
        ("claim", "claimed"),
        REASON,
    )
    .await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        line,
        (2, 1, evidence),
        ("evidence", "claimed", "closed"),
        "unmatched_line",
    )
    .await?;
    let reversal = post(&mut tx, "EUR", &[(euros.suspense, 1), (euros.claims, -1)]).await?;
    let err = insert_discrepancy_reversal(&mut tx, (evidence, line), claimed, reversal)
        .await
        .expect_err("a claim that posted nothing");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_discrepancy_reversals_posting_fkey",
    );
    tx.rollback().await?;

    // Two lines claimed, each with its posting, and the first closed by its late match.
    let late_entries = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];
    let line_mirror = [(euros.suspense, -9_835), (euros.claims, 9_835)];
    let (first_batch, first, first_claim) =
        decided_line(&pool, &euros, 3, ("claim", "claimed", euros.claims)).await?;
    let (_, _, second_claim) =
        decided_line(&pool, &euros, 4, ("claim", "claimed", euros.claims)).await?;
    let third = settled_capture(&pool, "EUR", 10_000).await?;

    // Carried by the claim's step instead of the evidence.
    let mut tx = runtime_transaction(&pool).await?;
    close_late(
        &mut tx,
        (first, first_batch),
        (&third, "claimed"),
        &late_entries,
    )
    .await?;
    let reversal = post(&mut tx, "EUR", &line_mirror).await?;
    let err = insert_discrepancy_reversal(&mut tx, (first_claim, first), first_claim, reversal)
        .await
        .expect_err("a step that is not evidence");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_discrepancy_reversals_step_fkey",
    );
    tx.rollback().await?;

    // Another line's claim.
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = close_late(
        &mut tx,
        (first, first_batch),
        (&third, "claimed"),
        &late_entries,
    )
    .await?;
    let reversal = post(&mut tx, "EUR", &line_mirror).await?;
    let err = insert_discrepancy_reversal(&mut tx, (evidence, first), second_claim, reversal)
        .await
        .expect_err("another discrepancy's outcome");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_discrepancy_reversals_outcome_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn each_reversal_has_a_posting_of_its_own(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;

    // Two claimed movements of one amount settle in turn, and the second's reversal names the
    // first's posting, which mirrors its claim as well.
    let entries = [(euros.receivable, -10_000), (euros.claims, 10_000)];
    let mirror = [(euros.receivable, 10_000), (euros.claims, -10_000)];
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = decided_overdue(&pool, &capture, ("claim", "claimed"), &entries).await?;
    let other_claim = decided_overdue(&pool, &other, ("claim", "claimed"), &entries).await?;
    let settle = claim.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(&mut tx, "EUR", &mirror).await?;
    insert_movement_reversal(&mut tx, (settle.id, "settle"), &capture, claim.id, reversal).await?;
    commit(tx).await?;
    let other_settle = other_claim.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 2, (&other, &other_settle)).await?;
    let err = insert_movement_reversal(
        &mut tx,
        (other_settle.id, "settle"),
        &other,
        other_claim.id,
        reversal,
    )
    .await
    .expect_err("one posting for two reversals");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_movement_reversals_ledger_transaction_id_key",
    );
    tx.rollback().await?;

    // Two claimed lines of one net close in turn, likewise.
    let late_entries = [
        (euros.suspense, 9_835),
        (euros.fees, 165),
        (euros.receivable, -10_000),
    ];
    let line_mirror = [(euros.suspense, -9_835), (euros.claims, 9_835)];
    let (first_batch, first, first_claim) =
        decided_line(&pool, &euros, 3, ("claim", "claimed", euros.claims)).await?;
    let (second_batch, second, second_claim) =
        decided_line(&pool, &euros, 4, ("claim", "claimed", euros.claims)).await?;
    let third = settled_capture(&pool, "EUR", 10_000).await?;
    let fourth = settled_capture(&pool, "EUR", 10_000).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = close_late(
        &mut tx,
        (first, first_batch),
        (&third, "claimed"),
        &late_entries,
    )
    .await?;
    let reversal = post(&mut tx, "EUR", &line_mirror).await?;
    insert_discrepancy_reversal(&mut tx, (evidence, first), first_claim, reversal).await?;
    commit(tx).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = close_late(
        &mut tx,
        (second, second_batch),
        (&fourth, "claimed"),
        &late_entries,
    )
    .await?;
    let err = insert_discrepancy_reversal(&mut tx, (evidence, second), second_claim, reversal)
        .await
        .expect_err("one posting for two reversals");
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "settlement_discrepancy_reversals_ledger_transaction_id_key",
    );
    Ok(())
}

#[sqlx::test]
async fn a_reversal_never_changes(pool: PgPool) -> TestResult {
    let euros = accounts(&pool, "EUR").await?;
    let capture = settled_capture(&pool, "EUR", 10_000).await?;
    let claim = decided_overdue(
        &pool,
        &capture,
        ("claim", "claimed"),
        &[(euros.receivable, -10_000), (euros.claims, 10_000)],
    )
    .await?;
    let settle = claim.then("settle", "settled");
    let mut tx = runtime_transaction(&pool).await?;
    book_on_time(&mut tx, &euros, 1, (&capture, &settle)).await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.receivable, 10_000), (euros.claims, -10_000)],
    )
    .await?;
    insert_movement_reversal(&mut tx, (settle.id, "settle"), &capture, claim.id, reversal).await?;
    commit(tx).await?;
    // A line's claim reversed as well, so that each table holds a row for its row trigger to
    // refuse.
    let other = settled_capture(&pool, "EUR", 10_000).await?;
    let (batch, line, claimed) =
        decided_line(&pool, &euros, 2, ("claim", "claimed", euros.claims)).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = close_late(
        &mut tx,
        (line, batch),
        (&other, "claimed"),
        &[
            (euros.suspense, 9_835),
            (euros.fees, 165),
            (euros.receivable, -10_000),
        ],
    )
    .await?;
    let reversal = post(
        &mut tx,
        "EUR",
        &[(euros.suspense, -9_835), (euros.claims, 9_835)],
    )
    .await?;
    insert_discrepancy_reversal(&mut tx, (evidence, line), claimed, reversal).await?;
    commit(tx).await?;

    // As the owner, each table's triggers refuse.
    for (err, trigger, table) in [
        (
            sqlx::query!("DELETE FROM settlement_movement_reversals")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_reversals_append_only",
            "settlement_movement_reversals",
        ),
        (
            sqlx::query!("UPDATE settlement_movement_reversals SET step_kind = step_kind")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_reversals_append_only",
            "settlement_movement_reversals",
        ),
        (
            sqlx::query!("TRUNCATE settlement_movement_reversals")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_movement_reversals_no_truncate",
            "settlement_movement_reversals",
        ),
        (
            sqlx::query!("DELETE FROM settlement_discrepancy_reversals")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_reversals_append_only",
            "settlement_discrepancy_reversals",
        ),
        (
            sqlx::query!("UPDATE settlement_discrepancy_reversals SET step_id = step_id")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_reversals_append_only",
            "settlement_discrepancy_reversals",
        ),
        (
            sqlx::query!("TRUNCATE settlement_discrepancy_reversals")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_discrepancy_reversals_no_truncate",
            "settlement_discrepancy_reversals",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, table);
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_movement_reversals")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

/// The arrival of batch `number` of the mock in EUR, the record of `discrepancy`'s
/// evidence step `step`.
async fn insert_arrival(
    conn: &mut PgConnection,
    (step, discrepancy): (Uuid, Uuid),
    number: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_batch_arrivals (step_id, discrepancy_id, psp, currency, number) \
         VALUES ($1, $2, 'mock', 'EUR', $3)",
        step,
        discrepancy,
        number
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Records batch `number` of the mock in EUR as missing, found and committed. Returns its
/// discrepancy.
async fn missing(pool: &PgPool, number: i64) -> sqlx::Result<Uuid> {
    let discrepancy = Uuid::now_v7();
    let mut tx = runtime_transaction(pool).await?;
    insert_discrepancy(&mut tx, discrepancy, "missing_batch").await?;
    find(&mut tx, discrepancy, "missing_batch").await?;
    insert_missing_batch(&mut tx, discrepancy, ("mock", "EUR", number), "not_listed").await?;
    tx.commit().await?;
    Ok(discrepancy)
}

/// Books batch `number` of the mock in EUR, with no line and no payout, so no posting, and
/// commits.
async fn book_empty(pool: &PgPool, number: i64) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(pool).await?;
    insert_batch(&mut tx, Uuid::now_v7(), "EUR", number, 0).await?;
    tx.commit().await?;
    Ok(())
}

#[sqlx::test]
async fn a_missing_batch_closes_when_it_arrives(pool: PgPool) -> TestResult {
    // In the transaction that books it.
    let first = missing(&pool, 5).await?;
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, Uuid::now_v7(), "EUR", 5, 0).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        first,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "missing_batch",
    )
    .await?;
    insert_arrival(&mut tx, (evidence, first), 5).await?;
    commit(tx).await?;

    // At a later pass, after an operator accepted it: the acceptance posted nothing, so
    // nothing is reversed.
    let second = missing(&pool, 6).await?;
    let mut tx = runtime_transaction(&pool).await?;
    outcome(
        &mut tx,
        (second, "missing_batch"),
        ("accept", "accepted"),
        REASON,
    )
    .await?;
    commit(tx).await?;
    book_empty(&pool, 6).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        second,
        (2, 1, evidence),
        ("evidence", "accepted", "closed"),
        "missing_batch",
    )
    .await?;
    insert_arrival(&mut tx, (evidence, second), 6).await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn an_arrival_names_the_batch_that_was_missing(pool: PgPool) -> TestResult {
    let discrepancy = missing(&pool, 5).await?;
    book_empty(&pool, 6).await?;

    // A batch that was not the missing one.
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        discrepancy,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "missing_batch",
    )
    .await?;
    let err = insert_arrival(&mut tx, (evidence, discrepancy), 6)
        .await
        .expect_err("another batch");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_batch_arrivals_missing_fkey",
    );
    tx.rollback().await?;

    // The missing batch, not booked yet.
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        discrepancy,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "missing_batch",
    )
    .await?;
    let err = insert_arrival(&mut tx, (evidence, discrepancy), 5)
        .await
        .expect_err("a batch that has not arrived");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_batch_arrivals_batch_fkey",
    );
    tx.rollback().await?;

    // The evidence step of another missing batch.
    let other = missing(&pool, 7).await?;
    book_empty(&pool, 5).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let evidence = Uuid::now_v7();
    insert_discrepancy_step(
        &mut tx,
        other,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "missing_batch",
    )
    .await?;
    let err = insert_arrival(&mut tx, (evidence, discrepancy), 5)
        .await
        .expect_err("another discrepancy's evidence");
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "settlement_batch_arrivals_step_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_missing_batchs_evidence_needs_its_arrival(pool: PgPool) -> TestResult {
    let discrepancy = missing(&pool, 5).await?;
    let mut tx = runtime_transaction(&pool).await?;
    insert_batch(&mut tx, Uuid::now_v7(), "EUR", 5, 0).await?;
    insert_discrepancy_step(
        &mut tx,
        discrepancy,
        (1, 0, Uuid::now_v7()),
        ("evidence", "open", "closed"),
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
async fn a_missing_batch_closes_once(pool: PgPool) -> TestResult {
    let discrepancy = missing(&pool, 5).await?;
    book_empty(&pool, 5).await?;
    let evidence = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_discrepancy_step(
        &mut tx,
        discrepancy,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "missing_batch",
    )
    .await?;
    insert_arrival(&mut tx, (evidence, discrepancy), 5).await?;
    commit(tx).await?;

    // The discrepancy closes once, and takes no outcome after.
    for (kind, to) in [("evidence", "closed"), ("accept", "accepted")] {
        let mut tx = runtime_transaction(&pool).await?;
        let err = insert_discrepancy_step(
            &mut tx,
            discrepancy,
            (2, 1, Uuid::now_v7()),
            (kind, "closed", to),
            "missing_batch",
        )
        .await
        .expect_err("a move after the discrepancy closed");
        assert_violation(
            err,
            CHECK_VIOLATION,
            "settlement_discrepancy_steps_edge_check",
        );
        tx.rollback().await?;
    }
    let mut tx = runtime_transaction(&pool).await?;
    let err = insert_arrival(&mut tx, (evidence, discrepancy), 5)
        .await
        .expect_err("a second record of one step");
    assert_violation(err, UNIQUE_VIOLATION, "settlement_batch_arrivals_pkey");
    Ok(())
}

#[sqlx::test]
async fn an_arrival_never_changes(pool: PgPool) -> TestResult {
    let discrepancy = missing(&pool, 5).await?;
    book_empty(&pool, 5).await?;
    let evidence = Uuid::now_v7();
    let mut tx = runtime_transaction(&pool).await?;
    insert_discrepancy_step(
        &mut tx,
        discrepancy,
        (1, 0, evidence),
        ("evidence", "open", "closed"),
        "missing_batch",
    )
    .await?;
    insert_arrival(&mut tx, (evidence, discrepancy), 5).await?;
    commit(tx).await?;

    // As the owner, the table's triggers refuse.
    for (err, trigger) in [
        (
            sqlx::query!("DELETE FROM settlement_batch_arrivals")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batch_arrivals_append_only",
        ),
        (
            sqlx::query!("UPDATE settlement_batch_arrivals SET number = number")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batch_arrivals_append_only",
        ),
        (
            sqlx::query!("TRUNCATE settlement_batch_arrivals")
                .execute(&pool)
                .await
                .expect_err("append-only"),
            "settlement_batch_arrivals_no_truncate",
        ),
    ] {
        assert_guard(err, APPEND_ONLY, trigger, "settlement_batch_arrivals");
    }

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM settlement_batch_arrivals")
        .execute(&mut *tx)
        .await
        .expect_err("no DELETE privilege");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}
