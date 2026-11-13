// Author: Nicola Antonello

//! The moves that carry no amount (ADR-0016, ADR-0019): the merchant's cancel, and the
//! PSP's decline, void, lapse and settlement, which reach a payment as facts through a
//! catch-up (ADR-0031, ADR-0041).
//!
//! Every command shares the cancel's path, so this is also where that path is tried: the
//! payment's lock (ADR-0018), identity before state, and what each failure proves. Those
//! tests reach each status through raw SQL that writes only the merchant's steps, and a
//! test that needs a "first attempt" writes it the same way, under the same lock. Faults
//! that the types rule out are injected with triggers that the owner adds to the test's
//! own database.

mod common;

use std::error::Error;
use std::fs;
use std::path::Path;

use common::payments::{
    AUTHORIZED, REQUESTED, Step, books, committed_at, create_command, fee_rate, status_of, steps,
    take_lock,
};
use common::psp::{PspSide, client_of};
use common::{
    CHECK_VIOLATION, UNIQUE_VIOLATION, assert_guard, assert_violation, merchant_id, request_id,
    runtime_pool, runtime_pool_of_one, runtime_pool_with_lock_timeout, runtime_transaction,
    sqlstate, tenant_pool, test_uuid, transition_id, wait_for_lock_wait,
};
use futures_util::future::{join, join_all};
use mock_psp::wire::Detail;
use payments_core::catch_up::{CaughtUp, Stop, StopReason, catch_up};
use payments_core::db::commit;
use payments_core::ids::{MerchantId, PaymentId, TransitionId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Transition, next};
use payments_core::money::PositiveAmount;
use payments_core::payments::{
    Outcome, Recorded, StepCommand, StepError, cancel, create_payment, find_payment, lock_key,
    register_merchant,
};
use payments_core::requests::{Ask, RecordRequest};
use payments_core::tenancy::Tenant;
use sqlx::{PgPool, Postgres, Transaction};

type TestResult = Result<(), Box<dyn Error>>;

/// What cancelling a payment that stands at its first step records.
const CANCELLED: Recorded = Recorded {
    seq: 1,
    from: PaymentStatus::Created,
    to: PaymentStatus::Voided,
    refundable: 0,
};

/// A command with step ID `n` on the payment that `step` belongs to.
fn command_on(n: u32, step: &Step) -> StepCommand {
    StepCommand {
        step: transition_id(n),
        payment: PaymentId::try_from(step.payment_id).expect("a UUIDv7"),
    }
}

/// A first attempt of `command`, uncommitted: under the lock, with its step
/// written as the library writes it.
async fn first_attempt_of_cancel(
    pool: &PgPool,
    first: &Step,
    command: &StepCommand,
) -> Result<Transaction<'static, Postgres>, Box<dyn Error>> {
    let mut attempt = runtime_transaction(pool).await?;
    take_lock(&mut attempt, command.payment).await?;
    let stored = Step {
        id: command.step.get(),
        ..first.then("cancel", "voided", 0, 0)
    };
    stored.insert(&mut attempt).await?;
    Ok(attempt)
}

/// A merchant, registered by the system role, and a pool of the tenant role, on which it
/// asks.
async fn registered(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let system = runtime_pool(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&system, merchant, fee_rate()).await?;
    system.close().await;
    Ok((tenant_pool(pool).await?, merchant))
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("valid amount")
}

/// Request `n`: the authorization of all of this payment.
fn authorization(n: u32, payment: PaymentId) -> RecordRequest {
    RecordRequest {
        request: request_id(n),
        payment,
        ask: Ask::Authorize(amount(REQUESTED)),
    }
}

