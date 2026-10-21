// Author: Nicola Antonello

//! sqlx #4393, the reason for the git pin (ADR-0006): a cancelled `begin()`.
//!
//! sqlx 0.9.0 counts a transaction only once BEGIN has returned. When the
//! future of `begin()` is dropped while the server still runs BEGIN, nothing
//! queues a ROLLBACK, and the pool hands the connection, still inside the
//! transaction, to the next caller, whose statements then run in it. The fix
//! (#4394) counts the transaction before the round trip, so the drop queues a
//! ROLLBACK, which the pool sends before it reuses the connection.
//!
//! The test drops the future at a known point: while the server waits for an
//! advisory lock that another connection holds. Every failure of this test
//! on a newer sqlx means the pin cannot move there. It fails on 0.9.0.

use std::error::Error;
use std::time::{Duration, Instant};

use futures_util::future::{Either, select};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

type TestResult = Result<(), Box<dyn Error>>;

/// Waits until backend `pid` waits for an advisory lock. Panics after ten
/// seconds, so a BEGIN that never reaches the lock fails instead of hanging.
async fn waiting_for_advisory_lock(pool: &PgPool, pid: i32) -> sqlx::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiting = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                               WHERE pid = $1 AND wait_event_type = 'Lock'
                                 AND wait_event = 'advisory')
                   AS "waiting!""#,
            pid
        )
        .fetch_one(pool)
        .await?;
        if waiting {
            return Ok(());
        }
        assert!(
            Instant::now() < deadline,
            "backend {pid} never waited for the advisory lock"
        );
    }
}

#[sqlx::test]
async fn a_cancelled_begin_leaves_its_connection_idle(pool: PgPool) -> TestResult {
    // One connection, so the test knows the backend that the pool hands out.
    let single = PgPoolOptions::new()
        .max_connections(1)
        .connect_with((*pool.connect_options()).clone())
        .await?;
    let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
        .fetch_one(&single)
        .await?;

    // Another connection holds advisory lock 1 until its transaction ends.
    // Advisory locks are per database, and each test has its own database.
    let mut holder = pool.begin().await?;
    sqlx::query_scalar!(r#"SELECT 1 AS "locked!" FROM pg_advisory_xact_lock(1)"#)
        .fetch_one(&mut *holder)
        .await?;

    // What CLAUDE.md forbids in the application, on purpose: the future of a
    // begin() is dropped while the server runs its BEGIN, as a timeout or a
    // select! would drop it. Its statement waits for lock 1, so the drop
    // lands while the server is inside the new transaction.
    let begin = Box::pin(single.begin_with("BEGIN; SELECT pg_advisory_xact_lock(1)"));
    let waiting = Box::pin(waiting_for_advisory_lock(&pool, pid));
    match select(begin, waiting).await {
        Either::Left(_) => panic!("BEGIN returned while another connection held the lock"),
        Either::Right((waited, begin)) => {
            waited?;
            drop(begin);
        }
    }

    // Releasing the lock lets the server finish the BEGIN; the pool, which
    // has taken the connection back, then drains its replies.
    holder.rollback().await?;

    // acquire() returns only once the pool has drained the connection and
    // released it, and the pool has a single connection to hand out.
    let mut reused = single.acquire().await?;
    let reused_pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
        .fetch_one(&mut *reused)
        .await?;
    assert_eq!(reused_pid, pid, "the pool replaced the connection");
    let state = sqlx::query_scalar!(
        r#"SELECT state AS "state!" FROM pg_stat_activity WHERE pid = $1"#,
        pid
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        state, "idle",
        "the cancelled BEGIN's transaction is still open"
    );

    drop(reused);
    single.close().await;
    Ok(())
}
