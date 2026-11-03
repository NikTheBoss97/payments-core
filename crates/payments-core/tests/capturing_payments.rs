// Author: Nicola Antonello

//! Capturing a payment through the library, as the runtime role (ADR-0016,
//! ADR-0019, ADR-0020, ADR-0021): the first command that moves money. Its step,
//! its posting and its record commit together or not at all, so these tests check
//! each account's change, not only the total, and that nothing is ever half
//! written. The path that every command shares is tried in `payment_steps.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, NewPayment, REQUESTED, Step, authorize_command, balance_of, books,
    capture_command, committed_at, created_with, eur, fee_rate, insert_authorization, status_of,
    steps, unbalanced_currencies, uncaused_postings,
};
use common::{
    CHECK_VIOLATION, UNIQUE_VIOLATION, assert_guard, assert_violation, merchant_id, payment_id,
    runtime_pool, runtime_pool_of_one, runtime_transaction, sqlstate, transaction_id,
    transition_id,
};
use futures_util::future::{join, join_all};
use payments_core::accounts::{AccountRef, Psp, find_account};
use payments_core::db::commit;
use payments_core::ids::{AccountId, MerchantId, PaymentId};
use payments_core::ledger::{Entry, LedgerTransaction};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::{Amount, BasisPoints, PositiveAmount};
use payments_core::payments::{
    Authorize, Capture, CaptureError, Outcome, Recorded, StepCommand, StepError, authorize,
    capture, register_merchant, settle, void,
};
use payments_core::posting::post;
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// What capturing an authorized payment for [`AUTHORIZED`] records.
const CAPTURED_REPLY: Recorded = Recorded {
    seq: 2,
    from: PaymentStatus::Authorized,
    to: PaymentStatus::Captured,
    refundable: AUTHORIZED,
};

