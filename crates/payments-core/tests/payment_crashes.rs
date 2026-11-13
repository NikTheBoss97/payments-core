// Author: Nicola Antonello

//! The backend killed around each statement of a create, and of a catch-up as it applies
//! each move of the PSP's (ADR-0014, ADR-0034, ADR-0041, invariant I6): a step, its
//! posting, its record, its fact and its request's decision commit together or not at
//! all, and the identical command, or the next catch-up, settles the outcome.
//!
//! A trigger on each table ends its own session when the session asked for that:
//! `crash.point`, a session setting, names the trigger and its table. Setting it
//! needs the session that runs the command or the catch-up, so these tests run them
//! on a pool of one connection. The setting dies with its session, so the retry, on a
//! new connection, runs clean. A point that is never reached fails its test, because
//! the run then answers something other than `NotConfirmed`.

mod common;

use std::error::Error;

use common::crashes::{
    AT_COMMIT, ATTEMPTS, FACT_POINTS, POSTING_POINTS, Role, STEP_POINTS, assert_books_are_whole,
    balances, install_crash_points, kill_during_commit, plan_death, slow_commit_pool, stored,
};
use common::payments::{
    AUTHORIZED, FEE, REQUESTED, create_command, fee_rate, status_of, take_lock,
};
use common::psp::{PspSide, client_of};
use common::{
    merchant_id, request_id, runtime_pool_of_one, runtime_transaction, tenant_pool,
    wait_for_lock_wait,
};
use futures_util::future::join;
use payments_core::catch_up::{CatchUpError, CaughtUp, catch_up};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::PaymentStatus;
use payments_core::money::PositiveAmount;
use payments_core::payments::{CreateError, Outcome, Recorded, create_payment, register_merchant};
use payments_core::psp::PspClient;
use payments_core::requests::{Ask, RecordRequest};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// A pool of one session, a merchant, and the crash points.
async fn crash_test(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let app = runtime_pool_of_one(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    install_crash_points(pool).await?;
    Ok((app, merchant))
}

/// What a test of the PSP's moves runs on: the PSP's side, a merchant on a pool of the
/// tenant role, and a pool of one session of the system role, on which the catch-up
/// runs and dies.
struct CatchUpTest {
    psp: PspSide,
    tenant: PgPool,
    merchant: MerchantId,
    app: PgPool,
    client: PspClient,
}

impl CatchUpTest {
    async fn start(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let psp = PspSide::start(pool).await?;
        let merchant = merchant_id(1);
        register_merchant(&psp.system, merchant, fee_rate()).await?;
        install_crash_points(pool).await?;
        Ok(Self {
            client: client_of(&psp.fake),
            tenant: tenant_pool(pool).await?,
            app: runtime_pool_of_one(pool).await?,
            psp,
            merchant,
        })
    }

    fn me(&self) -> Tenant<'_> {
        Tenant::new(&self.tenant, self.merchant)
    }

    /// Creates payment `n` as the merchant, and has the PSP authorize it for
    /// [`AUTHORIZED`], caught up on another session.
    async fn authorized(&mut self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let command = create_command(n, self.merchant);
        let me = Tenant::new(&self.tenant, self.merchant);
        create_payment(me, &command).await?;
        self.psp.authorized(me, command.id).await;
        Ok(command.id)
    }

    /// Asks for request `n` as the merchant, and the PSP approves it whole. The catch-up
    /// that applies it is the test's.
    async fn approved(&mut self, n: u32, payment: PaymentId, ask: Ask) {
        let command = RecordRequest {
            request: request_id(n),
            payment,
            ask,
        };
        let me = Tenant::new(&self.tenant, self.merchant);
        self.psp.sent(me, &command).await;
        self.psp.fake.approve(command.request.get());
    }

    /// Catches `payment` up on the session that may die.
    async fn catch_up(&self, payment: PaymentId) -> Result<CaughtUp, CatchUpError> {
        catch_up(&self.app, &self.client, payment).await
    }

    async fn close(self) {
        self.tenant.close().await;
        self.app.close().await;
        self.psp.close().await;
    }
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("valid amount")
}