/// Creates payment `n` as its merchant and takes it to `status` as the server would: the
/// merchant's own steps, and the PSP's word through the fake.
async fn at(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
    status: PaymentStatus,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    let payment = command.id;
    match status {
        PaymentStatus::Created => {}
        PaymentStatus::Failed => {
            let asked = authorization(n, payment);
            psp.sent(me, &asked).await;
            psp.fake.decline(asked.request.get());
            psp.caught_up(payment).await;
        }
        PaymentStatus::Voided => {
            let cancelling = StepCommand {
                step: transition_id(n + 1000),
                payment,
            };
            cancel(me, &cancelling).await?;
        }
        PaymentStatus::Authorized
        | PaymentStatus::Captured
        | PaymentStatus::Settled
        | PaymentStatus::Refunded => {
            psp.authorized(me, payment).await;
            if status != PaymentStatus::Authorized {
                psp.captured(me, payment).await;
            }
            if status == PaymentStatus::Settled {
                psp.fake.settle(payment.get());
                psp.caught_up(payment).await;
            }
            if status == PaymentStatus::Refunded {
                psp.applied(me, payment, Ask::Refund(amount(AUTHORIZED)))
                    .await;
            }
        }
    }
    Ok(payment)
}

/// The PSP's lapse or settlement of `payment`: decided by the fake where a PSP could,
/// and appended where none would (ADR-0038).
fn lapse_or_settle(psp: &PspSide, payment: PaymentId, status: PaymentStatus, event: Event) {
    match (event, status) {
        (Event::Expire, PaymentStatus::Authorized) => psp.fake.expire(payment.get()),
        (Event::Expire, _) => psp.fake.append(payment.get(), Detail::Expired),
        // Settled after a full refund too (ADR-0016).
        (Event::Settle, PaymentStatus::Captured | PaymentStatus::Refunded) => {
            psp.fake.settle(payment.get());
        }
        (Event::Settle, _) => psp.fake.append(payment.get(), Detail::Settled),
        _ => unreachable!("only a lapse and a settlement answer no request"),
    }
}

/// Where a payment stands: its newest step's `seq`, and what it leaves refundable.
async fn newest(pool: &PgPool, payment: PaymentId) -> Result<(i32, i64), Box<dyn Error>> {
    let count = steps(pool, payment).await?;
    let stored = find_payment(pool, payment).await?.ok_or("stored")?;
    Ok((i32::try_from(count - 1)?, stored.refundable))
}

