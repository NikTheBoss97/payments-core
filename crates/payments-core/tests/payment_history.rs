// Author: Nicola Antonello

//! Database tests for `payment_transitions`, a payment's history (ADR-0016,
//! ADR-0017).
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
    NO_GENESIS, NewPayment, Step, VERSION_4, payment_with_first_step, register_merchant,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, runtime_transaction, sqlstate,
};
use payments_core::db::{CommitError, commit};
use payments_core::lifecycle::{Event, PaymentStatus, StepKind, Transition, next};
use sqlx::PgPool;
use uuid::Uuid;

/// The CHECK that lists the legal moves.
const EDGE_CHECK: &str = "payment_transitions_edge_check";
/// The CHECK that states each kind's amount rule.
const MONEY_CHECK: &str = "payment_transitions_money_check";
/// The foreign key that chains a step to its predecessor.
const PREV_FKEY: &str = "payment_transitions_prev_fkey";

#[sqlx::test]
async fn the_database_allows_exactly_the_moves_the_table_allows(pool: PgPool) -> sqlx::Result<()> {
    // Rust and the database each hold a copy of the moves (ADR-0016). Every
    // triple of status, kind and status is tried against the database and
    // compared with next(). A refusal is the edge CHECK's own error, which runs
    // before every other rule; any other outcome means that the move passed it.
    let genesis = Transition::genesis();
    let mut legal = vec![(genesis.from(), genesis.kind(), genesis.to())];
    for &status in PaymentStatus::ALL {
        for &event in Event::ALL {
            if let Ok(step) = next(status, event) {
                legal.push((step.from(), step.kind(), step.to()));
            }
        }
    }

    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    let (mut tried, mut allowed) = (0, 0);
    for &from in PaymentStatus::ALL {
        for &kind in StepKind::ALL {
            for &to in PaymentStatus::ALL {
                let step = Step {
                    seq: if kind == StepKind::Create { 0 } else { 1 },
                    id: Uuid::now_v7(),
                    kind: kind.as_str(),
                    from_status: from.as_str(),
                    to_status: to.as_str(),
                    amount: 1,
                    prev_refundable: 1,
                    refundable: 1,
                    ..first
                };
                sqlx::query!("SAVEPOINT triple").execute(&mut *tx).await?;
                let inserted = step.insert(&mut tx).await;
                sqlx::query!("ROLLBACK TO SAVEPOINT triple")
                    .execute(&mut *tx)
                    .await?;
                let refused = inserted.is_err_and(|err| {
                    err.as_database_error().is_some_and(|db| {
                        db.code().as_deref() == Some(CHECK_VIOLATION)
                            && db.constraint() == Some(EDGE_CHECK)
                    })
                });
                let move_ = (from, kind, to);
                assert_eq!(!refused, legal.contains(&move_), "{move_:?}");
                tried += 1;
                if !refused {
                    allowed += 1;
                }
            }
        }
    }
    assert_eq!((tried, allowed), (441, 12));
    tx.rollback().await
}

#[sqlx::test]
async fn only_the_first_step_is_a_create(pool: PgPool) -> sqlx::Result<()> {
    // The CHECKs run before any key, so these rows need no payment.
    let mut app = runtime_connection(&pool).await?;
    let first = Step::first(Uuid::now_v7());
    let create_later = Step { seq: 1, ..first };
    let err = create_later.insert_rejected(&mut app).await;
    assert_violation(err, CHECK_VIOLATION, EDGE_CHECK);

    let authorize_first = Step {
        kind: "authorize",
        to_status: "authorized",
        amount: 1,
        ..first
    };
    let err = authorize_first.insert_rejected(&mut app).await;
    assert_violation(err, CHECK_VIOLATION, EDGE_CHECK);
    Ok(())
}

#[sqlx::test]
async fn a_step_has_a_uuidv7_and_counts_from_its_predecessor(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let first = Step::first(Uuid::now_v7());
    for id in [Uuid::nil(), VERSION_4] {
        let err = Step { id, ..first }.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, "payment_transitions_id_check");
    }
    let next_step = first.then("authorize", "authorized", 1, 0);
    for (seq, prev_seq) in [(2, 0), (1, 1), (-1, -2)] {
        let err = Step {
            seq,
            prev_seq,
            ..next_step
        }
        .insert_rejected(&mut app)
        .await;
        assert_violation(err, CHECK_VIOLATION, "payment_transitions_seq_check");
    }
    Ok(())
}

#[sqlx::test]
async fn a_step_belongs_to_a_payment(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let err = Step::first(Uuid::now_v7()).insert_rejected(&mut app).await;
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "payment_transitions_payment_fkey",
    );
    Ok(())
}

