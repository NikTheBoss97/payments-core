// Author: Nicola Antonello

//! The commands that carry no amount, through the library, as the runtime role:
//! decline, cancel, void, expire and settle (ADR-0016, ADR-0019).
//!
//! Every later command shares their path, so this is also where that path is
//! tried: the payment's lock (ADR-0018), identity before state, and what each
//! failure proves. Payments reach each status through raw SQL, and a test that
//! needs a "first attempt" writes it the same way, under the same lock. Faults
//! that the types rule out are injected with triggers that the owner adds to the
//! test's own database.

mod common;

use std::error::Error;
use std::fs;
use std::path::Path;

use common::payments::{
    AUTHORIZED, REQUESTED, Step, books, committed_at, insert_authorization, refund_step, status_of,
    steps, take_lock,
};
use common::{
    CHECK_VIOLATION, UNIQUE_VIOLATION, assert_guard, assert_violation, runtime_pool,
    runtime_pool_of_one, runtime_pool_with_lock_timeout, runtime_transaction, sqlstate, test_uuid,
    transition_id, wait_for_lock_wait,
};
use futures_util::future::{join, join_all};
use payments_core::db::commit;
use payments_core::ids::PaymentId;
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, next};
use payments_core::payments::{
    Outcome, Recorded, StepCommand, StepError, cancel, decline, expire, lock_key, settle, void,
};
use sqlx::{PgPool, Postgres, Transaction};

type TestResult = Result<(), Box<dyn Error>>;

/// What declining a payment that stands at its first step records.
const DECLINED: Recorded = Recorded {
    seq: 1,
    from: PaymentStatus::Created,
    to: PaymentStatus::Failed,
    refundable: 0,
};

/// Sends the command for this event.
async fn send(app: &PgPool, event: Event, command: &StepCommand) -> Result<Outcome, StepError> {
    match event {
        Event::Decline => decline(app, command).await,
        Event::Cancel => cancel(app, command).await,
        Event::Void => void(app, command).await,
        Event::Expire => expire(app, command).await,
        Event::Settle => settle(app, command).await,
        other => panic!("{other:?} is not a command without an amount"),
    }
}

/// A command with step ID `n` on the payment that `step` belongs to.
fn command_on(n: u32, step: &Step) -> StepCommand {
    StepCommand {
        step: transition_id(n),
        payment: PaymentId::try_from(step.payment_id).expect("a UUIDv7"),
    }
}

/// A first attempt of `command`, uncommitted: under the lock, with its step
/// written as the library writes it.
async fn first_attempt_of_decline(
    pool: &PgPool,
    first: &Step,
    command: &StepCommand,
) -> Result<Transaction<'static, Postgres>, Box<dyn Error>> {
    let mut attempt = runtime_transaction(pool).await?;
    take_lock(&mut attempt, command.payment).await?;
    let stored = Step {
        id: command.step.get(),
        ..first.then("decline", "failed", 0, 0)
    };
    stored.insert(&mut attempt).await?;
    Ok(attempt)
}

