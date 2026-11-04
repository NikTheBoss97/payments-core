// Author: Nicola Antonello

//! The backend killed around each statement of a command under a key (ADR-0026,
//! invariant I4), as a merchant's requests run it.
//!
//! A retry has fresh IDs, so nothing but the key's record can tell it that a first
//! attempt committed. So at every point the key must still be unused if the effect
//! is not there, and hold the answer if it is: one row and one `COMMIT` decide both.
//! Each test also sends a late duplicate, with fresh IDs again, which the key alone
//! must stop.

mod common;

use std::error::Error;

use common::crashes::{
    AT_COMMIT, ATTEMPTS, POSTING_POINTS, Role, STEP_POINTS, assert_books_are_whole, balances,
    install_crash_points, kill_during_commit, plan_death, slow_commit_pool, stored,
};
use common::keys::{answered, key, print, render};
use common::payments::{
    AUTHORIZED, FEE, authorize_command, capture_command, create_command, fee_rate, refund_command,
};
use common::{
    merchant_id, runtime_pool, tenant_pool_of_one, tenant_transaction, transition_id,
    wait_for_lock_wait,
};
use futures_util::future::join;
use payments_core::idempotency::{Recall, Seal, recall};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::payments::{
    AuthorizeError, CaptureError, CreateError, Outcome, RefundError, StepCommand, StepError,
    authorize, capture, create_payment, refund, register_merchant, void,
};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// The points around the key's own row.
const KEY_POINTS: [&str; 2] = [
    "die_before idempotency_records",
    "die_after idempotency_records",
];

/// A pool of one session as the tenant role, a registered merchant, and the crash
/// points.
async fn crash_test(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let merchant = merchant_id(1);
    let system = runtime_pool(pool).await?;
    register_merchant(&system, merchant, fee_rate()).await?;
    system.close().await;
    install_crash_points(pool).await?;
    Ok((tenant_pool_of_one(pool).await?, merchant))
}

async fn created(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    Ok(command.id)
}

async fn authorized(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(me, merchant, n).await?;
    authorize(me, &authorize_command(n, payment)).await?;
    Ok(payment)
}