/// Checks that `payment` moved from `status` by `step`, after a newest step at `seq`
/// that left `refundable`: a step that moves no money, and the status it leads to.
async fn assert_moved(
    pool: &PgPool,
    payment: PaymentId,
    (seq, refundable): (i32, i64),
    status: PaymentStatus,
    step: Transition,
    context: &str,
) -> TestResult {
    let row = sqlx::query!(
        "SELECT prev_seq, kind, from_status, to_status, amount, prev_refundable, refundable \
           FROM payment_transitions WHERE payment_id = $1 AND seq = $2",
        payment.get(),
        seq + 1
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(row.prev_seq, seq, "{context}");
    assert_eq!(row.kind, step.kind().as_str(), "{context}");
    assert_eq!(row.from_status, status.as_str(), "{context}");
    assert_eq!(row.to_status, step.to().as_str(), "{context}");
    assert_eq!(row.amount, 0, "{context}");
    assert_eq!(
        (row.prev_refundable, row.refundable),
        (refundable, refundable),
        "{context}"
    );
    assert_eq!(status_of(pool, payment).await?, step.to(), "{context}");
    Ok(())
}

#[sqlx::test]
async fn each_move_without_money_follows_the_table(pool: PgPool) -> TestResult {
    // The PSP's lapse and settlement, which answer no request, as facts from all 7
    // statuses, and the merchant's cancel from all 7, against the real tables. A decline
    // or a void answers its request, which the catch-up checks first (ADR-0041), so each
    // is tried where its request can wait; `catching_up.rs` stops one that answers no
    // waiting request. Rust's table is checked on its own in the lifecycle's unit tests.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let client = client_of(&psp.fake);
    let mut n = 0;
    for &status in PaymentStatus::ALL {
        for event in [Event::Expire, Event::Settle] {
            n += 1;
            let payment = at(&mut psp, me, merchant, n, status).await?;
            let before = newest(&psp.system, payment).await?;
            lapse_or_settle(&psp, payment, status, event);
            let caught_up = catch_up(&psp.system, &client, payment).await?;
            let context = format!("{event:?} from {status}");
            match next(status, event) {
                Ok(step) => {
                    assert_eq!(caught_up, CaughtUp::Current { applied: 1 }, "{context}");
                    assert_moved(&pool, payment, before, status, step, &context).await?;
                    // Caught up again, it applies nothing twice.
                    assert_eq!(
                        catch_up(&psp.system, &client, payment).await?,
                        CaughtUp::Current { applied: 0 },
                        "{context}"
                    );
                }
                // A settlement after a full refund is recorded without a step (ADR-0016).
                Err(_) if (status, event) == (PaymentStatus::Refunded, Event::Settle) => {
                    assert_eq!(caught_up, CaughtUp::Current { applied: 1 }, "{context}");
                    assert_eq!(newest(&psp.system, payment).await?, before, "{context}");
                }
                Err(_) => {
                    let history = psp.fake.history(payment.get()).ok_or("a history")?;
                    let stop = Stop {
                        position: i32::try_from(history.facts.len())?,
                        reason: StopReason::NoPlace,
                    };
                    assert_eq!(
                        caught_up,
                        CaughtUp::Stopped { applied: 0, stop },
                        "{context}"
                    );
                    assert_eq!(newest(&psp.system, payment).await?, before, "{context}");
                    assert_eq!(status_of(&psp.system, payment).await?, status, "{context}");
                }
            }
        }
        // The merchant's cancel, as a command.
        n += 1;
        let payment = at(&mut psp, me, merchant, n, status).await?;
        let before = newest(&psp.system, payment).await?;
        let command = StepCommand {
            step: transition_id(n + 2000),
            payment,
        };
        let result = cancel(me, &command).await;
        let context = format!("Cancel from {status}");
        match next(status, Event::Cancel) {
            Ok(step) => {
                let reply = Recorded {
                    seq: before.0 + 1,
                    from: status,
                    to: step.to(),
                    refundable: before.1,
                };
                assert_eq!(result?, Outcome::Applied(reply), "{context}");
                // Its retry finds the step, though the new status refuses the event, so
                // the command must store the kind it compares.
                assert_eq!(
                    cancel(me, &command).await?,
                    Outcome::AlreadyApplied(reply),
                    "{context}"
                );
                assert_moved(&pool, payment, before, status, step, &context).await?;
            }
            Err(illegal) => {
                let err = result.unwrap_err();
                assert!(
                    matches!(err, StepError::Illegal(got) if got == illegal),
                    "{context}: {err:?}"
                );
                assert_eq!(newest(&psp.system, payment).await?, before, "{context}");
                assert_eq!(status_of(&psp.system, payment).await?, status, "{context}");
            }
        }
    }
    // A decline answers the authorization's request, which waits while the payment is
    // created, and a void answers its own, which waits while the payment is authorized.
    n += 1;
    let payment = at(&mut psp, me, merchant, n, PaymentStatus::Created).await?;
    let asked = authorization(n, payment);
    psp.sent(me, &asked).await;
    psp.fake.decline(asked.request.get());
    psp.caught_up(payment).await;
    let declined = next(PaymentStatus::Created, Event::Decline)?;
    assert_moved(
        &pool,
        payment,
        (0, 0),
        PaymentStatus::Created,
        declined,
        "Decline from created",
    )
    .await?;
    n += 1;
    let payment = at(&mut psp, me, merchant, n, PaymentStatus::Authorized).await?;
    let voiding = RecordRequest {
        request: request_id(n),
        payment,
        ask: Ask::Void,
    };
    psp.sent(me, &voiding).await;
    psp.fake.approve(voiding.request.get());
    psp.caught_up(payment).await;
    let voided = next(PaymentStatus::Authorized, Event::Void)?;
    assert_moved(
        &pool,
        payment,
        (1, 0),
        PaymentStatus::Authorized,
        voided,
        "Void from authorized",
    )
    .await?;
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_command_on_an_unknown_payment_is_not_found(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let command = StepCommand {
        step: transition_id(1),
        payment: PaymentId::try_from(test_uuid(1))?,
    };
    let err = cancel(&app, &command).await.unwrap_err();
    assert!(
        matches!(err, StepError::NotFound { payment } if payment == command.payment),
        "{err:?}"
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_the_stored_reply_although_the_status_has_moved_on(
    pool: PgPool,
) -> TestResult {
    // Checked against the state first, this retry would be refused as illegal:
    // a voided payment cannot be cancelled (ADR-0019).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    assert_eq!(cancel(&app, &command).await?, Outcome::Applied(CANCELLED));
    for _ in 0..2 {
        assert_eq!(
            cancel(&app, &command).await?,
            Outcome::AlreadyApplied(CANCELLED)
        );
    }
    assert_eq!(steps(&pool, command.payment).await?, 2);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_its_own_steps_reply_and_not_the_newest_steps(pool: PgPool) -> TestResult {
    // A create, then a newer step from the PSP's fact: the create's retry gets the
    // create's own reply, by identity (ADR-0019).
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let command = create_command(1, merchant);
    let created = Recorded {
        seq: 0,
        from: PaymentStatus::Created,
        to: PaymentStatus::Created,
        refundable: 0,
    };
    assert_eq!(
        create_payment(me, &command).await?,
        Outcome::Applied(created)
    );
    psp.authorized(me, command.id).await;
    assert_eq!(
        status_of(&psp.system, command.id).await?,
        PaymentStatus::Authorized
    );
    assert_eq!(
        create_payment(me, &command).await?,
        Outcome::AlreadyApplied(created)
    );
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_changed_retry_is_a_mismatch(pool: PgPool) -> TestResult {
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    // The ID of the payment's create, sent as a cancel of the same payment.
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let same_id = StepCommand {
        step: TransitionId::try_from(first.id)?,
        payment: PaymentId::try_from(first.payment_id)?,
    };
    let err = cancel(&app, &same_id).await.unwrap_err();
    assert!(matches!(err, StepError::Mismatch { .. }), "{err:?}");
    assert_eq!(steps(&pool, same_id.payment).await?, 1);
    // A cancel's ID, on another payment, which stays as it was.
    let command = command_on(1, &first);
    cancel(&app, &command).await?;
    let other = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let err = cancel(&app, &command_on(1, &other)).await.unwrap_err();
    let StepError::Mismatch { step } = err else {
        panic!("expected Mismatch, got {err:?}");
    };
    assert_eq!(step, command.step.get());
    assert_eq!(steps(&pool, command.payment).await?, 2);
    assert_eq!(
        steps(&pool, PaymentId::try_from(other.payment_id)?).await?,
        1
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refused_command_is_decided_again(pool: PgPool) -> TestResult {
    // Rejections are not stored (ADR-0019): a refused command is decided again each
    // time, and its ID stays free for a command that applies.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let voided = committed_at(&pool, &books, PaymentStatus::Voided).await?;
    let refused = command_on(1, &voided);
    for _ in 0..2 {
        let err = cancel(&app, &refused).await.unwrap_err();
        assert!(
            matches!(
                err,
                StepError::Illegal(IllegalTransition {
                    status: PaymentStatus::Voided,
                    event: Event::Cancel,
                })
            ),
            "{err:?}"
        );
    }
    assert_eq!(steps(&pool, refused.payment).await?, 2);
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    assert_eq!(
        cancel(&app, &command_on(1, &first)).await?,
        Outcome::Applied(CANCELLED)
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_apply_once(pool: PgPool) -> TestResult {
    // Eight retries racing each other: they queue on the lock, one writes, and
    // the others read its step (ADR-0018).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let copies: Vec<StepCommand> = (0..8).map(|_| command_on(1, &first)).collect();
    let results = join_all(copies.iter().map(|copy| cancel(&app, copy))).await;
    let mut applied = 0;
    for result in results {
        match result? {
            Outcome::Applied(recorded) => {
                applied += 1;
                assert_eq!(recorded, CANCELLED);
            }
            Outcome::AlreadyApplied(recorded) => assert_eq!(recorded, CANCELLED),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, copies[0].payment).await?, 2);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn racing_commands_get_one_winner_and_a_domain_answer(pool: PgPool) -> TestResult {
    // Two cancels with IDs of their own: whichever gets the lock first, the other
    // waits, reads the winner's step and is refused by the table, with no database
    // error (ADR-0018).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    for n in 0..8 {
        let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
        let one = command_on(2 * n + 1, &first);
        let two = command_on(2 * n + 2, &first);
        let results = join(cancel(&app, &one), cancel(&app, &two)).await;
        let refused = match results {
            (Ok(Outcome::Applied(_)), Err(StepError::Illegal(illegal)))
            | (Err(StepError::Illegal(illegal)), Ok(Outcome::Applied(_))) => illegal,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            refused,
            IllegalTransition {
                status: PaymentStatus::Voided,
                event: Event::Cancel,
            }
        );
        assert_eq!(status_of(&app, one.payment).await?, PaymentStatus::Voided);
        assert_eq!(steps(&pool, one.payment).await?, 2);
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_command_waits_for_the_lock_and_reads_what_its_holder_committed(
    pool: PgPool,
) -> TestResult {
    // The read comes in a later statement than the lock, so at READ COMMITTED
    // it sees the holder's step. A read in the same statement would not.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let mut holder = runtime_transaction(&pool).await?;
    take_lock(&mut holder, command.payment).await?;
    first
        .then("cancel", "voided", 0, 0)
        .insert(&mut holder)
        .await?;
    let (answer, committed) = join(cancel(&app, &command), async {
        wait_for_lock_wait(&pool, "advisory").await?;
        commit(holder).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    committed?;
    let err = answer.unwrap_err();
    assert!(
        matches!(
            err,
            StepError::Illegal(IllegalTransition {
                status: PaymentStatus::Voided,
                event: Event::Cancel,
            })
        ),
        "{err:?}"
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_lock_timeout_is_not_confirmed_and_a_retry_settles_it(pool: PgPool) -> TestResult {
    // The first attempt has written but not committed, as when its COMMIT is
    // still on the way. Its retry gives up at the lock timeout with an unknown
    // outcome, and gets the original answer once the first attempt ends.
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let attempt = first_attempt_of_cancel(&pool, &first, &command).await?;
    let retry = runtime_pool_with_lock_timeout(&pool, 200).await?;
    let err = cancel(&retry, &command).await.unwrap_err();
    let StepError::NotConfirmed { source, .. } = err else {
        panic!("expected NotConfirmed, got {err:?}");
    };
    assert_eq!(sqlstate(source), "55P03", "lock_not_available");
    commit(attempt).await?;
    assert_eq!(
        cancel(&retry, &command).await?,
        Outcome::AlreadyApplied(CANCELLED)
    );
    retry.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_applies_after_a_first_attempt_rolls_back(pool: PgPool) -> TestResult {
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let attempt = first_attempt_of_cancel(&pool, &first, &command).await?;
    attempt.rollback().await?;
    let app = runtime_pool(&pool).await?;
    assert_eq!(cancel(&app, &command).await?, Outcome::Applied(CANCELLED));
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_outlasts_a_first_attempt_left_idle(pool: PgPool) -> TestResult {
    // ADR-0005's sizing, scaled down: the server ends a session left idle in
    // its transaction after 1 second here (30 in production), which releases
    // the lock, and a retry that may wait 10 seconds (35) outlasts it.
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let mut attempt = first_attempt_of_cancel(&pool, &first, &command).await?;
    sqlx::query!("SET LOCAL idle_in_transaction_session_timeout = '1s'")
        .execute(&mut *attempt)
        .await?;
    let retry = runtime_pool_with_lock_timeout(&pool, 10_000).await?;
    assert_eq!(cancel(&retry, &command).await?, Outcome::Applied(CANCELLED));
    // The server has ended the first attempt's session.
    assert!(attempt.commit().await.is_err());
    retry.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_writer_that_skips_the_lock_and_commits_first_makes_a_command_a_bug(
    pool: PgPool,
) -> TestResult {
    // The lock is free, so the command takes it, and reads the old state, since
    // the writer has not committed. Its insert then waits on the writer's row
    // and fails on the primary key: reported as a bug, and never written wrongly
    // (ADR-0018).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let mut writer = runtime_transaction(&pool).await?;
    first
        .then("cancel", "voided", 0, 0)
        .insert(&mut writer)
        .await?;
    let (answer, committed) = join(cancel(&app, &command), async {
        wait_for_lock_wait(&pool, "transactionid").await?;
        commit(writer).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    committed?;
    let err = answer.unwrap_err();
    let StepError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(source, UNIQUE_VIOLATION, "payment_transitions_pkey");
    assert_eq!(
        status_of(&app, command.payment).await?,
        PaymentStatus::Voided
    );
    assert_eq!(steps(&pool, command.payment).await?, 2);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_duplicate_key_is_settled_by_the_step_with_the_commands_id(pool: PgPool) -> TestResult {
    // A writer that skips the lock stores the command's own ID after the
    // command's read. Equal content is the command applied, and different
    // content a mismatch, however the duplicate came about (ADR-0019). The pool
    // has one connection, so the lookup cannot keep the failed transaction's.
    let books = books(&pool).await?;
    let app = runtime_pool_of_one(&pool).await?;
    for (n, equal) in [(1, true), (2, false)] {
        let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
        let command = command_on(n, &first);
        // The same cancel, or one with the same ID on another payment.
        let target = if equal {
            first
        } else {
            committed_at(&pool, &books, PaymentStatus::Created).await?
        };
        let mut writer = runtime_transaction(&pool).await?;
        let stored = Step {
            id: command.step.get(),
            ..target.then("cancel", "voided", 0, 0)
        };
        stored.insert(&mut writer).await?;
        let (answer, committed) = join(cancel(&app, &command), async {
            wait_for_lock_wait(&pool, "transactionid").await?;
            commit(writer).await?;
            Ok::<_, Box<dyn Error>>(())
        })
        .await;
        committed?;
        if equal {
            assert_eq!(answer?, Outcome::AlreadyApplied(CANCELLED));
        } else {
            let err = answer.unwrap_err();
            assert!(matches!(err, StepError::Mismatch { .. }), "{err:?}");
            assert_eq!(steps(&pool, command.payment).await?, 1);
        }
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_batch_queues_behind_a_command_and_a_command_behind_the_batch(
    pool: PgPool,
) -> TestResult {
    // Phase 6 will settle many payments in one transaction: it locks them all
    // first, in ascending key order, and then waits its turn instead of aborting
    // (ADR-0018). The batch here is raw SQL under the same locks, and its steps
    // are the merchant's own.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let one = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let two = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let mut ids = [one, two].map(|step| PaymentId::try_from(step.payment_id).expect("a UUIDv7"));
    ids.sort_by_key(|id| lock_key(*id));

    // Something in flight holds the first key, so the batch waits.
    let mut holder = runtime_transaction(&pool).await?;
    take_lock(&mut holder, ids[0]).await?;
    let mut batch = runtime_transaction(&pool).await?;
    let (locked, released) = join(
        async {
            for id in ids {
                take_lock(&mut batch, id).await?;
            }
            Ok::<_, sqlx::Error>(())
        },
        async {
            wait_for_lock_wait(&pool, "advisory").await?;
            holder.rollback().await
        },
    )
    .await;
    locked?;
    released?;

    // The batch holds both keys and writes, so a real command waits in turn,
    // then gets a domain answer.
    for step in [one, two] {
        step.then("cancel", "voided", 0, 0)
            .insert(&mut batch)
            .await?;
    }
    let command = command_on(1, &one);
    let (answer, committed) = join(cancel(&app, &command), async {
        wait_for_lock_wait(&pool, "advisory").await?;
        commit(batch).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    committed?;
    let err = answer.unwrap_err();
    assert!(
        matches!(
            err,
            StepError::Illegal(IllegalTransition {
                status: PaymentStatus::Voided,
                event: Event::Cancel,
            })
        ),
        "{err:?}"
    );
    for id in ids {
        assert_eq!(status_of(&app, id).await?, PaymentStatus::Voided);
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_write_that_a_trigger_corrupts_is_a_bug(pool: PgPool) -> TestResult {
    // The types allow the write; only the CHECK can refuse what reaches it.
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    sqlx::query!(
        "CREATE FUNCTION corrupt_status() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.to_status := 'settled'; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER corrupt_status BEFORE INSERT ON payment_transitions \
         FOR EACH ROW EXECUTE FUNCTION corrupt_status()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let command = command_on(1, &first);
    let err = cancel(&app, &command).await.unwrap_err();
    let StepError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(source, CHECK_VIOLATION, "payment_transitions_edge_check");
    assert_eq!(steps(&pool, command.payment).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_deferred_checks_rejection_is_a_bug_and_rolls_back(pool: PgPool) -> TestResult {
    // A trigger turns a cancel into a legal authorization, which has no record,
    // so only the check at COMMIT can notice: LG004 proves a rollback (ADR-0014).
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    sqlx::query!(
        "CREATE FUNCTION become_authorization() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.kind := 'authorize'; NEW.to_status := 'authorized'; NEW.amount := 1; \
         RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER become_authorization BEFORE INSERT ON payment_transitions \
         FOR EACH ROW EXECUTE FUNCTION become_authorization()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let command = command_on(1, &first);
    let err = cancel(&app, &command).await.unwrap_err();
    let StepError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(
        source,
        "LG004",
        "payment_transitions_commit_check",
        "payment_transitions",
    );
    assert_eq!(steps(&pool, command.payment).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unproven_commit_failure_is_settled_by_sending_again(pool: PgPool) -> TestResult {
    // A deferred trigger ends the session during COMMIT, which reports FATAL
    // 57P01: no proof of a rollback (ADR-0014). The function runs as its owner,
    // the superuser, which may end a session that the superuser opened.
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    sqlx::query!(
        "CREATE FUNCTION end_own_session() RETURNS trigger LANGUAGE plpgsql \
         SECURITY DEFINER AS $$ \
         BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER zz_end_session AFTER INSERT ON payment_transitions \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION end_own_session()"
    )
    .execute(&pool)
    .await?;
    let app = runtime_pool(&pool).await?;
    let command = command_on(1, &first);
    let err = cancel(&app, &command).await.unwrap_err();
    assert!(matches!(err, StepError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(steps(&pool, command.payment).await?, 1);
    // The caller's one rule: send the same command again.
    sqlx::query!("DROP TRIGGER zz_end_session ON payment_transitions")
        .execute(&pool)
        .await?;
    assert_eq!(cancel(&app, &command).await?, Outcome::Applied(CANCELLED));
    app.close().await;
    Ok(())
}

/// Every advisory-lock function that a Rust file under `dir` names.
fn advisory_lock_functions(dir: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("the directory exists") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            advisory_lock_functions(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = fs::read_to_string(&path).expect("a Rust file is UTF-8");
            found.extend(
                text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .filter(|word| word.starts_with("pg_") && word.contains("advisory"))
                    .map(str::to_owned),
            );
        }
    }
}

#[test]
fn the_library_calls_only_the_transaction_level_lock() {
    // A session-level lock outlives its transaction, so a session that then
    // sits idle blocks the payment (ADR-0018). Comments count too, which keeps
    // the search simple and the source free of the names.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    advisory_lock_functions(&src, &mut found);
    assert!(!found.is_empty(), "the search should find the lock itself");
    assert!(
        found.iter().all(|name| name == "pg_advisory_xact_lock"),
        "{found:?}"
    );
}