#[sqlx::test]
async fn every_command_follows_the_table_from_every_status(pool: PgPool) -> TestResult {
    // 7 statuses and 5 commands, 35 pairs against the real tables. Rust's table
    // is checked on its own in the lifecycle's unit tests.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let mut n = 0;
    for &status in PaymentStatus::ALL {
        for event in [
            Event::Decline,
            Event::Cancel,
            Event::Void,
            Event::Expire,
            Event::Settle,
        ] {
            n += 1;
            let newest = committed_at(&pool, &books, status).await?;
            let command = command_on(n, &newest);
            let result = send(&app, event, &command).await;
            let context = format!("{event:?} from {status}");
            match next(status, event) {
                Ok(step) => {
                    let seq = newest.seq + 1;
                    let reply = Recorded {
                        seq,
                        from: status,
                        to: step.to(),
                        refundable: newest.refundable,
                    };
                    assert_eq!(result?, Outcome::Applied(reply), "{context}");
                    // Its retry finds the step, though the new status refuses the
                    // event, so each command must store the kind it compares.
                    assert_eq!(
                        send(&app, event, &command).await?,
                        Outcome::AlreadyApplied(reply),
                        "{context}"
                    );
                    let row = sqlx::query!(
                        "SELECT payment_id, seq, prev_seq, kind, from_status, to_status, amount, \
                                prev_refundable, refundable \
                           FROM payment_transitions WHERE id = $1",
                        command.step.get()
                    )
                    .fetch_one(&pool)
                    .await?;
                    assert_eq!(row.payment_id, newest.payment_id, "{context}");
                    assert_eq!((row.seq, row.prev_seq), (seq, newest.seq), "{context}");
                    assert_eq!(row.kind, step.kind().as_str(), "{context}");
                    assert_eq!(row.from_status, status.as_str(), "{context}");
                    assert_eq!(row.to_status, step.to().as_str(), "{context}");
                    assert_eq!(row.amount, 0, "{context}");
                    assert_eq!(row.prev_refundable, newest.refundable, "{context}");
                    assert_eq!(row.refundable, newest.refundable, "{context}");
                    assert_eq!(status_of(&app, command.payment).await?, step.to());
                }
                Err(illegal) => {
                    let err = result.unwrap_err();
                    assert!(
                        matches!(err, StepError::Illegal(got) if got == illegal),
                        "{context}: {err:?}"
                    );
                    assert_eq!(status_of(&app, command.payment).await?, status);
                    assert_eq!(
                        steps(&pool, command.payment).await?,
                        i64::from(newest.seq) + 1,
                        "{context}"
                    );
                }
            }
        }
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_command_on_an_unknown_payment_is_not_found(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let command = StepCommand {
        step: transition_id(1),
        payment: PaymentId::try_from(test_uuid(1))?,
    };
    let err = decline(&app, &command).await.unwrap_err();
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
    // a failed payment cannot be declined (ADR-0019).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    assert_eq!(decline(&app, &command).await?, Outcome::Applied(DECLINED));
    for _ in 0..2 {
        assert_eq!(
            decline(&app, &command).await?,
            Outcome::AlreadyApplied(DECLINED)
        );
    }
    assert_eq!(steps(&pool, command.payment).await?, 2);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_its_own_steps_reply_and_not_the_newest_steps(pool: PgPool) -> TestResult {
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let captured = committed_at(&pool, &books, PaymentStatus::Captured).await?;
    let command = command_on(1, &captured);
    let settled = Recorded {
        seq: captured.seq + 1,
        from: PaymentStatus::Captured,
        to: PaymentStatus::Settled,
        refundable: AUTHORIZED,
    };
    assert_eq!(settle(&app, &command).await?, Outcome::Applied(settled));
    // A newer step, written after it: the rest of the capture is refunded.
    let mut tx = runtime_transaction(&pool).await?;
    let stored = Step {
        id: command.step.get(),
        ..captured.then("settle", "settled", 0, AUTHORIZED)
    };
    refund_step(&mut tx, &books, &stored, "refunded", AUTHORIZED).await?;
    commit(tx).await?;
    assert_eq!(
        status_of(&app, command.payment).await?,
        PaymentStatus::Refunded
    );
    assert_eq!(
        settle(&app, &command).await?,
        Outcome::AlreadyApplied(settled)
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_changed_retry_is_a_mismatch(pool: PgPool) -> TestResult {
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    decline(&app, &command).await?;
    // The same ID, as another kind of step on the same payment.
    let err = cancel(&app, &command).await.unwrap_err();
    assert!(matches!(err, StepError::Mismatch { .. }), "{err:?}");
    // The same ID, on another payment, which stays as it was.
    let other = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let err = decline(&app, &command_on(1, &other)).await.unwrap_err();
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
    // Rejections are not stored (ADR-0019), so one refused while the payment
    // was created succeeds later, with the same ID.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let err = void(&app, &command).await.unwrap_err();
    assert!(
        matches!(
            err,
            StepError::Illegal(IllegalTransition {
                status: PaymentStatus::Created,
                event: Event::Void,
            })
        ),
        "{err:?}"
    );
    assert_eq!(steps(&pool, command.payment).await?, 1);
    let mut tx = runtime_transaction(&pool).await?;
    let authorized = first.then("authorize", "authorized", AUTHORIZED, 0);
    authorized.insert(&mut tx).await?;
    insert_authorization(&mut tx, &authorized, REQUESTED).await?;
    commit(tx).await?;
    assert_eq!(
        void(&app, &command).await?,
        Outcome::Applied(Recorded {
            seq: 2,
            from: PaymentStatus::Authorized,
            to: PaymentStatus::Voided,
            refundable: 0,
        })
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
    let results = join_all(copies.iter().map(|copy| decline(&app, copy))).await;
    let mut applied = 0;
    for result in results {
        match result? {
            Outcome::Applied(recorded) => {
                applied += 1;
                assert_eq!(recorded, DECLINED);
            }
            Outcome::AlreadyApplied(recorded) => assert_eq!(recorded, DECLINED),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, copies[0].payment).await?, 2);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn racing_commands_get_one_winner_and_a_domain_answer(pool: PgPool) -> TestResult {
    // Whichever gets the lock first, the other waits, reads the winner's step
    // and is refused by the table, with no database error (ADR-0018).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    for n in 0..8 {
        let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
        let declining = command_on(2 * n + 1, &first);
        let cancelling = command_on(2 * n + 2, &first);
        let results = join(decline(&app, &declining), cancel(&app, &cancelling)).await;
        let (status, event) = match results {
            (Ok(Outcome::Applied(_)), Err(StepError::Illegal(illegal))) => {
                assert_eq!(illegal.status, PaymentStatus::Failed);
                (PaymentStatus::Failed, illegal.event)
            }
            (Err(StepError::Illegal(illegal)), Ok(Outcome::Applied(_))) => {
                assert_eq!(illegal.status, PaymentStatus::Voided);
                (PaymentStatus::Voided, illegal.event)
            }
            other => panic!("{other:?}"),
        };
        let expected = match status {
            PaymentStatus::Failed => Event::Cancel,
            _ => Event::Decline,
        };
        assert_eq!(event, expected);
        assert_eq!(status_of(&app, declining.payment).await?, status);
        assert_eq!(steps(&pool, declining.payment).await?, 2);
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
    let (answer, committed) = join(decline(&app, &command), async {
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
                event: Event::Decline,
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
    let attempt = first_attempt_of_decline(&pool, &first, &command).await?;
    let retry = runtime_pool_with_lock_timeout(&pool, 200).await?;
    let err = decline(&retry, &command).await.unwrap_err();
    let StepError::NotConfirmed { source, .. } = err else {
        panic!("expected NotConfirmed, got {err:?}");
    };
    assert_eq!(sqlstate(source), "55P03", "lock_not_available");
    commit(attempt).await?;
    assert_eq!(
        decline(&retry, &command).await?,
        Outcome::AlreadyApplied(DECLINED)
    );
    retry.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_applies_after_a_first_attempt_rolls_back(pool: PgPool) -> TestResult {
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let command = command_on(1, &first);
    let attempt = first_attempt_of_decline(&pool, &first, &command).await?;
    attempt.rollback().await?;
    let app = runtime_pool(&pool).await?;
    assert_eq!(decline(&app, &command).await?, Outcome::Applied(DECLINED));
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
    let mut attempt = first_attempt_of_decline(&pool, &first, &command).await?;
    sqlx::query!("SET LOCAL idle_in_transaction_session_timeout = '1s'")
        .execute(&mut *attempt)
        .await?;
    let retry = runtime_pool_with_lock_timeout(&pool, 10_000).await?;
    assert_eq!(decline(&retry, &command).await?, Outcome::Applied(DECLINED));
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
    let (answer, committed) = join(decline(&app, &command), async {
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
    for (n, kind, to_status, equal) in [
        (1, "decline", "failed", true),
        (2, "cancel", "voided", false),
    ] {
        let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
        let command = command_on(n, &first);
        let mut writer = runtime_transaction(&pool).await?;
        let stored = Step {
            id: command.step.get(),
            ..first.then(kind, to_status, 0, 0)
        };
        stored.insert(&mut writer).await?;
        let (answer, committed) = join(decline(&app, &command), async {
            wait_for_lock_wait(&pool, "transactionid").await?;
            commit(writer).await?;
            Ok::<_, Box<dyn Error>>(())
        })
        .await;
        committed?;
        if equal {
            assert_eq!(answer?, Outcome::AlreadyApplied(DECLINED));
        } else {
            let err = answer.unwrap_err();
            assert!(matches!(err, StepError::Mismatch { .. }), "{err:?}");
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
    // (ADR-0018). The batch here is raw SQL, under the same locks.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let one = committed_at(&pool, &books, PaymentStatus::Captured).await?;
    let two = committed_at(&pool, &books, PaymentStatus::Captured).await?;
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
        step.then("settle", "settled", 0, AUTHORIZED)
            .insert(&mut batch)
            .await?;
    }
    let command = command_on(1, &one);
    let (answer, committed) = join(settle(&app, &command), async {
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
                status: PaymentStatus::Settled,
                event: Event::Settle,
            })
        ),
        "{err:?}"
    );
    for id in ids {
        assert_eq!(status_of(&app, id).await?, PaymentStatus::Settled);
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
    let err = decline(&app, &command).await.unwrap_err();
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
    let err = decline(&app, &command).await.unwrap_err();
    assert!(matches!(err, StepError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(steps(&pool, command.payment).await?, 1);
    // The caller's one rule: send the same command again.
    sqlx::query!("DROP TRIGGER zz_end_session ON payment_transitions")
        .execute(&pool)
        .await?;
    assert_eq!(decline(&app, &command).await?, Outcome::Applied(DECLINED));
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
