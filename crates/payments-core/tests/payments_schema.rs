// Author: Nicola Antonello

//! Database tests for the payment tables (ADR-0017, ADR-0020).
//!
//! Rows are written with raw SQL, bypassing the domain types, the way a buggy
//! code path or a manual session could write them. They run as the runtime
//! role, `payments_app`, unless a test says otherwise. Each rejected write
//! breaks a single rule. The test asserts the SQLSTATE and, where PostgreSQL
//! reports one, the constraint or trigger that rejects the write; privilege
//! errors carry none. Writes that must succeed run in a transaction that rolls
//! back, so that the deferred checks do not fire unless a test wants them to.

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, runtime_transaction, sqlstate,
};
use payments_core::accounts::Psp;
use payments_core::db::{CommitError, commit};
use payments_core::lifecycle::{Event, PaymentStatus, StepKind, Transition, next};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::{Uuid, uuid};

/// The largest magnitude of a single amount (ADR-0003).
const CAP: i64 = 1_000_000_000_000_000;
/// A valid UUID that is not version 7.
const VERSION_4: Uuid = uuid!("019b76da-a800-4000-8000-000000000001");

/// Registers a new merchant and returns its ID.
async fn register_merchant(conn: &mut PgConnection) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!("INSERT INTO merchants (id) VALUES ($1)", id)
        .execute(conn)
        .await?;
    Ok(id)
}

/// A payment as the runtime role writes it: every column except
/// `recorded_at`, which only the database sets (ADR-0012).
#[derive(Debug, Clone, Copy)]
struct NewPayment<'a> {
    id: Uuid,
    merchant_id: Uuid,
    psp: &'a str,
    currency: &'a str,
    amount: i64,
    fee_bps: i32,
}

impl<'a> NewPayment<'a> {
    /// A valid payment with a fresh ID. Tests change one field to break one
    /// rule.
    fn valid(merchant_id: Uuid) -> Self {
        Self {
            id: Uuid::now_v7(),
            merchant_id,
            psp: Psp::Mock.as_str(),
            currency: "EUR",
            amount: 10_000,
            fee_bps: 250,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payments (id, merchant_id, psp, currency, amount, fee_bps) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            self.id,
            self.merchant_id,
            self.psp,
            self.currency,
            self.amount,
            self.fee_bps
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Inserts the payment and returns the error, failing the test if the
    /// insert succeeds.
    async fn insert_rejected(&self, conn: &mut PgConnection) -> sqlx::Error {
        self.insert(conn)
            .await
            .err()
            .unwrap_or_else(|| panic!("{self:?} was accepted"))
    }
}

#[sqlx::test]
async fn a_merchant_has_a_uuidv7_and_registers_once(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    for id in [Uuid::nil(), Uuid::max(), VERSION_4] {
        let err = sqlx::query!("INSERT INTO merchants (id) VALUES ($1)", id)
            .execute(&mut app)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "merchants_id_check");
    }

    let id = register_merchant(&mut app).await?;
    let err = sqlx::query!("INSERT INTO merchants (id) VALUES ($1)", id)
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "merchants_pkey");

    // How the library registers one: registering again changes nothing.
    let done = sqlx::query!(
        "INSERT INTO merchants (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
        id
    )
    .execute(&mut app)
    .await?;
    assert_eq!(done.rows_affected(), 0);
    Ok(())
}

#[sqlx::test]
async fn a_payment_needs_a_registered_merchant(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let err = NewPayment::valid(Uuid::now_v7())
        .insert_rejected(&mut app)
        .await;
    assert_violation(err, FOREIGN_KEY_VIOLATION, "payments_merchant_fkey");

    let merchant = register_merchant(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    NewPayment::valid(merchant).insert(&mut tx).await?;
    tx.rollback().await
}

#[sqlx::test]
async fn each_payment_column_keeps_its_rule(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let valid = NewPayment::valid(merchant);

    let mut broken = vec![
        (
            NewPayment {
                id: Uuid::nil(),
                ..valid
            },
            "payments_id_check",
        ),
        (
            NewPayment {
                id: VERSION_4,
                ..valid
            },
            "payments_id_check",
        ),
        (
            NewPayment {
                currency: "eur",
                ..valid
            },
            "currency_code_check",
        ),
    ];
    for psp in ["", "Mock", "stripe"] {
        broken.push((NewPayment { psp, ..valid }, "payments_psp_check"));
    }
    for amount in [i64::MIN, -1, 0, CAP + 1, i64::MAX] {
        broken.push((NewPayment { amount, ..valid }, "payments_amount_check"));
    }
    for fee_bps in [-1, 10_001] {
        broken.push((NewPayment { fee_bps, ..valid }, "payments_fee_bps_check"));
    }
    for (payment, constraint) in broken {
        let err = payment.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, constraint);
    }

    // Both ends of each range.
    let mut tx = runtime_transaction(&pool).await?;
    for (amount, fee_bps) in [(1, 0), (CAP, 10_000)] {
        NewPayment {
            id: Uuid::now_v7(),
            amount,
            fee_bps,
            ..valid
        }
        .insert(&mut tx)
        .await?;
    }
    tx.rollback().await
}

