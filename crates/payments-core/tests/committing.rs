// Author: Nicola Antonello

//! Committing through `db::commit`, which reads a failed `COMMIT` as
//! ADR-0014 says: only the ledger's deferred-check codes prove a rollback,
//! and any other failure leaves the outcome unknown.

mod common;

use std::error::Error;

use common::{account_id, assert_guard, runtime_pool, runtime_transaction};
use payments_core::accounts::{AccountKey, AccountKind, NormalSide, Party, open_account};
use payments_core::db::{CommitError, commit};
use payments_core::money::Currency;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// Opens two EUR accounts through the library and returns their IDs.
async fn two_accounts(pool: &PgPool) -> Result<[Uuid; 2], Box<dyn Error>> {
    let app = runtime_pool(pool).await?;
    let mut ids = Vec::new();
    for (n, kind, side) in [
        (1, "psp_receivable", NormalSide::Debit),
        (2, "merchant_payable", NormalSide::Credit),
    ] {
        let key = AccountKey {
            kind: AccountKind::parse(kind)?,
            party: Party::label("platform")?,
            currency: Currency::parse("EUR")?,
        };
        ids.push(open_account(&app, account_id(n), &key, side).await?.get());
    }
    app.close().await;
    Ok([ids[0], ids[1]])
}

/// Writes a ledger transaction with raw SQL: a header that declares
/// `entry_count` entries, then one entry per amount, alternating between the
/// two accounts.
async fn write_posting(
    conn: &mut PgConnection,
    accounts: [Uuid; 2],
    entry_count: i32,
    amounts: &[i64],
) -> sqlx::Result<()> {
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
        id,
        entry_count,
        "EUR"
    )
    .execute(&mut *conn)
    .await?;
    for (line_no, (&amount, &account)) in (1..).zip(amounts.iter().zip(accounts.iter().cycle())) {
        sqlx::query!(
            "INSERT INTO ledger_entries \
             (transaction_id, line_no, entry_count, account_id, currency, amount) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            id,
            line_no,
            entry_count,
            account,
            "EUR",
            amount
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// How many ledger transactions are stored.
async fn headers(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM ledger_transactions"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn a_successful_commit_stores_the_transaction(pool: PgPool) -> TestResult {
    let accounts = two_accounts(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    write_posting(&mut tx, accounts, 2, &[100, -100]).await?;
    commit(tx).await?;
    assert_eq!(headers(&pool).await?, 1);
    Ok(())
}

#[sqlx::test]
async fn a_deferred_checks_code_proves_a_rollback(pool: PgPool) -> TestResult {
    // The commit-time check raises LG002 or LG003 before PostgreSQL writes
    // the commit record, so the transaction certainly rolled back.
    let accounts = two_accounts(&pool).await?;
    for (amounts, code) in [(&[100][..], "LG002"), (&[100, -90][..], "LG003")] {
        let mut tx = runtime_transaction(&pool).await?;
        write_posting(&mut tx, accounts, 2, amounts).await?;
        let err = commit(tx).await.unwrap_err();
        let CommitError::Rejected(source) = err else {
            panic!("expected Rejected, got {err:?}");
        };
        assert_guard(
            source,
            code,
            "ledger_transactions_commit_check",
            "ledger_transactions",
        );
    }
    assert_eq!(headers(&pool).await?, 0);
    Ok(())
}

#[sqlx::test]
async fn any_other_commit_error_leaves_the_outcome_unknown(pool: PgPool) -> TestResult {
    // A deferred trigger that ends its own session during COMMIT. PostgreSQL
    // reports FATAL 57P01, a code that can also follow a COMMIT that did
    // commit (ADR-0014). Constraint triggers fire in name order, so the
    // ledger's own check passes first; this one runs before the commit
    // record, so nothing is stored. The test pins the classification, not
    // the race.
    let accounts = two_accounts(&pool).await?;
    sqlx::query!(
        "CREATE FUNCTION terminate_own_session() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER zz_terminate_at_commit AFTER INSERT ON ledger_transactions \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION terminate_own_session()"
    )
    .execute(&pool)
    .await?;
    // As the superuser, which may signal its own backend.
    let mut tx = pool.begin().await?;
    write_posting(&mut tx, accounts, 2, &[100, -100]).await?;
    let err = commit(tx).await.unwrap_err();
    assert!(matches!(err, CommitError::Unknown(_)), "{err:?}");
    assert_eq!(headers(&pool).await?, 0);
    Ok(())
}
