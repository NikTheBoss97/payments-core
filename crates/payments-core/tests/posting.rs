// Author: Nicola Antonello

//! Posting through the library (ADR-0015), as the runtime role. `post` is
//! idempotent by transaction ID, reads a failed COMMIT as ADR-0014 says, and
//! settles an unknown outcome when the same value is posted again
//! (ADR-0005). Faults that the types rule out are injected with triggers
//! that the owner adds to the test's own database.

mod common;

use std::error::Error;

use common::{
    CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, UNIQUE_VIOLATION, account_id, assert_guard,
    assert_violation, runtime_pool, runtime_pool_with_lock_timeout, runtime_transaction, sqlstate,
    transaction_id,
};
use payments_core::accounts::{AccountKey, AccountKind, NormalSide, Party, open_account};
use payments_core::db::commit;
use payments_core::ids::{AccountId, TransactionId};
use payments_core::ledger::{Entry, LedgerTransaction};
use payments_core::money::{Amount, Currency};
use payments_core::posting::{PostError, PostInError, Posted, post, post_in};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// The trigger that checks each ledger transaction at COMMIT.
const COMMIT_CHECK: &str = "ledger_transactions_commit_check";

/// The accounts the postings move money between: three in EUR, one in USD.
struct Accounts {
    receivable: AccountId,
    payable: AccountId,
    fees: AccountId,
    usd_receivable: AccountId,
}

async fn open(
    app: &PgPool,
    n: u32,
    kind: &str,
    currency: &str,
    side: NormalSide,
) -> Result<AccountId, Box<dyn Error>> {
    let key = AccountKey {
        kind: AccountKind::parse(kind)?,
        party: Party::label("platform")?,
        currency: Currency::parse(currency)?,
    };
    Ok(open_account(app, account_id(n), &key, side).await?)
}

async fn open_accounts(app: &PgPool) -> Result<Accounts, Box<dyn Error>> {
    Ok(Accounts {
        receivable: open(app, 1, "psp_receivable", "EUR", NormalSide::Debit).await?,
        payable: open(app, 2, "merchant_payable", "EUR", NormalSide::Credit).await?,
        fees: open(app, 3, "fee_revenue", "EUR", NormalSide::Credit).await?,
        usd_receivable: open(app, 4, "psp_receivable", "USD", NormalSide::Debit).await?,
    })
}

/// An EUR ledger transaction with these lines.
fn eur(id: TransactionId, lines: &[(AccountId, i64)]) -> LedgerTransaction {
    let entries = lines
        .iter()
        .map(|&(account, minor)| Entry {
            account,
            amount: Amount::new(minor).expect("valid amount"),
        })
        .collect();
    let eur = Currency::parse("EUR").expect("valid code");
    LedgerTransaction::new(id, eur, entries).expect("balanced")
}

/// A capture of 100.00 EUR: 97.00 owed to the merchant, 3.00 of fees.
fn capture(id: TransactionId, accounts: &Accounts) -> LedgerTransaction {
    eur(
        id,
        &[
            (accounts.receivable, 10_000),
            (accounts.payable, -9_700),
            (accounts.fees, -300),
        ],
    )
}

/// How many headers and entries are stored.
async fn stored(pool: &PgPool) -> sqlx::Result<(i64, i64)> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM ledger_transactions) AS "headers!",
                  (SELECT count(*) FROM ledger_entries) AS "entries!""#
    )
    .fetch_one(pool)
    .await?;
    Ok((row.headers, row.entries))
}