#[sqlx::test]
async fn a_payment_id_names_one_payment(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    let err = payment.insert_rejected(&mut tx).await;
    // Either unique key that holds the ID may report it, whichever index
    // PostgreSQL checks first (ADR-0015).
    let db = err.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some(UNIQUE_VIOLATION));
    assert!(
        matches!(
            db.constraint(),
            Some("payments_pkey" | "payments_id_amount_key")
        ),
        "{db}"
    );
    Ok(())
}

#[sqlx::test]
async fn the_psp_label_round_trips_through_the_database(pool: PgPool) -> sqlx::Result<()> {
    // SQL binds Psp::as_str() and Rust parses what comes back (ADR-0009). This
    // stands in for the check an enum type would give.
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    let stored = sqlx::query_scalar!("SELECT psp FROM payments WHERE id = $1", payment.id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(Psp::parse(&stored), Ok(Psp::Mock));
    tx.rollback().await
}

#[sqlx::test]
async fn the_database_stamps_recorded_at(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;

    // Naming the column fails, even with a value of its own type.
    let err = sqlx::query!(
        "INSERT INTO merchants (id, recorded_at) VALUES ($1, DEFAULT)",
        Uuid::now_v7()
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let err = sqlx::query!(
        "INSERT INTO payments (id, merchant_id, psp, currency, amount, fee_bps, recorded_at) \
         VALUES ($1, $2, 'mock', 'EUR', 100, 0, DEFAULT)",
        Uuid::now_v7(),
        merchant
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);

    // Stamped with the start of the inserting database transaction.
    let mut tx = runtime_transaction(&pool).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    let stamped = sqlx::query_scalar!(
        r#"SELECT recorded_at = transaction_timestamp() AS "stamped!"
             FROM payments WHERE id = $1"#,
        payment.id
    )
    .fetch_one(&mut *tx)
    .await?;
    assert!(stamped);
    tx.rollback().await
}

#[sqlx::test]
async fn the_runtime_role_cannot_change_merchants_or_payments(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    for err in [
        sqlx::query!("UPDATE merchants SET id = id")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM merchants")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE merchants CASCADE")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("UPDATE payments SET amount = 1")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM payments")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE payments CASCADE")
            .execute(&mut app)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    Ok(())
}

#[sqlx::test]
async fn merchants_and_payments_are_append_only_even_for_the_owner(
    pool: PgPool,
) -> sqlx::Result<()> {
    let merchant = register_merchant(&mut *pool.acquire().await?).await?;

    let update = sqlx::query!("UPDATE merchants SET id = id")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "merchants_append_only", "merchants");
    let delete = sqlx::query!("DELETE FROM merchants")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "merchants_append_only", "merchants");
    // CASCADE, because a foreign key refers to these tables: without it,
    // PostgreSQL fails with 0A000 before any trigger runs. The triggers of the
    // named table fire first, which the trigger name proves.
    let truncate = sqlx::query!("TRUNCATE merchants CASCADE")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "merchants_no_truncate", "merchants");

    // The row stays uncommitted, because a committed payment needs its first
    // step. A failed statement ends a transaction, so each change gets its own.
    let payment = NewPayment::valid(merchant);
    let mut tx = pool.begin().await?;
    payment.insert(&mut tx).await?;
    let update = sqlx::query!("UPDATE payments SET amount = 1")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "payments_append_only", "payments");
    tx.rollback().await?;

    let mut tx = pool.begin().await?;
    payment.insert(&mut tx).await?;
    let delete = sqlx::query!("DELETE FROM payments")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "payments_append_only", "payments");
    tx.rollback().await?;

    let truncate = sqlx::query!("TRUNCATE payments CASCADE")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "payments_no_truncate", "payments");
    Ok(())
}

/// The CHECK that lists the legal moves.
const EDGE_CHECK: &str = "payment_transitions_edge_check";
/// The CHECK that states each kind's amount rule.
const MONEY_CHECK: &str = "payment_transitions_money_check";
/// The foreign key that chains a step to its predecessor.
const PREV_FKEY: &str = "payment_transitions_prev_fkey";
/// SQLSTATE of a payment without its first step (ADR-0013).
const NO_GENESIS: &str = "LG005";