/// A runtime pool and a registered merchant.
async fn app_with_merchant(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let app = runtime_pool(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    Ok((app, merchant))
}

/// Creates payment `n` at this fee rate and authorizes it for [`AUTHORIZED`].
async fn authorized_at(
    app: &PgPool,
    merchant: MerchantId,
    n: u32,
    fee_bps: u16,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created_with(app, merchant, n, BasisPoints::new(fee_bps)?).await?;
    authorize(app, &authorize_command(n, payment)).await?;
    Ok(payment)
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("valid amount")
}

/// How many ledger transactions are stored.
async fn postings(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM ledger_transactions"#)
        .fetch_one(pool)
        .await
}

/// Each account's balance after a capture: the PSP's receivable, the merchant's
/// payable, and the platform's fees.
async fn balances(app: &PgPool, merchant: MerchantId) -> Result<[i64; 3], Box<dyn Error>> {
    Ok([
        balance_of(app, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        balance_of(app, AccountRef::MerchantPayable(merchant, eur())).await?,
        balance_of(app, AccountRef::FeeRevenue(eur())).await?,
    ])
}

/// An account's ID.
async fn account_of(app: &PgPool, reference: AccountRef) -> Result<AccountId, Box<dyn Error>> {
    let account = find_account(app, &reference.key()).await?;
    Ok(account.ok_or("the account exists")?.id)
}

#[sqlx::test]
async fn a_capture_stores_its_step_its_posting_and_its_record(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    let command = capture_command(1, payment);
    assert_eq!(
        capture(&app, &command).await?,
        Outcome::Applied(CAPTURED_REPLY)
    );
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Captured);
    // ADR-0020's lines: 80.00 from the PSP, 2.5% of it to the platform.
    assert_eq!(
        balances(&app, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    let step = sqlx::query!(
        "SELECT seq, prev_seq, kind, from_status, to_status, amount, prev_refundable, refundable \
           FROM payment_transitions WHERE id = $1",
        command.step.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((step.seq, step.prev_seq), (2, 1));
    assert_eq!(step.kind, "capture");
    assert_eq!(
        (step.from_status.as_str(), step.to_status.as_str()),
        ("authorized", "captured")
    );
    assert_eq!(step.amount, AUTHORIZED);
    assert_eq!((step.prev_refundable, step.refundable), (0, AUTHORIZED));
    let record = sqlx::query!(
        "SELECT payment_id, amount, authorized_amount, fee, captured_at, ledger_transaction_id \
           FROM payment_captures WHERE transition_id = $1",
        command.step.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(record.payment_id, payment.get());
    assert_eq!(record.amount, AUTHORIZED);
    assert_eq!(record.authorized_amount, AUTHORIZED);
    assert_eq!(record.fee, FEE);
    assert_eq!(record.captured_at, command.captured_at.get());
    assert_eq!(record.ledger_transaction_id, command.posting.get());
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_fee_follows_the_rate_and_rounds_half_up(pool: PgPool) -> TestResult {
    // Each on a merchant of its own, so that the balances are this capture's:
    // (rate, captured) -> fee. A fee of 0 or of the whole capture leaves a line
    // out, and the check at COMMIT still finds exactly its lines (ADR-0020).
    let app = runtime_pool(&pool).await?;
    for (n, rate, captured, fee) in [
        (1, 250, 8_000, 200),
        (2, 0, 8_000, 0),
        (3, 10_000, 8_000, 8_000),
        // 100 * 2.5% is exactly 2.5, and the half goes to the platform.
        (4, 250, 100, 3),
        // 100 * 2.4% is 2.4, which rounds down.
        (5, 240, 100, 2),
        (6, 1, 1, 0),
    ] {
        let merchant = merchant_id(n);
        register_merchant(&app, merchant, fee_rate()).await?;
        let before = balance_of(&app, AccountRef::FeeRevenue(eur())).await?;
        let receivable_before =
            balance_of(&app, AccountRef::PspReceivable(Psp::Mock, eur())).await?;
        let payment = authorized_at(&app, merchant, n, rate).await?;
        let command = Capture {
            amount: amount(captured),
            ..capture_command(n, payment)
        };
        capture(&app, &command).await?;
        let [receivable, payable, fees] = balances(&app, merchant).await?;
        let context = format!("{captured} at {rate} basis points");
        assert_eq!(receivable - receivable_before, captured, "{context}");
        assert_eq!(payable, fee - captured, "{context}");
        assert_eq!(fees - before, -fee, "{context}");
    }
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_amount_is_at_most_the_authorization(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    // A partial capture and the whole authorization.
    for (n, minor) in [(1, 1), (2, AUTHORIZED)] {
        let payment = authorized_at(&app, merchant, n, 250).await?;
        let command = Capture {
            amount: amount(minor),
            ..capture_command(n, payment)
        };
        assert_eq!(
            capture(&app, &command).await?,
            Outcome::Applied(Recorded {
                refundable: minor,
                ..CAPTURED_REPLY
            })
        );
    }
    // Above the authorization, though within the request.
    let payment = authorized_at(&app, merchant, 3, 250).await?;
    let before = postings(&pool).await?;
    let command = Capture {
        amount: amount(AUTHORIZED + 1),
        ..capture_command(3, payment)
    };
    let err = capture(&app, &command).await.unwrap_err();
    assert!(
        matches!(
            err,
            CaptureError::Refused(Refused::AboveLimit { amount, limit })
                if amount == AUTHORIZED + 1 && limit == AUTHORIZED
        ),
        "{err:?}"
    );
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Authorized);
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(postings(&pool).await?, before);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_status_is_judged_before_the_amount(pool: PgPool) -> TestResult {
    // Every status but `authorized`, with an amount above any limit as well: the
    // answer is the illegal move, and nothing is posted (ADR-0016).
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let mut n = 0;
    for &status in PaymentStatus::ALL {
        if status == PaymentStatus::Authorized {
            continue;
        }
        n += 1;
        let newest = committed_at(&pool, &books, status).await?;
        let payment = PaymentId::try_from(newest.payment_id)?;
        let before = postings(&pool).await?;
        let command = Capture {
            amount: amount(REQUESTED + 1),
            ..capture_command(n, payment)
        };
        let err = capture(&app, &command).await.unwrap_err();
        assert!(
            matches!(
                err,
                CaptureError::Refused(Refused::Illegal(IllegalTransition {
                    status: got,
                    event: Event::Capture,
                })) if got == status
            ),
            "{status}: {err:?}"
        );
        assert_eq!(steps(&pool, payment).await?, i64::from(newest.seq) + 1);
        assert_eq!(postings(&pool).await?, before);
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_payment_is_not_found(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let unknown = payment_id(1);
    let err = capture(&app, &capture_command(1, unknown))
        .await
        .unwrap_err();
    assert!(
        matches!(err, CaptureError::NotFound { payment } if payment == unknown),
        "{err:?}"
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_payment_written_without_its_accounts_is_reported(pool: PgPool) -> TestResult {
    // A manual session wrote the payment, so nothing opened its accounts. Posting
    // never creates them: a typo would become an account (ADR-0005).
    let mut tx = runtime_transaction(&pool).await?;
    let merchant = common::payments::register_merchant(&mut tx).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    let first = Step::first(payment.id);
    first.insert(&mut tx).await?;
    let authorized = first.then("authorize", "authorized", AUTHORIZED, 0);
    authorized.insert(&mut tx).await?;
    insert_authorization(&mut tx, &authorized, REQUESTED).await?;
    commit(tx).await?;
    let app = runtime_pool(&pool).await?;
    let id = PaymentId::try_from(payment.id)?;
    let err = capture(&app, &capture_command(1, id)).await.unwrap_err();
    let CaptureError::MissingAccount { key } = err else {
        panic!("expected MissingAccount, got {err:?}");
    };
    assert_eq!(key.to_string(), "psp_receivable/mock/EUR");
    assert_eq!(status_of(&app, id).await?, PaymentStatus::Authorized);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_the_stored_reply_although_the_status_has_moved_on(
    pool: PgPool,
) -> TestResult {
    // On a pool of one: the retry compares the stored entries while it holds the
    // lock, so it must do that on the connection that it has.
    let app = runtime_pool_of_one(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    let command = capture_command(1, payment);
    capture(&app, &command).await?;
    assert_eq!(
        capture(&app, &command).await?,
        Outcome::AlreadyApplied(CAPTURED_REPLY)
    );
    let settling = StepCommand {
        step: transition_id(5001),
        payment,
    };
    assert_eq!(
        settle(&app, &settling).await?,
        Outcome::Applied(Recorded {
            seq: 3,
            from: PaymentStatus::Captured,
            to: PaymentStatus::Settled,
            refundable: AUTHORIZED,
        })
    );
    assert_eq!(
        capture(&app, &command).await?,
        Outcome::AlreadyApplied(CAPTURED_REPLY)
    );
    // Applied once: one posting, and the money moved once.
    assert_eq!(postings(&pool).await?, 1);
    assert_eq!(
        balances(&app, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_changed_retry_is_a_mismatch_for_each_field(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    capture(&app, &capture_command(1, payment)).await?;
    let later = BusinessTime::new("2026-01-10T12:00:00Z".parse()?)?;
    let other = authorized_at(&app, merchant, 2, 250).await?;
    // Another capture of the same size for the same merchant: its posting has the
    // very lines of this one, so only the record's link can tell them apart.
    let twin = authorized_at(&app, merchant, 3, 250).await?;
    capture(&app, &capture_command(3, twin)).await?;
    let before = balances(&app, merchant).await?;
    for changed in [
        Capture {
            amount: amount(AUTHORIZED - 1),
            ..capture_command(1, payment)
        },
        Capture {
            captured_at: later,
            ..capture_command(1, payment)
        },
        Capture {
            posting: transaction_id(99),
            ..capture_command(1, payment)
        },
        Capture {
            posting: transaction_id(3),
            ..capture_command(1, payment)
        },
        Capture {
            payment: other,
            ..capture_command(1, payment)
        },
    ] {
        let err = capture(&app, &changed).await.unwrap_err();
        let CaptureError::Mismatch { step } = err else {
            panic!("expected Mismatch for {changed:?}, got {err:?}");
        };
        assert_eq!(step, changed.step.get());
    }
    assert_eq!(status_of(&app, other).await?, PaymentStatus::Authorized);
    assert_eq!(postings(&pool).await?, 2);
    assert_eq!(balances(&app, merchant).await?, before);
    app.close().await;
    Ok(())
}

/// A first attempt of `command` written by hand and committed, after `authorized`,
/// with this fee and these lines. The checks at COMMIT hold it to the same rules as
/// the library, but compare the lines as a set.
async fn capture_by_hand(
    pool: &PgPool,
    authorized: &Step,
    command: &Capture,
    fee: i64,
    lines: &[(Uuid, i64)],
) -> Result<(), Box<dyn Error>> {
    let mut tx = runtime_transaction(pool).await?;
    let stored = Step {
        id: command.step.get(),
        ..authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED)
    };
    stored.insert(&mut tx).await?;
    let id = command.posting.get();
    let entry_count = i32::try_from(lines.len())?;
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, 'EUR')",
        id,
        entry_count
    )
    .execute(&mut *tx)
    .await?;
    for (line_no, (account, minor)) in (1..).zip(lines) {
        sqlx::query!(
            "INSERT INTO ledger_entries \
                 (transaction_id, line_no, entry_count, account_id, currency, amount) \
             VALUES ($1, $2, $3, $4, 'EUR', $5)",
            id,
            line_no,
            entry_count,
            account,
            minor
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "INSERT INTO payment_captures \
             (transition_id, payment_id, amount, authorized_amount, fee, captured_at, \
              ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        stored.id,
        stored.payment_id,
        AUTHORIZED,
        AUTHORIZED,
        fee,
        command.captured_at.get(),
        id
    )
    .execute(&mut *tx)
    .await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_first_attempt_with_the_same_content_settles_a_retry(pool: PgPool) -> TestResult {
    // The control for the two tests below: written by hand, with the lines and
    // the fee that the command would write, it is the command applied.
    let books = books(&pool).await?;
    let authorized = committed_at(&pool, &books, PaymentStatus::Authorized).await?;
    let command = capture_command(1, PaymentId::try_from(authorized.payment_id)?);
    let lines = books.capture_lines(AUTHORIZED, FEE);
    capture_by_hand(&pool, &authorized, &command, FEE, &lines).await?;
    let app = runtime_pool(&pool).await?;
    assert_eq!(
        capture(&app, &command).await?,
        Outcome::AlreadyApplied(CAPTURED_REPLY)
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_compares_the_postings_entries(pool: PgPool) -> TestResult {
    // The same step and record, and the right lines in another order. The check
    // at COMMIT accepts them, since it compares a set. The command would not have
    // written these, and its retry says so, through the comparison that post()
    // uses (ADR-0019).
    let books = books(&pool).await?;
    let authorized = committed_at(&pool, &books, PaymentStatus::Authorized).await?;
    let command = capture_command(1, PaymentId::try_from(authorized.payment_id)?);
    let mut lines = books.capture_lines(AUTHORIZED, FEE);
    lines.reverse();
    capture_by_hand(&pool, &authorized, &command, FEE, &lines).await?;
    let app = runtime_pool(&pool).await?;
    let err = capture(&app, &command).await.unwrap_err();
    assert!(matches!(err, CaptureError::Mismatch { .. }), "{err:?}");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_compares_the_stored_fee(pool: PgPool) -> TestResult {
    // The fee comes from the amount and the payment's fixed rate, so a command
    // cannot ask for another one. A first attempt by hand can store one, with the
    // lines that it implies, and this command did not ask for that. The check at
    // COMMIT ties a fee to its lines, so the entries differ as well, and comparing
    // either would answer this: ADR-0019 compares both.
    let books = books(&pool).await?;
    let authorized = committed_at(&pool, &books, PaymentStatus::Authorized).await?;
    let command = capture_command(1, PaymentId::try_from(authorized.payment_id)?);
    let lines = books.capture_lines(AUTHORIZED, FEE + 1);
    capture_by_hand(&pool, &authorized, &command, FEE + 1, &lines).await?;
    let app = runtime_pool(&pool).await?;
    let err = capture(&app, &command).await.unwrap_err();
    assert!(matches!(err, CaptureError::Mismatch { .. }), "{err:?}");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_posting_id_that_exists_without_this_step_is_a_bug(pool: PgPool) -> TestResult {
    // The ID is taken by a posting that nothing caused, so the duplicate key has
    // no stored step to explain it.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    let command = capture_command(1, payment);
    let receivable = account_of(&app, AccountRef::PspReceivable(Psp::Mock, eur())).await?;
    let fees = account_of(&app, AccountRef::FeeRevenue(eur())).await?;
    let taken = LedgerTransaction::new(
        transaction_id(1),
        eur(),
        vec![
            Entry {
                account: receivable,
                amount: Amount::new(10)?,
            },
            Entry {
                account: fees,
                amount: Amount::new(-10)?,
            },
        ],
    )?;
    post(&app, &taken).await?;
    let err = capture(&app, &command).await.unwrap_err();
    let CaptureError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_eq!(sqlstate(source), UNIQUE_VIOLATION);
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(postings(&pool).await?, 1);
    // The detection query of ADR-0021 reports the posting that nothing caused.
    assert_eq!(uncaused_postings(&pool).await?, [taken.id().get()]);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_apply_once(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    let copies: Vec<Capture> = (0..8).map(|_| capture_command(1, payment)).collect();
    let results = join_all(copies.iter().map(|copy| capture(&app, copy))).await;
    let mut applied = 0;
    for result in results {
        match result? {
            Outcome::Applied(recorded) => {
                applied += 1;
                assert_eq!(recorded, CAPTURED_REPLY);
            }
            Outcome::AlreadyApplied(recorded) => assert_eq!(recorded, CAPTURED_REPLY),
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(postings(&pool).await?, 1);
    assert_eq!(
        balances(&app, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_capture_racing_a_void_gets_one_winner(pool: PgPool) -> TestResult {
    // The money moves only if the capture won, whichever order the lock gives.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let mut captures = 0;
    for n in 1..=8 {
        let payment = authorized_at(&app, merchant, n, 250).await?;
        let capturing = capture_command(n, payment);
        let voiding = StepCommand {
            step: transition_id(n + 5000),
            payment,
        };
        let results = join(capture(&app, &capturing), void(&app, &voiding)).await;
        let winner = match results {
            (Ok(Outcome::Applied(_)), Err(StepError::Illegal(illegal))) => {
                assert_eq!(
                    illegal,
                    IllegalTransition {
                        status: PaymentStatus::Captured,
                        event: Event::Void,
                    }
                );
                captures += 1;
                PaymentStatus::Captured
            }
            (Err(CaptureError::Refused(Refused::Illegal(illegal))), Ok(Outcome::Applied(_))) => {
                assert_eq!(
                    illegal,
                    IllegalTransition {
                        status: PaymentStatus::Voided,
                        event: Event::Capture,
                    }
                );
                PaymentStatus::Voided
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(status_of(&app, payment).await?, winner);
        assert_eq!(steps(&pool, payment).await?, 3);
    }
    assert_eq!(postings(&pool).await?, captures);
    assert_eq!(
        balances(&app, merchant).await?,
        [
            captures * AUTHORIZED,
            captures * (FEE - AUTHORIZED),
            -captures * FEE
        ]
    );
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_posting_that_a_trigger_corrupts_fails_at_commit(pool: PgPool) -> TestResult {
    // Still balanced, so the ledger's own check passes: only the check of the
    // record against its posting can notice, and LG006 proves a rollback. The
    // step, the posting and the record all go (ADR-0014, ADR-0021).
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    sqlx::query!(
        "CREATE FUNCTION shift_a_unit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
             IF NEW.line_no = 2 THEN NEW.amount := NEW.amount + 1; END IF; \
             IF NEW.line_no = 3 THEN NEW.amount := NEW.amount - 1; END IF; \
             RETURN NEW; \
         END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER shift_a_unit BEFORE INSERT ON ledger_entries \
         FOR EACH ROW EXECUTE FUNCTION shift_a_unit()"
    )
    .execute(&pool)
    .await?;
    let err = capture(&app, &capture_command(1, payment))
        .await
        .unwrap_err();
    let CaptureError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(
        source,
        "LG006",
        "payment_captures_commit_check",
        "payment_captures",
    );
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(postings(&pool).await?, 0);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_loses_fails_at_commit(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    sqlx::query!(
        "CREATE FUNCTION lose_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER lose_record BEFORE INSERT ON payment_captures \
         FOR EACH ROW EXECUTE FUNCTION lose_row()"
    )
    .execute(&pool)
    .await?;
    let err = capture(&app, &capture_command(1, payment))
        .await
        .unwrap_err();
    let CaptureError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(
        source,
        "LG004",
        "payment_transitions_commit_check",
        "payment_transitions",
    );
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(postings(&pool).await?, 0);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_corrupts_is_a_bug(pool: PgPool) -> TestResult {
    // The fee is above the capture, which only the CHECK can refuse.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    sqlx::query!(
        "CREATE FUNCTION raise_fee() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.fee := NEW.amount + 1; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER raise_fee BEFORE INSERT ON payment_captures \
         FOR EACH ROW EXECUTE FUNCTION raise_fee()"
    )
    .execute(&pool)
    .await?;
    let err = capture(&app, &capture_command(1, payment))
        .await
        .unwrap_err();
    let CaptureError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(source, CHECK_VIOLATION, "payment_captures_fee_check");
    assert_eq!(postings(&pool).await?, 0);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unproven_commit_failure_is_settled_by_sending_again(pool: PgPool) -> TestResult {
    // A deferred trigger ends the session during COMMIT, which proves nothing
    // (ADR-0014). Here nothing was stored, and the same command then applies, with
    // the same posting ID.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = authorized_at(&app, merchant, 1, 250).await?;
    sqlx::query!(
        "CREATE FUNCTION end_own_session() RETURNS trigger LANGUAGE plpgsql \
         SECURITY DEFINER AS $$ \
         BEGIN PERFORM pg_terminate_backend(pg_backend_pid()); RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE CONSTRAINT TRIGGER zz_end_session AFTER INSERT ON payment_captures \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION end_own_session()"
    )
    .execute(&pool)
    .await?;
    let command = capture_command(1, payment);
    let err = capture(&app, &command).await.unwrap_err();
    assert!(matches!(err, CaptureError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(postings(&pool).await?, 0);
    sqlx::query!("DROP TRIGGER zz_end_session ON payment_captures")
        .execute(&pool)
        .await?;
    assert_eq!(
        capture(&app, &command).await?,
        Outcome::Applied(CAPTURED_REPLY)
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_partial_authorization_limits_the_capture(pool: PgPool) -> TestResult {
    // The limit is what the PSP authorized, read under the lock, not the request.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = created_with(&app, merchant, 1, BasisPoints::new(250)?).await?;
    let authorizing = Authorize {
        amount: amount(500),
        ..authorize_command(1, payment)
    };
    authorize(&app, &authorizing).await?;
    let too_much = Capture {
        amount: amount(501),
        ..capture_command(1, payment)
    };
    let err = capture(&app, &too_much).await.unwrap_err();
    assert!(
        matches!(
            err,
            CaptureError::Refused(Refused::AboveLimit {
                amount: 501,
                limit: 500,
            })
        ),
        "{err:?}"
    );
    let within = Capture {
        amount: amount(500),
        ..capture_command(1, payment)
    };
    assert_eq!(
        capture(&app, &within).await?,
        Outcome::Applied(Recorded {
            refundable: 500,
            ..CAPTURED_REPLY
        })
    );
    app.close().await;
    Ok(())
}