#[sqlx::test]
async fn a_balanced_posting_is_applied_and_stored_as_built(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    assert_eq!(post(&app, &transaction).await?, Posted::Applied);
    let rows = sqlx::query!(
        r#"SELECT h.entry_count, h.currency AS "currency!", e.line_no, e.account_id, e.amount
             FROM ledger_transactions h
             JOIN ledger_entries e ON e.transaction_id = h.id
            WHERE h.id = $1
            ORDER BY e.line_no"#,
        transaction.id().get()
    )
    .fetch_all(&pool)
    .await?;
    let lines: Vec<_> = rows
        .into_iter()
        .map(|r| (r.entry_count, r.currency, r.line_no, r.account_id, r.amount))
        .collect();
    let eur = String::from("EUR");
    assert_eq!(
        lines,
        [
            (3, eur.clone(), 1, accounts.receivable.get(), 10_000),
            (3, eur.clone(), 2, accounts.payable.get(), -9_700),
            (3, eur, 3, accounts.fees.get(), -300),
        ]
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn posting_the_same_value_again_is_already_applied(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    assert_eq!(post(&app, &transaction).await?, Posted::Applied);
    assert_eq!(post(&app, &transaction).await?, Posted::AlreadyApplied);
    assert_eq!(stored(&pool).await?, (1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_same_id_with_other_entries_is_a_mismatch(pool: PgPool) -> TestResult {
    // Reusing an ID has to be written out: TransactionId is neither Copy
    // nor Clone (ADR-0005).
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    post(&app, &capture(transaction_id(1), &accounts)).await?;
    let other = eur(
        transaction_id(1),
        &[(accounts.receivable, 5_000), (accounts.payable, -5_000)],
    );
    let err = post(&app, &other).await.unwrap_err();
    assert!(
        matches!(err, PostError::Mismatch { id } if id == transaction_id(1).get()),
        "{err:?}"
    );
    assert_eq!(stored(&pool).await?, (1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_entry_needs_an_account_in_the_transactions_currency(pool: PgPool) -> TestResult {
    // A USD account in an EUR posting, and an account that was never opened:
    // one foreign key reports both.
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    for (n, account) in [(1, accounts.usd_receivable), (2, account_id(99))] {
        let transaction = eur(
            transaction_id(n),
            &[(account, 100), (accounts.payable, -100)],
        );
        let err = post(&app, &transaction).await.unwrap_err();
        let PostError::AccountNotInCurrency {
            currency, source, ..
        } = err
        else {
            panic!("expected AccountNotInCurrency, got {err:?}");
        };
        assert_eq!(currency.as_str(), "EUR");
        assert_violation(source, FOREIGN_KEY_VIOLATION, "ledger_entries_account_fkey");
    }
    assert_eq!(stored(&pool).await?, (0, 0));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_commit_check_catches_an_unbalanced_write(pool: PgPool) -> TestResult {
    // The type cannot be built unbalanced, so a trigger unbalances the
    // write on its way in: the database's own check is the second layer
    // (ADR-0004).
    sqlx::query!(
        "CREATE FUNCTION unbalance_line_one() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.line_no = 1 THEN NEW.amount := NEW.amount + 1; END IF; \
         RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER unbalance BEFORE INSERT ON ledger_entries \
         FOR EACH ROW EXECUTE FUNCTION unbalance_line_one()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let err = post(&app, &capture(transaction_id(1), &accounts))
        .await
        .unwrap_err();
    let PostError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(source, "LG003", COMMIT_CHECK, "ledger_transactions");
    assert_eq!(stored(&pool).await?, (0, 0));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_commit_check_catches_a_missing_entry(pool: PgPool) -> TestResult {
    // A trigger that silently drops the last line.
    sqlx::query!(
        "CREATE FUNCTION drop_line_three() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.line_no = 3 THEN RETURN NULL; END IF; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER drop_line BEFORE INSERT ON ledger_entries \
         FOR EACH ROW EXECUTE FUNCTION drop_line_three()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let err = post(&app, &capture(transaction_id(1), &accounts))
        .await
        .unwrap_err();
    let PostError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(source, "LG002", COMMIT_CHECK, "ledger_transactions");
    assert_eq!(stored(&pool).await?, (0, 0));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_write_that_breaks_a_check_is_an_invariant(pool: PgPool) -> TestResult {
    // A trigger that zeroes every amount, which ledger_entries_amount_check
    // rejects at INSERT.
    sqlx::query!(
        "CREATE FUNCTION zero_amounts() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.amount := 0; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER zero BEFORE INSERT ON ledger_entries \
         FOR EACH ROW EXECUTE FUNCTION zero_amounts()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let err = post(&app, &capture(transaction_id(1), &accounts))
        .await
        .unwrap_err();
    let PostError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(source, CHECK_VIOLATION, "ledger_entries_amount_check");
    assert_eq!(stored(&pool).await?, (0, 0));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unproven_commit_failure_is_settled_by_posting_again(pool: PgPool) -> TestResult {
    // A deferred trigger ends the session during COMMIT, which reports
    // FATAL 57P01: no proof of a rollback, so post() reports NotConfirmed
    // (ADR-0014). The function runs as its owner, the superuser, which may
    // end a session that the superuser opened.
    sqlx::query!(
        "CREATE FUNCTION end_own_session() RETURNS trigger LANGUAGE plpgsql \
         SECURITY DEFINER AS $$ \
         BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER zz_end_session AFTER INSERT ON ledger_transactions \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION end_own_session()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    let err = post(&app, &transaction).await.unwrap_err();
    assert!(matches!(err, PostError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(stored(&pool).await?, (0, 0));
    // The caller's one rule: post the same value again.
    sqlx::query!("DROP TRIGGER zz_end_session ON ledger_transactions")
        .execute(&pool)
        .await?;
    assert_eq!(post(&app, &transaction).await?, Posted::Applied);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn posting_again_waits_for_an_open_first_attempt(pool: PgPool) -> TestResult {
    // The first attempt has written but not committed, as when its COMMIT
    // is still on the way. Posting again waits on its rows, gives up at the
    // lock timeout with an unknown outcome, and settles once the first
    // attempt ends: committed here, rolled back in the next test.
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    let mut first = runtime_transaction(&pool).await?;
    post_in(&mut first, &transaction).await?;
    let retry = runtime_pool_with_lock_timeout(&pool, 200).await?;
    let err = post(&retry, &transaction).await.unwrap_err();
    let PostError::NotConfirmed { source, .. } = err else {
        panic!("expected NotConfirmed, got {err:?}");
    };
    assert_eq!(sqlstate(source), "55P03", "lock_not_available");
    first.commit().await?;
    assert_eq!(post(&retry, &transaction).await?, Posted::AlreadyApplied);
    assert_eq!(stored(&pool).await?, (1, 3));
    retry.close().await;
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn posting_again_applies_after_a_first_attempt_rolls_back(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    let mut first = runtime_transaction(&pool).await?;
    post_in(&mut first, &transaction).await?;
    first.rollback().await?;
    assert_eq!(post(&app, &transaction).await?, Posted::Applied);
    assert_eq!(stored(&pool).await?, (1, 3));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn posting_again_outlasts_a_first_attempt_left_idle(pool: PgPool) -> TestResult {
    // ADR-0005's sizing, scaled down: the server ends a session left idle
    // in its transaction after 1 second here (30 in production), and a
    // retry that may wait 10 seconds for a lock (35) outlasts it, then
    // gets a definite answer.
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    let mut first = runtime_transaction(&pool).await?;
    sqlx::query!("SET LOCAL idle_in_transaction_session_timeout = '1s'")
        .execute(&mut *first)
        .await?;
    post_in(&mut first, &transaction).await?;
    let retry = runtime_pool_with_lock_timeout(&pool, 10_000).await?;
    assert_eq!(post(&retry, &transaction).await?, Posted::Applied);
    assert_eq!(stored(&pool).await?, (1, 3));
    // The server has ended the first attempt's session.
    assert!(first.commit().await.is_err());
    retry.close().await;
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn either_unique_key_means_the_posting_exists(pool: PgPool) -> TestResult {
    // A retry breaks both unique keys of ledger_transactions, and PostgreSQL
    // reports the one whose index it checks first, in OID order. Rebuilding
    // the primary key gives it a new, higher OID (ADR-0015).
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = capture(transaction_id(1), &accounts);
    assert_eq!(post(&app, &transaction).await?, Posted::Applied);
    sqlx::query!("REINDEX INDEX CONCURRENTLY ledger_transactions_pkey")
        .execute(&pool)
        .await?;
    let currency = transaction.currency();
    let err = sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
        transaction.id().get(),
        transaction.entry_count(),
        currency.as_str()
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert_violation(
        err,
        UNIQUE_VIOLATION,
        "ledger_transactions_id_entry_count_currency_key",
    );
    assert_eq!(post(&app, &transaction).await?, Posted::AlreadyApplied);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_failed_statement_aborts_the_database_transaction(pool: PgPool) -> TestResult {
    // After post_in fails, PostgreSQL answers COMMIT with ROLLBACK, which
    // sqlx reports as success. That is why post() drops the transaction
    // instead of committing it (ADR-0014).
    let app = runtime_pool(&pool).await?;
    let accounts = open_accounts(&app).await?;
    let transaction = eur(
        transaction_id(1),
        &[(accounts.usd_receivable, 100), (accounts.payable, -100)],
    );
    let mut tx = runtime_transaction(&pool).await?;
    let err = post_in(&mut tx, &transaction).await.unwrap_err();
    assert!(
        matches!(err, PostInError::AccountNotInCurrency { .. }),
        "{err:?}"
    );
    commit(tx).await?;
    assert_eq!(stored(&pool).await?, (0, 0));
    app.close().await;
    Ok(())
}
