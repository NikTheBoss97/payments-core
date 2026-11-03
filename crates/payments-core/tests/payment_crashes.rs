// Author: Nicola Antonello

//! The backend killed around each statement of a command (ADR-0014, ADR-0019,
//! invariant I6): a step, its posting and its record commit together or not at
//! all, and the identical command sent again settles the outcome.
//!
//! A trigger on each table ends its own session when the session asked for that:
//! `crash.point`, a session setting, names the trigger and its table. Setting it
//! needs the session that runs the command, so these tests use a pool of one
//! connection. The setting dies with its session, so the retry, on a new
//! connection, runs clean. A point that a command never reaches fails its test,
//! because the command then answers something other than `NotConfirmed`.

mod common;

use std::error::Error;
use std::time::{Duration, Instant};

use common::payments::{
    AUTHORIZED, FEE, authorize_command, balance_of, capture_command, create_command, eur, fee_rate,
    refund_command, status_of, take_lock, unbalanced_currencies, uncaused_postings,
};
use common::{
    merchant_id, runtime_pool_of_one, runtime_transaction, transition_id, wait_for_lock_wait,
};
use futures_util::future::join;
use payments_core::accounts::{AccountRef, Psp};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::PaymentStatus;
use payments_core::payments::{
    AuthorizeError, CaptureError, CreateError, Outcome, Recorded, RefundError, StepCommand,
    StepError, authorize, capture, create_payment, refund, register_merchant, void,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// The points around a step's own row.
const STEP_POINTS: [&str; 2] = [
    "die_before payment_transitions",
    "die_after payment_transitions",
];
/// The points around a posting's rows.
const POSTING_POINTS: [&str; 4] = [
    "die_before ledger_transactions",
    "die_after ledger_transactions",
    "die_before ledger_entries",
    "die_after ledger_entries",
];
/// During `COMMIT`, in the deferred checks, before the commit record.
const AT_COMMIT: &str = "zz_die_at_commit payment_transitions";

/// Adds the triggers to the test's own database, as the owner. The function runs
/// as its owner too, the superuser, which may end a session that it opened.
async fn install_crash_points(pool: &PgPool) -> sqlx::Result<()> {
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
async fn plan_death(app: &PgPool, point: &str) -> sqlx::Result<()> {
    sqlx::query!("SELECT set_config('crash.point', $1, false)", point)
        .fetch_one(app)
        .await?;
    Ok(())
}

/// How many rows each table that a command writes in its transaction holds. A command
/// that died must leave these as they were.
async fn stored(pool: &PgPool) -> sqlx::Result<[i64; 7]> {
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

/// A pool of one session, a merchant, and the crash points.
async fn crash_test(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let app = runtime_pool_of_one(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    install_crash_points(pool).await?;
    Ok((app, merchant))
}

/// Creates payment `n` and authorizes it for [`AUTHORIZED`].
async fn authorized(
    app: &PgPool,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(app, &command).await?;
    authorize(app, &authorize_command(n, command.id)).await?;
    Ok(command.id)
}

/// Each account's balance: the PSP's receivable, the merchant's payable, and the
/// platform's fees.
async fn balances(app: &PgPool, merchant: MerchantId) -> Result<[i64; 3], Box<dyn Error>> {
    Ok([
        balance_of(app, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        balance_of(app, AccountRef::MerchantPayable(merchant, eur())).await?,
        balance_of(app, AccountRef::FeeRevenue(eur())).await?,
    ])
}

/// What every crash test ends by checking.
async fn assert_books_are_whole(pool: &PgPool) -> sqlx::Result<()> {
    assert_eq!(unbalanced_currencies(pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(pool).await?, Vec::<Uuid>::new());
    Ok(())
}

#[sqlx::test]
async fn creating_a_payment_killed_anywhere_stores_nothing_and_settles(pool: PgPool) -> TestResult {
    let (app, _) = crash_test(&pool).await?;
    let points = [
        "die_before accounts",
        "die_after accounts",
        "die_before payments",
        "die_after payments",
    ]
    .into_iter()
    .chain(STEP_POINTS)
    .chain([AT_COMMIT]);
    let reply = Recorded {
        seq: 0,
        from: PaymentStatus::Created,
        to: PaymentStatus::Created,
        refundable: 0,
    };
    for (n, point) in (1..).zip(points) {
        // A merchant of its own, so that each payment has a new account to open.
        let merchant = merchant_id(n + 100);
        register_merchant(&app, merchant, fee_rate()).await?;
        let before = stored(&pool).await?;
        let command = create_command(n, merchant);
        plan_death(&app, point).await?;
        let err = create_payment(&app, &command).await.unwrap_err();
        assert!(
            matches!(err, CreateError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        // Accounts it had opened stay, committed on their own, and a retry
        // converges on them. They are not in this count.
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(
            create_payment(&app, &command).await?,
            Outcome::Applied(reply),
            "{point}"
        );
        assert_eq!(
            create_payment(&app, &command).await?,
            Outcome::AlreadyApplied(reply),
            "{point}"
        );
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_authorization_killed_anywhere_stores_nothing_and_settles(pool: PgPool) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let points = STEP_POINTS
        .into_iter()
        .chain([
            "die_before payment_authorizations",
            "die_after payment_authorizations",
        ])
        .chain([AT_COMMIT]);
    let reply = Recorded {
        seq: 1,
        from: PaymentStatus::Created,
        to: PaymentStatus::Authorized,
        refundable: 0,
    };
    for (n, point) in (1..).zip(points) {
        let create = create_command(n, merchant);
        create_payment(&app, &create).await?;
        let before = stored(&pool).await?;
        let command = authorize_command(n, create.id);
        plan_death(&app, point).await?;
        let err = authorize(&app, &command).await.unwrap_err();
        assert!(
            matches!(err, AuthorizeError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(status_of(&app, create.id).await?, PaymentStatus::Created);
        assert_eq!(
            authorize(&app, &command).await?,
            Outcome::Applied(reply),
            "{point}"
        );
        assert_eq!(
            authorize(&app, &command).await?,
            Outcome::AlreadyApplied(reply),
            "{point}"
        );
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_void_killed_anywhere_stores_nothing_and_settles(pool: PgPool) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let reply = Recorded {
        seq: 2,
        from: PaymentStatus::Authorized,
        to: PaymentStatus::Voided,
        refundable: 0,
    };
    for (n, point) in (1..).zip(STEP_POINTS.into_iter().chain([AT_COMMIT])) {
        let payment = authorized(&app, merchant, n).await?;
        let before = stored(&pool).await?;
        let command = StepCommand {
            step: transition_id(n + 5000),
            payment,
        };
        plan_death(&app, point).await?;
        let err = void(&app, &command).await.unwrap_err();
        assert!(
            matches!(err, StepError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(status_of(&app, payment).await?, PaymentStatus::Authorized);
        assert_eq!(
            void(&app, &command).await?,
            Outcome::Applied(reply),
            "{point}"
        );
        assert_eq!(
            void(&app, &command).await?,
            Outcome::AlreadyApplied(reply),
            "{point}"
        );
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_capture_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    // The heart of invariant I6: at no point can a step exist without its posting
    // and its record, or money move without them.
    let (app, merchant) = crash_test(&pool).await?;
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_captures", "die_after payment_captures"])
        .chain([AT_COMMIT])
        .collect();
    let reply = Recorded {
        seq: 2,
        from: PaymentStatus::Authorized,
        to: PaymentStatus::Captured,
        refundable: AUTHORIZED,
    };
    let mut captures = 0;
    for (n, point) in (1..).zip(points) {
        let payment = authorized(&app, merchant, n).await?;
        let before = stored(&pool).await?;
        let money = balances(&app, merchant).await?;
        let command = capture_command(n, payment);
        plan_death(&app, point).await?;
        let err = capture(&app, &command).await.unwrap_err();
        assert!(
            matches!(err, CaptureError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(balances(&app, merchant).await?, money, "{point}");
        assert_eq!(status_of(&app, payment).await?, PaymentStatus::Authorized);
        assert_eq!(
            capture(&app, &command).await?,
            Outcome::Applied(reply),
            "{point}"
        );
        assert_eq!(
            capture(&app, &command).await?,
            Outcome::AlreadyApplied(reply),
            "{point}"
        );
        captures += 1;
    }
    // Each capture moved its money once, for all the deaths and retries.
    assert_eq!(
        balances(&app, merchant).await?,
        [
            captures * AUTHORIZED,
            captures * (FEE - AUTHORIZED),
            -captures * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refund_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_refunds", "die_after payment_refunds"])
        .chain([AT_COMMIT])
        .collect();
    let reply = Recorded {
        seq: 3,
        from: PaymentStatus::Captured,
        to: PaymentStatus::Captured,
        refundable: AUTHORIZED - 3_000,
    };
    let mut refunds = 0;
    for (n, point) in (1..).zip(points) {
        let payment = authorized(&app, merchant, n).await?;
        capture(&app, &capture_command(n, payment)).await?;
        let before = stored(&pool).await?;
        let money = balances(&app, merchant).await?;
        let command = refund_command(n, payment, 3_000);
        plan_death(&app, point).await?;
        let err = refund(&app, &command).await.unwrap_err();
        assert!(
            matches!(err, RefundError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(balances(&app, merchant).await?, money, "{point}");
        assert_eq!(
            refund(&app, &command).await?,
            Outcome::Applied(reply),
            "{point}"
        );
        assert_eq!(
            refund(&app, &command).await?,
            Outcome::AlreadyApplied(reply),
            "{point}"
        );
        refunds += 1;
    }
    assert_eq!(
        balances(&app, merchant).await?,
        [
            refunds * (AUTHORIZED - 3_000),
            refunds * (FEE - AUTHORIZED + 3_000),
            -refunds * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    app.close().await;
    Ok(())
}

/// Ends the session that waits for an advisory lock. Pass the test's own pool: only
/// a superuser may signal another session.
async fn kill_the_waiter(pool: &PgPool) -> sqlx::Result<()> {
    wait_for_lock_wait(pool, "advisory").await?;
    sqlx::query_scalar!(
        r#"SELECT pg_terminate_backend(pid) AS "sent!" FROM pg_stat_activity
            WHERE datname = current_database()
              AND wait_event_type = 'Lock' AND wait_event = 'advisory'"#
    )
    .fetch_all(pool)
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_command_killed_while_it_waits_for_the_lock_can_be_sent_again(
    pool: PgPool,
) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let payment = authorized(&app, merchant, 1).await?;
    let command = capture_command(1, payment);
    let mut holder = runtime_transaction(&pool).await?;
    take_lock(&mut holder, payment).await?;
    let before = stored(&pool).await?;
    let (answer, killed) = join(capture(&app, &command), kill_the_waiter(&pool)).await;
    killed?;
    let err = answer.unwrap_err();
    assert!(matches!(err, CaptureError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(stored(&pool).await?, before);
    holder.rollback().await?;
    assert!(matches!(
        capture(&app, &command).await?,
        Outcome::Applied(_)
    ));
    app.close().await;
    Ok(())
}

/// How many times the last test tries to land a kill inside a commit.
const ATTEMPTS: u32 = 20;

/// A one-connection pool whose session sleeps 100 ms before each commit's WAL flush,
/// to widen the window, and logs every statement's duration, because PostgreSQL
/// services the held signal while it writes that line, after `CommandComplete`. It
/// then acts as the runtime role. These settings are for superusers only, so they
/// come before `SET ROLE`, and they last for the session.
async fn slow_commit_pool(pool: &PgPool) -> sqlx::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
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
                sqlx::query!("SET ROLE payments_app")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
}

/// Waits until the backend runs `COMMIT`, then ends its session. Gives up after a
/// second, when the command has failed or ended before its `COMMIT`.
async fn kill_during_commit(pool: &PgPool, pid: i32) -> sqlx::Result<()> {
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

#[sqlx::test]
async fn a_commit_that_commits_and_loses_its_session_is_settled_by_sending_again(
    pool: PgPool,
) -> TestResult {
    // The hardest point: PostgreSQL holds a terminate that arrives while it commits
    // until the commit is recorded, so the client loses its session for a transaction
    // that committed. It cannot tell, and need not: the identical command finds the
    // step. Timing decides where each kill lands, so the test tries several times and
    // needs one hit, and every attempt, hit or miss, must end with the money moved
    // exactly once (ADR-0014).
    let app = slow_commit_pool(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    let reply = Recorded {
        seq: 2,
        from: PaymentStatus::Authorized,
        to: PaymentStatus::Captured,
        refundable: AUTHORIZED,
    };
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let payment = authorized(&app, merchant, n).await?;
        let command = capture_command(n, payment);
        // The last kill may have ended the session, and a new one has a new ID.
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let (answer, killed) = join(capture(&app, &command), kill_during_commit(&pool, pid)).await;
        killed?;
        let committed = status_of(&app, payment).await? == PaymentStatus::Captured;
        match answer {
            Ok(outcome) => {
                assert_eq!(outcome, Outcome::Applied(reply), "attempt {n}");
                assert!(committed, "attempt {n}: Applied, not stored");
            }
            // The one answer that may go either way, and sending again settles it.
            Err(CaptureError::NotConfirmed { .. }) => {
                let settled = capture(&app, &command).await?;
                if committed {
                    assert_eq!(settled, Outcome::AlreadyApplied(reply), "attempt {n}");
                    hits += 1;
                } else {
                    assert_eq!(settled, Outcome::Applied(reply), "attempt {n}");
                }
            }
            other => panic!("attempt {n}: {other:?}"),
        }
    }
    assert!(
        hits > 0,
        "no kill landed inside a commit in {ATTEMPTS} attempts"
    );
    let attempts = i64::from(ATTEMPTS);
    assert_eq!(
        balances(&app, merchant).await?,
        [
            attempts * AUTHORIZED,
            attempts * (FEE - AUTHORIZED),
            -attempts * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    app.close().await;
    Ok(())
}
