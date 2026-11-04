// Author: Nicola Antonello

//! Killing the backend at a chosen point (ADR-0014, ADR-0019).
//!
//! A trigger on each table ends its own session when the session asked for that:
//! `crash.point`, a session setting, names the trigger and its table. Setting it
//! needs the session that runs the command, so these helpers serve a pool of one
//! connection. The setting dies with its session, so the retry, on a new
//! connection, runs clean.

use std::error::Error;
use std::time::{Duration, Instant};

use payments_core::accounts::{AccountRef, Psp};
use payments_core::ids::MerchantId;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use super::payments::{balance_of, eur, unbalanced_currencies, uncaused_postings};

/// The points around a step's own row.
pub const STEP_POINTS: [&str; 2] = [
    "die_before payment_transitions",
    "die_after payment_transitions",
];
/// The points around a posting's rows.
pub const POSTING_POINTS: [&str; 4] = [
    "die_before ledger_transactions",
    "die_after ledger_transactions",
    "die_before ledger_entries",
    "die_after ledger_entries",
];
/// During `COMMIT`, in the deferred checks, before the commit record.
pub const AT_COMMIT: &str = "zz_die_at_commit payment_transitions";

/// Adds the triggers to the test's own database, as the owner. The function runs
/// as its owner too, the superuser, which may end a session that it opened.
pub async fn install_crash_points(pool: &PgPool) -> sqlx::Result<()> {
    sqlx::query!(
        "CREATE FUNCTION die_if_planned() RETURNS trigger LANGUAGE plpgsql \
         SECURITY DEFINER AS $$ \
         BEGIN \
             IF current_setting('crash.point', true) = TG_NAME || ' ' || TG_TABLE_NAME THEN \
                 PERFORM pg_terminate_backend(pg_backend_pid()); \
             END IF; \
             RETURN NEW; \
         END $$"
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        "DO $$ \
         DECLARE target text; \
         BEGIN \
             FOREACH target IN ARRAY ARRAY['accounts', 'payments', 'payment_transitions', \
                 'ledger_transactions', 'ledger_entries', 'payment_authorizations', \
                 'payment_captures', 'payment_refunds'] LOOP \
                 EXECUTE format('CREATE TRIGGER die_before BEFORE INSERT ON %I \
                     FOR EACH ROW EXECUTE FUNCTION die_if_planned()', target); \
                 EXECUTE format('CREATE TRIGGER die_after AFTER INSERT ON %I \
                     FOR EACH ROW EXECUTE FUNCTION die_if_planned()', target); \
             END LOOP; \
         END $$"
    )
    .execute(pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER zz_die_at_commit AFTER INSERT ON payment_transitions \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION die_if_planned()"
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Asks the pool's one session to die at this point.
pub async fn plan_death(app: &PgPool, point: &str) -> sqlx::Result<()> {
    sqlx::query!("SELECT set_config('crash.point', $1, false)", point)
        .fetch_one(app)
        .await?;
    Ok(())
}

/// How many rows each table that a command writes in its transaction holds. A command
/// that died must leave these as they were.
pub async fn stored(pool: &PgPool) -> sqlx::Result<[i64; 7]> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM payments) AS "payments!",
                  (SELECT count(*) FROM payment_transitions) AS "steps!",
                  (SELECT count(*) FROM ledger_transactions) AS "headers!",
                  (SELECT count(*) FROM ledger_entries) AS "entries!",
                  (SELECT count(*) FROM payment_authorizations) AS "authorizations!",
                  (SELECT count(*) FROM payment_captures) AS "captures!",
                  (SELECT count(*) FROM payment_refunds) AS "refunds!""#
    )
    .fetch_one(pool)
    .await?;
    Ok([
        row.payments,
        row.steps,
        row.headers,
        row.entries,
        row.authorizations,
        row.captures,
        row.refunds,
    ])
}

/// Each account's balance: the PSP's receivable, the merchant's payable, and the
/// platform's fees.
pub async fn balances(app: &PgPool, merchant: MerchantId) -> Result<[i64; 3], Box<dyn Error>> {
    Ok([
        balance_of(app, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        balance_of(app, AccountRef::MerchantPayable(merchant, eur())).await?,
        balance_of(app, AccountRef::FeeRevenue(eur())).await?,
    ])
}

/// What every crash test ends by checking.
pub async fn assert_books_are_whole(pool: &PgPool) -> sqlx::Result<()> {
    assert_eq!(unbalanced_currencies(pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(pool).await?, Vec::<Uuid>::new());
    Ok(())
}

/// How many times a test tries to land a kill inside a commit.
pub const ATTEMPTS: u32 = 20;

/// Which role a [`slow_commit_pool`] acts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    Tenant,
}

/// A one-connection pool whose session sleeps 100 ms before each commit's WAL flush,
/// to widen the window, and logs every statement's duration, because PostgreSQL
/// services the held signal while it writes that line, after `CommandComplete`. It
/// then acts as this role. These settings are for superusers only, so they come
/// before `SET ROLE`, and they last for the session.
pub async fn slow_commit_pool(pool: &PgPool, role: Role) -> sqlx::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                sqlx::query!("SET log_min_duration_statement = 0")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query!("SET commit_delay = 100000")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query!("SET commit_siblings = 0")
                    .execute(&mut *conn)
                    .await?;
                match role {
                    Role::System => sqlx::query!("SET ROLE payments_app").execute(&mut *conn),
                    Role::Tenant => sqlx::query!("SET ROLE payments_tenant").execute(&mut *conn),
                }
                .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
}

/// Waits until the backend runs `COMMIT`, then ends its session. Gives up after a
/// second, when the command has failed or ended before its `COMMIT`.
pub async fn kill_during_commit(pool: &PgPool, pid: i32) -> sqlx::Result<()> {
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
            sqlx::query_scalar!(r#"SELECT pg_terminate_backend($1) AS "sent!""#, pid)
                .fetch_one(pool)
                .await?;
            return Ok(());
        }
    }
    Ok(())
}
