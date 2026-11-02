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
use sqlx::{PgConnection, PgPool};
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