#[sqlx::test]
async fn a_step_starts_from_its_predecessor(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;

    // Each of these is a legal move on its own, so only the chain refuses it.
    // A status the predecessor does not have:
    let skip_ahead = Step {
        from_status: "authorized",
        ..first.then("capture", "captured", 100, 100)
    };
    let err = skip_ahead.rejected_in(&mut tx).await?;
    assert_violation(err, FOREIGN_KEY_VIOLATION, PREV_FKEY);

    // No predecessor at all:
    let out_of_nowhere = Step {
        seq: 5,
        prev_seq: 4,
        ..first.then("authorize", "authorized", 1, 0)
    };
    let err = out_of_nowhere.rejected_in(&mut tx).await?;
    assert_violation(err, FOREIGN_KEY_VIOLATION, PREV_FKEY);

    // A refundable amount the predecessor does not have, the way to get past
    // the refund limit: this refund of 150 fits the 200 it claims is left, and
    // not the 100 that is.
    let authorized = first.then("authorize", "authorized", 100, 0);
    authorized.insert(&mut tx).await?;
    let captured = authorized.then("capture", "captured", 100, 100);
    captured.insert(&mut tx).await?;
    let inflated = Step {
        prev_refundable: 200,
        ..captured.then("refund", "captured", 150, 50)
    };
    let err = inflated.rejected_in(&mut tx).await?;
    assert_violation(err, FOREIGN_KEY_VIOLATION, PREV_FKEY);
    tx.rollback().await
}

#[sqlx::test]
async fn only_one_step_follows_a_state(pool: PgPool) -> sqlx::Result<()> {
    // What a writer that skips the payment's lock meets (ADR-0018).
    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    first
        .then("authorize", "authorized", 100, 0)
        .insert(&mut tx)
        .await?;

    let err = first
        .then("decline", "failed", 0, 0)
        .rejected_in(&mut tx)
        .await?;
    assert_violation(err, UNIQUE_VIOLATION, "payment_transitions_pkey");

    // The same move again also breaks the key on the state, and either may
    // report it, whichever index PostgreSQL checks first (ADR-0015).
    let err = first
        .then("authorize", "authorized", 100, 0)
        .rejected_in(&mut tx)
        .await?;
    let db = err.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some(UNIQUE_VIOLATION));
    assert!(
        matches!(
            db.constraint(),
            Some("payment_transitions_pkey" | "payment_transitions_state_key")
        ),
        "{db}"
    );
    tx.rollback().await
}

#[sqlx::test]
async fn a_transition_id_names_one_step_across_all_payments(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    let merchant = register_merchant(&mut tx).await?;
    let other = NewPayment::valid(merchant);
    other.insert(&mut tx).await?;
    let reused = Step {
        id: first.id,
        ..Step::first(other.id)
    };
    let err = reused.rejected_in(&mut tx).await?;
    assert_violation(err, UNIQUE_VIOLATION, "payment_transitions_id_key");
    tx.rollback().await
}

#[sqlx::test]
async fn each_kind_keeps_its_amount_rule(pool: PgPool) -> sqlx::Result<()> {
    // The CHECKs run before any key, so these rows need no payment and no
    // predecessor. Each move is legal, and only its amounts break a rule.
    let mut app = runtime_connection(&pool).await?;
    let first = Step::first(Uuid::now_v7());
    let step = |kind, from_status, to_status, amount, prev_refundable, refundable| Step {
        seq: 1,
        id: Uuid::now_v7(),
        kind,
        from_status,
        to_status,
        amount,
        prev_refundable,
        refundable,
        ..first
    };
    let broken = [
        // A first step moves no money.
        Step { amount: 1, ..first },
        Step {
            prev_refundable: 1,
            ..first
        },
        Step {
            refundable: 1,
            ..first
        },
        // An authorization has an amount and leaves nothing to refund.
        step("authorize", "created", "authorized", 0, 0, 0),
        step("authorize", "created", "authorized", 1, 0, 1),
        // A capture has an amount, which becomes the refundable amount.
        step("capture", "authorized", "captured", 0, 0, 0),
        step("capture", "authorized", "captured", 100, 1, 100),
        step("capture", "authorized", "captured", 100, 0, 99),
        // A refund has an amount, at most what is left, and takes it off.
        step("refund", "captured", "captured", 0, 100, 100),
        step("refund", "captured", "refunded", 101, 100, 0),
        step("refund", "captured", "captured", 10, 100, 100),
        // Refunded exactly when nothing is left.
        step("refund", "captured", "refunded", 10, 100, 90),
        step("refund", "captured", "captured", 100, 100, 0),
        step("refund", "settled", "settled", 100, 100, 0),
        // Every other kind moves no money and leaves the refundable amount.
        step("decline", "created", "failed", 1, 0, 0),
        step("cancel", "created", "voided", 1, 0, 0),
        step("void", "authorized", "voided", 1, 0, 0),
        step("expire", "authorized", "voided", 1, 0, 0),
        step("settle", "captured", "settled", 1, 100, 100),
        step("settle", "captured", "settled", 0, 100, 99),
    ];
    for row in broken {
        let err = row.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, MONEY_CHECK);
    }
    Ok(())
}