#[sqlx::test]
async fn a_keyed_create_killed_anywhere_leaves_no_answer_and_settles(pool: PgPool) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    let points = ["die_before payments", "die_after payments"]
        .into_iter()
        .chain(STEP_POINTS)
        .chain(KEY_POINTS)
        .chain([AT_COMMIT]);
    let mut creates = 0;
    for (n, point) in (1_u32..).zip(points) {
        let k = key(&format!("create-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let before = stored(&pool).await?;
        plan_death(&app, point).await?;
        let err = create_payment(me.sealed(&seal), &create_command(n * 10, merchant))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CreateError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        create_payment(me.sealed(&seal), &create_command(n * 10 + 1, merchant)).await?;
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(200, r#"{"seq":0,"refundable":0}"#),
            "{point}"
        );
        creates += 1;

        // A late duplicate would create a second payment under its own fresh ID.
        let err = create_payment(me.sealed(&seal), &create_command(n * 10 + 2, merchant))
            .await
            .unwrap_err();
        assert!(matches!(err, CreateError::KeyTaken), "{point}: {err:?}");
    }
    let payments = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(payments, creates);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_keyed_authorization_killed_anywhere_leaves_no_answer_and_settles(
    pool: PgPool,
) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    let points = STEP_POINTS
        .into_iter()
        .chain([
            "die_before payment_authorizations",
            "die_after payment_authorizations",
        ])
        .chain(KEY_POINTS)
        .chain([AT_COMMIT]);
    for (n, point) in (1_u32..).zip(points) {
        let payment = created(me, merchant, n).await?;
        let k = key(&format!("authorize-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let before = stored(&pool).await?;
        plan_death(&app, point).await?;
        let err = authorize(me.sealed(&seal), &authorize_command(n * 10, payment))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthorizeError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        authorize(me.sealed(&seal), &authorize_command(n * 10 + 1, payment)).await?;
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(200, r#"{"seq":1,"refundable":0}"#),
            "{point}"
        );
        let err = authorize(me.sealed(&seal), &authorize_command(n * 10 + 2, payment))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthorizeError::KeyTaken), "{point}: {err:?}");
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_keyed_void_killed_anywhere_leaves_no_answer_and_settles(pool: PgPool) -> TestResult {
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    let points = STEP_POINTS.into_iter().chain(KEY_POINTS).chain([AT_COMMIT]);
    for (n, point) in (1_u32..).zip(points) {
        let payment = authorized(me, merchant, n).await?;
        let k = key(&format!("void-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let attempt = |step| StepCommand {
            step: transition_id(5_000 + step),
            payment,
        };
        let before = stored(&pool).await?;
        plan_death(&app, point).await?;
        let err = void(me.sealed(&seal), &attempt(n * 10)).await.unwrap_err();
        assert!(
            matches!(err, StepError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        void(me.sealed(&seal), &attempt(n * 10 + 1)).await?;
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(200, r#"{"seq":2,"refundable":0}"#),
            "{point}"
        );
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_keyed_capture_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    // I4 and I6 together: at no point can the money move without the answer, or the
    // answer exist without the money having moved.
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_captures", "die_after payment_captures"])
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    let mut captures = 0;
    for (n, point) in (1_u32..).zip(points) {
        let payment = authorized(me, merchant, n).await?;
        let k = key(&format!("capture-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let before = stored(&pool).await?;
        let money = balances(&app, merchant).await?;
        plan_death(&app, point).await?;
        let err = capture(me.sealed(&seal), &capture_command(n * 10, payment))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaptureError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(balances(&app, merchant).await?, money, "{point}");
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        capture(me.sealed(&seal), &capture_command(n * 10 + 1, payment)).await?;
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(200, r#"{"seq":2,"refundable":8000}"#),
            "{point}"
        );
        captures += 1;
    }
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
async fn a_keyed_refund_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    // The sharpest case: a partial refund still fits after it applies, so a late
    // duplicate with fresh IDs would move the money twice if the key did not stop it.
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    let points: Vec<&str> = STEP_POINTS
        .into_iter()
        .chain(POSTING_POINTS)
        .chain(["die_before payment_refunds", "die_after payment_refunds"])
        .chain(KEY_POINTS)
        .chain([AT_COMMIT])
        .collect();
    let mut refunds = 0;
    for (n, point) in (1_u32..).zip(points) {
        let payment = authorized(me, merchant, n).await?;
        capture(me, &capture_command(n, payment)).await?;
        let k = key(&format!("refund-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let before = stored(&pool).await?;
        let money = balances(&app, merchant).await?;
        plan_death(&app, point).await?;
        let err = refund(me.sealed(&seal), &refund_command(n * 10, payment, 3_000))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RefundError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(stored(&pool).await?, before, "{point}");
        assert_eq!(balances(&app, merchant).await?, money, "{point}");
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        refund(
            me.sealed(&seal),
            &refund_command(n * 10 + 1, payment, 3_000),
        )
        .await?;
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(200, r#"{"seq":3,"refundable":5000}"#),
            "{point}"
        );
        refunds += 1;

        let err = refund(
            me.sealed(&seal),
            &refund_command(n * 10 + 2, payment, 3_000),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RefundError::KeyTaken), "{point}: {err:?}");
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

#[sqlx::test]
async fn a_refusal_killed_as_it_is_stored_is_decided_again(pool: PgPool) -> TestResult {
    // A client that was never answered has been told nothing, so deciding again is
    // right, and it is only the stored refusal that is final.
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    for (n, point) in (1_u32..).zip(KEY_POINTS) {
        let payment = created(me, merchant, n).await?;
        let k = key(&format!("refused-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        plan_death(&app, point).await?;
        let err = capture(me.sealed(&seal), &capture_command(n * 10, payment))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaptureError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        let err = capture(me.sealed(&seal), &capture_command(n * 10 + 1, payment))
            .await
            .unwrap_err();
        assert!(matches!(err, CaptureError::Refused(_)), "{point}: {err:?}");
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(409, r#"{"type":"illegal"}"#),
            "{point}"
        );
    }
    app.close().await;
    Ok(())
}

/// Ends the session that waits on another transaction. Pass the test's own pool: only
/// a superuser may signal another session.
async fn kill_the_waiter(pool: &PgPool) -> sqlx::Result<()> {
    wait_for_lock_wait(pool, "transactionid").await?;
    sqlx::query_scalar!(
        r#"SELECT pg_terminate_backend(pid) AS "sent!" FROM pg_stat_activity
            WHERE datname = current_database()
              AND wait_event_type = 'Lock' AND wait_event = 'transactionid'"#
    )
    .fetch_all(pool)
    .await?;
    Ok(())
}

#[sqlx::test]
async fn an_attempt_killed_while_it_waits_for_a_key_can_be_sent_again(pool: PgPool) -> TestResult {
    // A create has no lock to queue on, so a second attempt waits in the key's unique
    // index, behind a first that has not committed.
    let (app, merchant) = crash_test(&pool).await?;
    let me = Tenant::new(&app, merchant);
    let k = key("create");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let digest = print(1);
    let mut first = tenant_transaction(&pool, merchant.get()).await?;
    sqlx::query!(
        "INSERT INTO idempotency_records (merchant_id, key, fingerprint, status, body) \
         VALUES ($1, 'create', $2, 200, '{}')",
        merchant.get(),
        digest.as_bytes()
    )
    .execute(&mut *first)
    .await?;

    let command = create_command(10, merchant);
    let (answer, killed) = join(
        create_payment(me.sealed(&seal), &command),
        kill_the_waiter(&pool),
    )
    .await;
    killed?;
    let err = answer.unwrap_err();
    assert!(matches!(err, CreateError::NotConfirmed { .. }), "{err:?}");

    first.rollback().await?;
    assert_eq!(recall(me, &k, print(1)).await?, Recall::Unused);
    let outcome = create_payment(me.sealed(&seal), &create_command(11, merchant)).await?;
    assert!(matches!(outcome, Outcome::Applied(_)), "{outcome:?}");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_commit_that_commits_and_loses_its_session_is_settled_by_the_key(
    pool: PgPool,
) -> TestResult {
    // The hardest point, and the reason the answer shares the effect's transaction. The
    // client loses its session for a transaction that committed, cannot tell, and its
    // retry has fresh IDs. Only the record says what happened: an answer means that it
    // committed, so nothing runs, and none means that it did not, so the retry does. Every
    // attempt, hit or miss, must end with the money moved exactly once.
    let merchant = merchant_id(1);
    let system = runtime_pool(&pool).await?;
    register_merchant(&system, merchant, fee_rate()).await?;
    system.close().await;
    let app = slow_commit_pool(&pool, Role::Tenant).await?;
    let me = Tenant::new(&app, merchant);
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let payment = authorized(me, merchant, n).await?;
        let k = key(&format!("capture-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        // The last kill may have ended the session, and a new one has a new ID.
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let attempt = capture_command(n * 10, payment);
        let (answer, killed) = join(
            capture(me.sealed(&seal), &attempt),
            kill_during_commit(&pool, pid),
        )
        .await;
        killed?;
        match answer {
            Ok(outcome) => assert!(matches!(outcome, Outcome::Applied(_)), "attempt {n}"),
            Err(CaptureError::NotConfirmed { .. }) => match recall(me, &k, print(n)).await? {
                Recall::Answered(_) => hits += 1,
                Recall::Unused => {
                    capture(me.sealed(&seal), &capture_command(n * 10 + 1, payment)).await?;
                }
                Recall::Reused => panic!("attempt {n}: the same request reads as reused"),
            },
            Err(other) => panic!("attempt {n}: {other:?}"),
        }
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(200, r#"{"seq":2,"refundable":8000}"#),
            "attempt {n}"
        );
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