/// One row of a payment's history as the runtime role writes it.
#[derive(Debug, Clone, Copy)]
struct Step {
    payment_id: Uuid,
    seq: i32,
    prev_seq: i32,
    id: Uuid,
    kind: &'static str,
    from_status: &'static str,
    to_status: &'static str,
    amount: i64,
    prev_refundable: i64,
    refundable: i64,
}

impl Step {
    /// A payment's first step, which points to itself.
    fn first(payment_id: Uuid) -> Self {
        Self {
            payment_id,
            seq: 0,
            prev_seq: 0,
            id: Uuid::now_v7(),
            kind: "create",
            from_status: "created",
            to_status: "created",
            amount: 0,
            prev_refundable: 0,
            refundable: 0,
        }
    }

    /// The step after this one, starting from its status and refundable
    /// amount.
    fn then(
        &self,
        kind: &'static str,
        to_status: &'static str,
        amount: i64,
        refundable: i64,
    ) -> Self {
        Self {
            payment_id: self.payment_id,
            seq: self.seq.checked_add(1).expect("a short history"),
            prev_seq: self.seq,
            id: Uuid::now_v7(),
            kind,
            from_status: self.to_status,
            to_status,
            amount,
            prev_refundable: self.refundable,
            refundable,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_transitions \
                 (payment_id, seq, prev_seq, id, kind, from_status, to_status, \
                  amount, prev_refundable, refundable) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            self.payment_id,
            self.seq,
            self.prev_seq,
            self.id,
            self.kind,
            self.from_status,
            self.to_status,
            self.amount,
            self.prev_refundable,
            self.refundable
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Inserts the step and returns the error, failing the test if the insert
    /// succeeds.
    async fn insert_rejected(&self, conn: &mut PgConnection) -> sqlx::Error {
        self.insert(conn)
            .await
            .err()
            .unwrap_or_else(|| panic!("{self:?} was accepted"))
    }

    /// [`Self::insert_rejected`] in a savepoint, so that the rejection does not
    /// end the surrounding transaction.
    async fn rejected_in(&self, tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<sqlx::Error> {
        savepoint(tx).await?;
        let err = self.insert_rejected(tx).await;
        back_to_savepoint(tx).await?;
        Ok(err)
    }
}

/// Registers a merchant, then writes a payment with its first step.
async fn payment_with_first_step(conn: &mut PgConnection) -> sqlx::Result<Step> {
    let merchant = register_merchant(conn).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(conn).await?;
    let first = Step::first(payment.id);
    first.insert(conn).await?;
    Ok(first)
}

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

/// The requested amount of [`NewPayment::valid`].
const REQUESTED: i64 = 10_000;
/// What the test payments are authorized for, and then captured.
const AUTHORIZED: i64 = 8_000;
/// The fee on that capture, at the 250 basis points of [`NewPayment::valid`].
const FEE: i64 = 200;
/// SQLSTATE of a step without its record (ADR-0013).
const NO_RECORD: &str = "LG004";
/// The trigger that checks each step at COMMIT.
const STEP_COMMIT_CHECK: &str = "payment_transitions_commit_check";

/// Sets a savepoint that [`back_to_savepoint`] returns to, so that a rejected
/// statement does not end the surrounding transaction.
async fn savepoint(tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<()> {
    sqlx::query!("SAVEPOINT rejected")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn back_to_savepoint(tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<()> {
    sqlx::query!("ROLLBACK TO SAVEPOINT rejected")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A merchant and the EUR accounts that its payments move money between.
#[derive(Debug, Clone, Copy)]
struct Books {
    merchant: Uuid,
    receivable: Uuid,
    payable: Uuid,
    fees: Uuid,
}

impl Books {
    /// ADR-0020's lines for a capture, a zero line left out.
    fn capture_lines(&self, captured: i64, fee: i64) -> Vec<(Uuid, i64)> {
        [
            (self.receivable, captured),
            (self.payable, fee - captured),
            (self.fees, -fee),
        ]
        .into_iter()
        .filter(|line| line.1 != 0)
        .collect()
    }

    /// ADR-0020's lines for a refund.
    fn refund_lines(&self, refund: i64) -> Vec<(Uuid, i64)> {
        vec![(self.payable, refund), (self.receivable, -refund)]
    }
}

async fn open_account(
    conn: &mut PgConnection,
    kind: &str,
    party: &str,
    side: &str,
) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency, normal_side) \
         VALUES ($1, $2, $3, 'EUR', $4)",
        id,
        kind,
        party,
        side
    )
    .execute(conn)
    .await?;
    Ok(id)
}

async fn open_books(conn: &mut PgConnection) -> sqlx::Result<Books> {
    let merchant = register_merchant(conn).await?;
    Ok(Books {
        merchant,
        receivable: open_account(conn, "psp_receivable", Psp::Mock.as_str(), "debit").await?,
        payable: open_account(
            conn,
            "merchant_payable",
            &merchant.hyphenated().to_string(),
            "credit",
        )
        .await?,
        fees: open_account(conn, "fee_revenue", "platform", "credit").await?,
    })
}

/// Writes an EUR posting with these lines and returns its ID.
async fn write_posting(conn: &mut PgConnection, lines: &[(Uuid, i64)]) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    let entry_count = i32::try_from(lines.len()).expect("a few lines");
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, 'EUR')",
        id,
        entry_count
    )
    .execute(&mut *conn)
    .await?;
    for (line_no, (account, amount)) in (1..).zip(lines) {
        sqlx::query!(
            "INSERT INTO ledger_entries (transaction_id, line_no, entry_count, account_id, currency, amount) \
             VALUES ($1, $2, $3, $4, 'EUR', $5)",
            id,
            line_no,
            entry_count,
            account,
            amount
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

async fn insert_authorization(
    conn: &mut PgConnection,
    step: &Step,
    payment_amount: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_authorizations \
             (transition_id, payment_id, amount, payment_amount, expires_at) \
         VALUES ($1, $2, $3, $4, transaction_timestamp())",
        step.id,
        step.payment_id,
        step.amount,
        payment_amount
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn insert_capture(
    conn: &mut PgConnection,
    step: &Step,
    authorized: i64,
    fee: i64,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_captures \
             (transition_id, payment_id, amount, authorized_amount, fee, captured_at, \
              ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, $5, transaction_timestamp(), $6)",
        step.id,
        step.payment_id,
        step.amount,
        authorized,
        fee,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn insert_refund(conn: &mut PgConnection, step: &Step, posting: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_refunds \
             (transition_id, payment_id, amount, refunded_at, ledger_transaction_id) \
         VALUES ($1, $2, $3, transaction_timestamp(), $4)",
        step.id,
        step.payment_id,
        step.amount,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Writes a payment up to its authorization, with its record.
async fn authorized_payment(conn: &mut PgConnection, books: &Books) -> sqlx::Result<Step> {
    let payment = NewPayment::valid(books.merchant);
    payment.insert(conn).await?;
    let first = Step::first(payment.id);
    first.insert(conn).await?;
    let authorized = first.then("authorize", "authorized", AUTHORIZED, 0);
    authorized.insert(conn).await?;
    insert_authorization(conn, &authorized, REQUESTED).await?;
    Ok(authorized)
}

/// Writes a payment up to its capture, with its records and posting.
async fn captured_payment(conn: &mut PgConnection, books: &Books) -> sqlx::Result<Step> {
    let authorized = authorized_payment(conn, books).await?;
    let captured = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    captured.insert(conn).await?;
    let posting = write_posting(conn, &books.capture_lines(AUTHORIZED, FEE)).await?;
    insert_capture(conn, &captured, AUTHORIZED, FEE, posting).await?;
    Ok(captured)
}

/// Writes a refund after `previous`, with its record and posting.
async fn refund_step(
    conn: &mut PgConnection,
    books: &Books,
    previous: &Step,
    to_status: &'static str,
    amount: i64,
) -> sqlx::Result<Step> {
    let refund = previous.then("refund", to_status, amount, previous.refundable - amount);
    refund.insert(conn).await?;
    let posting = write_posting(conn, &books.refund_lines(amount)).await?;
    insert_refund(conn, &refund, posting).await?;
    Ok(refund)
}

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
    settled.insert(&mut tx).await?;
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
        .insert(&mut tx)
        .await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, table);

    let mut tx = runtime_transaction(&pool).await?;
    let authorized = authorized_payment(&mut tx, &books).await?;
    authorized
        .then("capture", "captured", AUTHORIZED, AUTHORIZED)
        .insert(&mut tx)
        .await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, table);

    // The library reads this code as a proven rollback (ADR-0014).
    let mut tx = runtime_transaction(&pool).await?;
    let captured = captured_payment(&mut tx, &books).await?;
    captured
        .then("refund", "captured", 1, AUTHORIZED - 1)
        .insert(&mut tx)
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
        first.then(kind, to_status, 0, 0).insert(&mut tx).await?;
        tx.commit().await?;
    }
    for kind in ["void", "expire"] {
        let mut tx = runtime_transaction(&pool).await?;
        let authorized = authorized_payment(&mut tx, &books).await?;
        authorized
            .then(kind, "voided", 0, 0)
            .insert(&mut tx)
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
        .insert(&mut tx)
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