#[sqlx::test]
async fn a_refund_cannot_exceed_what_is_left(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(&pool).await?;
    let first = payment_with_first_step(&mut tx).await?;
    let authorized = first.then("authorize", "authorized", 100, 0);
    authorized.insert(&mut tx).await?;
    let captured = authorized.then("capture", "captured", 80, 80);
    captured.insert(&mut tx).await?;
    let refunded_30 = captured.then("refund", "captured", 30, 50);
    refunded_30.insert(&mut tx).await?;

    let too_much = refunded_30.then("refund", "captured", 51, -1);
    let err = too_much.rejected_in(&mut tx).await?;
    assert_violation(err, CHECK_VIOLATION, MONEY_CHECK);

    // A refund after settlement, for exactly what is left, ends the payment.
    let settled = refunded_30.then("settle", "settled", 0, 50);
    settled.insert(&mut tx).await?;
    let refunded = settled.then("refund", "refunded", 50, 0);
    refunded.insert(&mut tx).await?;

    // And nothing follows.
    let err = refunded
        .then("refund", "refunded", 1, 0)
        .rejected_in(&mut tx)
        .await?;
    assert_violation(err, CHECK_VIOLATION, EDGE_CHECK);
    tx.rollback().await
}

#[sqlx::test]
async fn a_payment_cannot_commit_without_its_first_step(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;

    let payment = NewPayment::valid(merchant);
    let mut tx = runtime_transaction(&pool).await?;
    payment.insert(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_GENESIS, "payments_commit_check", "payments");

    // The library reads this code as a proven rollback (ADR-0014).
    let mut tx = runtime_transaction(&pool).await?;
    payment.insert(&mut tx).await?;
    let CommitError::Rejected(source) = commit(tx).await.unwrap_err() else {
        panic!("expected a proven rollback");
    };
    assert_guard(source, NO_GENESIS, "payments_commit_check", "payments");
    let stored: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 0);

    // With its first step, it commits.
    let mut tx = runtime_transaction(&pool).await?;
    payment.insert(&mut tx).await?;
    Step::first(payment.id).insert(&mut tx).await?;
    tx.commit().await?;
    let stored: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 1);
    Ok(())
}

#[sqlx::test]
async fn checking_early_refuses_a_payment_at_once(pool: PgPool) -> sqlx::Result<()> {
    // A session can run the deferred check early, but cannot skip it.
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    NewPayment::valid(merchant).insert(&mut tx).await?;
    let err = sqlx::query!("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_guard(err, NO_GENESIS, "payments_commit_check", "payments");
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_cannot_choose_a_steps_recorded_at(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let err = sqlx::query!(
        "INSERT INTO payment_transitions \
             (payment_id, seq, prev_seq, id, kind, from_status, to_status, \
              amount, prev_refundable, refundable, recorded_at) \
         VALUES ($1, 0, 0, $2, 'create', 'created', 'created', 0, 0, 0, DEFAULT)",
        Uuid::now_v7(),
        Uuid::now_v7()
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_cannot_change_the_history(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    for err in [
        sqlx::query!("UPDATE payment_transitions SET refundable = 1")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM payment_transitions")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE payment_transitions CASCADE")
            .execute(&mut app)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    // Locking a row needs UPDATE on the table, so the role can take no row lock
    // that would block other sessions (ADR-0018).
    let err = sqlx::query!("SELECT seq FROM payment_transitions FOR KEY SHARE")
        .fetch_all(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

#[sqlx::test]
async fn the_history_is_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    payment_with_first_step(&mut tx).await?;
    tx.commit().await?;

    let table = "payment_transitions";
    let update = sqlx::query!("UPDATE payment_transitions SET refundable = 1")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "payment_transitions_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM payment_transitions")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "payment_transitions_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE payment_transitions CASCADE")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "payment_transitions_no_truncate",
        table,
    );
    Ok(())
}
