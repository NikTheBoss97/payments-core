// Author: Nicola Antonello

//! The race behind ADR-0014: a COMMIT that commits, then returns an error.
//!
//! PostgreSQL holds a cancel that arrives while it commits until the commit
//! is recorded. When it logs statement durations, it services the cancel
//! while writing the duration line, after CommandComplete, so the client
//! receives ERROR 57014 for a transaction that committed. The test widens
//! the window with `commit_delay`, a sleep before the WAL flush, and cancels
//! the backend while it runs COMMIT. Timing decides where each cancel lands,
//! so the test tries several times and needs one hit; every attempt, hit or
//! miss, must be reported safely.

mod common;

use std::error::Error;
use std::time::{Duration, Instant};

use common::{account_id, runtime_pool, sqlstate, transaction_id};
use futures_util::future::join;
use payments_core::accounts::{AccountKey, AccountKind, NormalSide, Party, open_account};
use payments_core::ids::{AccountId, TransactionId};
use payments_core::ledger::{Entry, LedgerTransaction};
use payments_core::money::{Amount, Currency};
use payments_core::posting::{PostError, Posted, post};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

type TestResult = Result<(), Box<dyn Error>>;

/// How many times the test tries to land a cancel inside a commit.
const ATTEMPTS: u32 = 20;

/// A one-connection pool, so the test knows the backend that posts. Its
/// session logs every statement's duration and sleeps 100 ms before each
/// commit's WAL flush, then acts as the runtime role. Both settings are
/// superuser-only, so they are set before SET ROLE; they last for the
/// session.
async fn racing_pool(pool: &PgPool) -> sqlx::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query!("SET log_min_duration_statement = 0")
                    .execute(&mut *conn)
                    .await?;
                // Microseconds. commit_siblings = 0 makes the sleep
                // unconditional; it needs fsync, which is on.
                sqlx::query!("SET commit_delay = 100000")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query!("SET commit_siblings = 0")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query!("SET ROLE payments_app")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
}

/// Waits until the backend runs COMMIT, then cancels it. Gives up after a
/// second, when the posting has failed or ended before its COMMIT.
async fn cancel_during_commit(pool: &PgPool, pid: i32) -> sqlx::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        let committing = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                               WHERE pid = $1 AND state = 'active' AND query = 'COMMIT')
                   AS "committing!""#,
            pid
        )
        .fetch_one(pool)
        .await?;
        if committing {
            sqlx::query_scalar!(r#"SELECT pg_cancel_backend($1) AS "sent!""#, pid)
                .fetch_one(pool)
                .await?;
            return Ok(());
        }
    }
    Ok(())
}

async fn open(
    app: &PgPool,
    n: u32,
    kind: &str,
    side: NormalSide,
) -> Result<AccountId, Box<dyn Error>> {
    let key = AccountKey {
        kind: AccountKind::parse(kind)?,
        party: Party::label("platform")?,
        currency: Currency::parse("EUR")?,
    };
    Ok(open_account(app, account_id(n), &key, side).await?)
}

fn capture(id: TransactionId, receivable: AccountId, payable: AccountId) -> LedgerTransaction {
    let entries = [(receivable, 10_000), (payable, -10_000)]
        .into_iter()
        .map(|(account, minor)| Entry {
            account,
            amount: Amount::new(minor).expect("valid amount"),
        })
        .collect();
    let eur = Currency::parse("EUR").expect("valid code");
    LedgerTransaction::new(id, eur, entries).expect("balanced")
}

/// Whether a ledger transaction with this ID is stored.
async fn is_stored(pool: &PgPool, id: &TransactionId) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM ledger_transactions WHERE id = $1) AS "stored!""#,
        id.get()
    )
    .fetch_one(pool)
    .await
}

#[sqlx::test]
async fn a_commit_that_commits_and_then_fails_is_settled_by_posting_again(
    pool: PgPool,
) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let receivable = open(&app, 1, "psp_receivable", NormalSide::Debit).await?;
    let payable = open(&app, 2, "merchant_payable", NormalSide::Credit).await?;
    let racing = racing_pool(&pool).await?;
    let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
        .fetch_one(&racing)
        .await?;

    let mut hits = 0;
    for attempt in 1..=ATTEMPTS {
        let posting = capture(transaction_id(attempt), receivable, payable);
        let (posted, cancelled) =
            join(post(&racing, &posting), cancel_during_commit(&pool, pid)).await;
        cancelled?;
        let stored = is_stored(&pool, posting.id()).await?;
        match posted {
            // Applied must mean stored.
            Ok(Posted::Applied) => assert!(stored, "attempt {attempt}: Applied, not stored"),
            // NotConfirmed is the one answer that may go either way.
            Err(PostError::NotConfirmed { source, .. }) => {
                if stored {
                    // The race: an error for a COMMIT that committed. Under
                    // the rule of ADR-0004 and ADR-0013 that ADR-0014
                    // replaced, this posting would count as rolled back.
                    assert_eq!(sqlstate(source), "57014", "query_canceled");
                    assert_eq!(post(&app, &posting).await?, Posted::AlreadyApplied);
                    hits += 1;
                }
            }
            other => panic!("attempt {attempt}: {other:?}"),
        }
    }
    assert!(
        hits > 0,
        "no cancel landed inside a commit in {ATTEMPTS} attempts"
    );
    racing.close().await;
    app.close().await;
    Ok(())
}
