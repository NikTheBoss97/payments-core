// Author: Nicola Antonello

//! The backend killed around each statement of a command under a key (ADR-0026,
//! invariant I4), as a merchant's requests run it.
//!
//! A retry has fresh IDs, so nothing but the key's record can tell it that a first
//! attempt committed. So at every point the key must still be unused if the effect
//! is not there, and hold the answer if it is: one row and one `COMMIT` decide both.
//! Each test also sends a late duplicate, with fresh IDs again, which the key alone
//! must stop. The effect is a payment created, or a request to the PSP (ADR-0028),
//! which moves no money: only the PSP's word, applied by a catch-up, does (ADR-0031).

mod common;

use std::error::Error;

use common::crashes::{
    AT_COMMIT, ATTEMPTS, REQUEST_AT_COMMIT, REQUEST_POINTS, Role, STEP_POINTS,
    assert_books_are_whole, balances, install_crash_points, kill_during_commit, plan_death,
    slow_commit_pool, stored,
};
use common::keys::{answered, key, print, render};
use common::payments::{AUTHORIZED, REQUESTED, create_command, fee_rate, requests_of};
use common::psp::PspSide;
use common::{
    merchant_id, request_id, runtime_pool, tenant_pool_of_one, tenant_transaction,
    wait_for_lock_wait,
};
use futures_util::future::join;
use payments_core::idempotency::{Recall, Seal, recall};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{CreateError, Outcome, create_payment, register_merchant};
use payments_core::requests::{
    Ask, RecordRequest, RequestError, RequestOutcome, RequestRecorded, record_request,
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

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// Request `n`, which asks this of this payment.
fn request(n: u32, payment: PaymentId, ask: Ask) -> RecordRequest {
    RecordRequest {
        request: request_id(n),
        payment,
        ask,
    }
}

/// What a death must leave as it was: the rows of each table that a command writes in
/// its transaction, and the requests to the PSP.
async fn rows(pool: &PgPool) -> sqlx::Result<([i64; 8], i64)> {
    let requests = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payment_requests"#)
        .fetch_one(pool)
        .await?;
    Ok((stored(pool).await?, requests))
}

/// Where the PSP takes a payment before the request under test.
#[derive(Debug, Clone, Copy)]
enum Start {
    Created,
    Authorized,
    Captured,
}

/// Kills `ask`, under a key of its own, at each point of a request: around its own row
/// and the key's, and in its commit, each time on a payment of its own that the PSP first
/// takes to `start`. Each death leaves every row, the money and the key as they were; a
/// retry with fresh IDs records the request and stores its answer; and a late duplicate,
/// fresh again, meets the key. Each payment ends with one request more, and the money
/// where its start left it.
async fn killed_anywhere(pool: &PgPool, start: Start, name: &str, ask: Ask) -> TestResult {
    let (app, merchant) = crash_test(pool).await?;
    let me = Tenant::new(&app, merchant);
    let mut psp = PspSide::start(pool).await?;
    let points = REQUEST_POINTS
        .into_iter()
        .chain(KEY_POINTS)
        .chain([REQUEST_AT_COMMIT]);
    for (n, point) in (1_u32..).zip(points) {
        let payment = created(me, merchant, n).await?;
        if matches!(start, Start::Authorized | Start::Captured) {
            psp.authorized(me, payment).await;
        }
        if matches!(start, Start::Captured) {
            psp.captured(me, payment).await;
        }
        let k = key(&format!("{name}-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let before = rows(pool).await?;
        let money = balances(&app, merchant).await?;
        let asked = requests_of(pool, payment).await?;
        plan_death(&app, point).await?;
        let err = record_request(me.sealed(&seal), &request(n * 10, payment, ask))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RequestError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(rows(pool).await?, before, "{point}");
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        let retry = request(n * 10 + 1, payment, ask);
        assert_eq!(
            record_request(me.sealed(&seal), &retry).await?,
            RequestOutcome::Recorded(RequestRecorded {
                request: retry.request.get(),
                payment,
            }),
            "{point}"
        );
        assert_eq!(
            recall(me, &k, print(n)).await?,
            answered(202, &format!(r#"{{"request":"{}"}}"#, retry.request.get())),
            "{point}"
        );
        let err = record_request(me.sealed(&seal), &request(n * 10 + 2, payment, ask))
            .await
            .unwrap_err();
        assert!(matches!(err, RequestError::KeyTaken), "{point}: {err:?}");
        assert_eq!(requests_of(pool, payment).await?, asked + 1, "{point}");
        assert_eq!(balances(&app, merchant).await?, money, "{point}");
    }
    assert_books_are_whole(pool).await?;
    psp.close().await;
    app.close().await;
    Ok(())
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
    killed_anywhere(
        &pool,
        Start::Created,
        "authorize",
        Ask::Authorize(amount(REQUESTED)),
    )
    .await
}

#[sqlx::test]
async fn a_keyed_void_killed_anywhere_leaves_no_answer_and_settles(pool: PgPool) -> TestResult {
    killed_anywhere(&pool, Start::Authorized, "void", Ask::Void).await
}

#[sqlx::test]
async fn a_keyed_capture_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    // I4 and I6 together: at no point can the request exist without the answer, or the
    // answer without the request, and neither moves the money.
    killed_anywhere(
        &pool,
        Start::Authorized,
        "capture",
        Ask::Capture(amount(AUTHORIZED)),
    )
    .await
}

#[sqlx::test]
async fn a_keyed_refund_killed_anywhere_moves_no_money_and_settles(pool: PgPool) -> TestResult {
    // The sharpest case: a partial refund still fits after it is recorded, so a late
    // duplicate with fresh IDs would record a second if the key did not stop it.
    killed_anywhere(&pool, Start::Captured, "refund", Ask::Refund(amount(3_000))).await
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
        let capture = |m| request(m, payment, Ask::Capture(amount(AUTHORIZED)));
        plan_death(&app, point).await?;
        let err = record_request(me.sealed(&seal), &capture(n * 10))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RequestError::NotConfirmed { .. }),
            "{point}: {err:?}"
        );
        assert_eq!(recall(me, &k, print(n)).await?, Recall::Unused, "{point}");

        let err = record_request(me.sealed(&seal), &capture(n * 10 + 1))
            .await
            .unwrap_err();
        assert!(matches!(err, RequestError::Refused(_)), "{point}: {err:?}");
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
    // attempt, hit or miss, must end with exactly one request.
    let merchant = merchant_id(1);
    let system = runtime_pool(&pool).await?;
    register_merchant(&system, merchant, fee_rate()).await?;
    system.close().await;
    let app = slow_commit_pool(&pool, Role::Tenant).await?;
    let me = Tenant::new(&app, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let mut hits = 0;
    for n in 1..=ATTEMPTS {
        let payment = created(me, merchant, n).await?;
        psp.authorized(me, payment).await;
        let k = key(&format!("capture-{n}"));
        let seal = Seal {
            key: &k,
            fingerprint: print(n),
            answer: &render,
        };
        let capture = |m| request(m, payment, Ask::Capture(amount(AUTHORIZED)));
        // The last kill may have ended the session, and a new one has a new ID.
        let pid = sqlx::query_scalar!(r#"SELECT pg_backend_pid() AS "pid!""#)
            .fetch_one(&app)
            .await?;
        let attempt = capture(n * 10);
        let (answer, killed) = join(
            record_request(me.sealed(&seal), &attempt),
            kill_during_commit(&pool, pid),
        )
        .await;
        killed?;
        match answer {
            Ok(outcome) => assert!(
                matches!(outcome, RequestOutcome::Recorded(_)),
                "attempt {n}"
            ),
            Err(RequestError::NotConfirmed { .. }) => match recall(me, &k, print(n)).await? {
                Recall::Answered(_) => hits += 1,
                Recall::Unused => {
                    record_request(me.sealed(&seal), &capture(n * 10 + 1)).await?;
                }
                Recall::Reused => panic!("attempt {n}: the same request reads as reused"),
            },
            Err(other) => panic!("attempt {n}: {other:?}"),
        }
        let Recall::Answered(answer) = recall(me, &k, print(n)).await? else {
            panic!("attempt {n}: no answer stored");
        };
        assert_eq!(answer.status, 202, "attempt {n}");
        // The authorization's request, and one capture's.
        assert_eq!(requests_of(&pool, payment).await?, 2, "attempt {n}");
    }
    assert!(
        hits > 0,
        "no kill landed inside a commit in {ATTEMPTS} attempts"
    );
    // The requests moved no money: only the PSP's history does (ADR-0031).
    assert_eq!(balances(&app, merchant).await?, [0, 0, 0]);
    assert_books_are_whole(&pool).await?;
    psp.close().await;
    app.close().await;
    Ok(())
}