/// What [`stored`] counts, and the rows that a catch-up writes with each fact: the facts,
/// the links of their steps, and the decisions of requests. A catch-up that died must
/// leave these as they were.
async fn rows(pool: &PgPool) -> Result<([i64; 8], [i64; 3]), Box<dyn Error>> {
    let facts = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM payment_facts) AS "facts!",
                  (SELECT count(*) FROM payment_transition_facts) AS "links!",
                  (SELECT count(*) FROM payment_request_decisions) AS "decisions!""#
    )
    .fetch_one(pool)
    .await?;
    Ok((
        stored(pool).await?,
        [facts.facts, facts.links, facts.decisions],
    ))
}

/// Kills the catch-up of `payment` at `point`, checks that none of it stayed, that no
/// money moved and that the payment still stands at `status`, then catches it up twice:
/// the first applies the fact, the second finds it held.
async fn killed_then_caught_up(
    pool: &PgPool,
    test: &CatchUpTest,
    payment: PaymentId,
    status: PaymentStatus,
    point: &str,
) -> TestResult {
    let before = rows(pool).await?;
    let money = balances(&test.psp.system, test.merchant).await?;
    plan_death(&test.app, point).await?;
    let err = test.catch_up(payment).await.unwrap_err();
    assert!(
        matches!(err, CatchUpError::NotConfirmed { .. }),
        "{point}: {err:?}"
    );
    assert_eq!(rows(pool).await?, before, "{point}");
    assert_eq!(
        balances(&test.psp.system, test.merchant).await?,
        money,
        "{point}"
    );
    assert_eq!(
        status_of(&test.psp.system, payment).await?,
        status,
        "{point}"
    );
    assert_eq!(
        test.catch_up(payment).await?,
        CaughtUp::Current { applied: 1 },
        "{point}"
    );
    assert_eq!(
        test.catch_up(payment).await?,
        CaughtUp::Current { applied: 0 },
        "{point}"
    );
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
    let mut test = CatchUpTest::start(&pool).await?;
    let points = STEP_POINTS
        .into_iter()
        .chain([
            "die_before payment_authorizations",
            "die_after payment_authorizations",
        ])
        .chain(FACT_POINTS)
        .chain([AT_COMMIT]);
    for (n, point) in (1..).zip(points) {
        let command = create_command(n, test.merchant);
        create_payment(test.me(), &command).await?;
        test.approved(n, command.id, Ask::Authorize(amount(REQUESTED)))
            .await;
        killed_then_caught_up(&pool, &test, command.id, PaymentStatus::Created, point).await?;
        assert_eq!(
            status_of(&test.psp.system, command.id).await?,
            PaymentStatus::Authorized
        );
    }
    test.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_void_killed_anywhere_stores_nothing_and_settles(pool: PgPool) -> TestResult {
    let mut test = CatchUpTest::start(&pool).await?;
    let points = STEP_POINTS
        .into_iter()
        .chain(FACT_POINTS)
        .chain([AT_COMMIT]);
    for (n, point) in (1..).zip(points) {
        let payment = test.authorized(n).await?;
        test.approved(n, payment, Ask::Void).await;
        killed_then_caught_up(&pool, &test, payment, PaymentStatus::Authorized, point).await?;
        assert_eq!(
            status_of(&test.psp.system, payment).await?,
            PaymentStatus::Voided
        );
    }
    test.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_capture_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    // The heart of invariant I6: at no point can a step exist without its posting
    // and its record, or money move without them.
    let mut test = CatchUpTest::start(&pool).await?;
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_captures", "die_after payment_captures"])
        .chain(FACT_POINTS)
        .chain([AT_COMMIT])
        .collect();
    let mut captures = 0;
    for (n, point) in (1..).zip(points) {
        let payment = test.authorized(n).await?;
        test.approved(n, payment, Ask::Capture(amount(AUTHORIZED)))
            .await;
        killed_then_caught_up(&pool, &test, payment, PaymentStatus::Authorized, point).await?;
        assert_eq!(
            status_of(&test.psp.system, payment).await?,
            PaymentStatus::Captured
        );
        captures += 1;
    }
    // Each capture moved its money once, for all the deaths and retries.
    assert_eq!(
        balances(&test.psp.system, test.merchant).await?,
        [
            captures * AUTHORIZED,
            captures * (FEE - AUTHORIZED),
            -captures * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    test.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refund_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    let mut test = CatchUpTest::start(&pool).await?;
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_refunds", "die_after payment_refunds"])
        .chain(FACT_POINTS)
        .chain([AT_COMMIT])
        .collect();
    let mut refunds = 0;
    for (n, point) in (1..).zip(points) {
        let payment = test.authorized(n).await?;
        let me = Tenant::new(&test.tenant, test.merchant);
        test.psp.captured(me, payment).await;
        test.approved(n, payment, Ask::Refund(amount(3_000))).await;
        killed_then_caught_up(&pool, &test, payment, PaymentStatus::Captured, point).await?;
        refunds += 1;
    }
    assert_eq!(
        balances(&test.psp.system, test.merchant).await?,
        [
            refunds * (AUTHORIZED - 3_000),
            refunds * (FEE - AUTHORIZED + 3_000),
            -refunds * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    test.close().await;
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
async fn a_catch_up_killed_while_it_waits_for_the_lock_can_run_again(pool: PgPool) -> TestResult {
    let mut test = CatchUpTest::start(&pool).await?;
    let payment = test.authorized(1).await?;
    test.approved(1, payment, Ask::Capture(amount(AUTHORIZED)))
        .await;
    let mut holder = runtime_transaction(&pool).await?;
    take_lock(&mut holder, payment).await?;
    let before = rows(&pool).await?;
    let (answer, killed) = join(test.catch_up(payment), kill_the_waiter(&pool)).await;
    killed?;
    let err = answer.unwrap_err();
    assert!(matches!(err, CatchUpError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(rows(&pool).await?, before);
    holder.rollback().await?;
    assert_eq!(
        test.catch_up(payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    test.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_commit_that_commits_and_loses_its_session_is_settled_by_catching_up_again(
    pool: PgPool,
) -> TestResult {
    // The hardest point: PostgreSQL holds a terminate that arrives while it commits
    // until the commit is recorded, so the catch-up loses its session for a transaction
    // that committed. It cannot tell, and need not: the next catch-up finds the fact
    // held (ADR-0039). Timing decides where each kill lands, so the test tries several
    // times and needs one hit, and every attempt, hit or miss, must end with the money
    // moved exactly once (ADR-0014).
    let mut test = CatchUpTest::start(&pool).await?;
    let app = slow_commit_pool(&pool, Role::System).await?;
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let payment = test.authorized(n).await?;
        test.approved(n, payment, Ask::Capture(amount(AUTHORIZED)))
            .await;
        // The last kill may have ended the session, and a new one has a new ID.
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let (answer, killed) = join(
            catch_up(&app, &test.client, payment),
            kill_during_commit(&pool, pid),
        )
        .await;
        killed?;
        let committed = status_of(&test.psp.system, payment).await? == PaymentStatus::Captured;
        match answer {
            Ok(caught_up) => {
                assert_eq!(caught_up, CaughtUp::Current { applied: 1 }, "attempt {n}");
                assert!(committed, "attempt {n}: applied, not stored");
            }
            // The one answer that may go either way, and catching up again settles it.
            Err(CatchUpError::NotConfirmed { .. }) => {
                let settled = catch_up(&app, &test.client, payment).await?;
                if committed {
                    assert_eq!(settled, CaughtUp::Current { applied: 0 }, "attempt {n}");
                    hits += 1;
                } else {
                    assert_eq!(settled, CaughtUp::Current { applied: 1 }, "attempt {n}");
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
        balances(&test.psp.system, test.merchant).await?,
        [
            attempts * AUTHORIZED,
            attempts * (FEE - AUTHORIZED),
            -attempts * FEE
        ]
    );
    assert_books_are_whole(&pool).await?;
    app.close().await;
    test.close().await;
    Ok(())
}
