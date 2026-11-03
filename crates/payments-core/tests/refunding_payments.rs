// Author: Nicola Antonello

//! Refunding a payment through the library, as the runtime role (ADR-0016,
//! ADR-0019, ADR-0020). What can be refunded falls with each refund, so its limit
//! depends on every refund before it, and racing refunds are where the lock and
//! the history's constraints earn their keep. The path that every command shares
//! is tried in `payment_steps.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, authorize_command, balance_of, books, capture_command, committed_at,
    created_with, eur, fee_rate, refund_command, status_of, steps, unbalanced_currencies,
    uncaused_postings,
};
use common::{
    assert_guard, merchant_id, payment_id, runtime_pool, runtime_pool_of_one, transaction_id,
    transition_id,
};
use futures_util::future::{join, join_all};
use payments_core::accounts::{AccountRef, Psp};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::{BasisPoints, PositiveAmount};
use payments_core::payments::{
    Capture, Outcome, Recorded, Refund, RefundError, StepCommand, StepError, authorize, capture,
    find_payment, refund, register_merchant, settle,
};
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// A runtime pool and a registered merchant.
async fn app_with_merchant(pool: &PgPool) -> Result<(PgPool, MerchantId), Box<dyn Error>> {
    let app = runtime_pool(pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    Ok((app, merchant))
}

/// Takes payment `n` to `captured`, for [`AUTHORIZED`] with a fee of [`FEE`].
async fn captured(app: &PgPool, merchant: MerchantId, n: u32) -> Result<PaymentId, Box<dyn Error>> {
    captured_for(app, merchant, n, AUTHORIZED).await
}

/// Takes payment `n` to `captured`, for `minor`, at a fee rate of 2.5%.
async fn captured_for(
    app: &PgPool,
    merchant: MerchantId,
    n: u32,
    minor: i64,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created_with(app, merchant, n, BasisPoints::new(250)?).await?;
    authorize(app, &authorize_command(n, payment)).await?;
    let command = Capture {
        amount: PositiveAmount::new(minor)?,
        ..capture_command(n, payment)
    };
    capture(app, &command).await?;
    Ok(payment)
}

/// A command that settles a payment, with step ID `5000 + n`.
fn settle_command(n: u32, payment: PaymentId) -> StepCommand {
    StepCommand {
        step: transition_id(n + 5000),
        payment,
    }
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

/// How many ledger transactions are stored.
async fn postings(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM ledger_transactions"#)
        .fetch_one(pool)
        .await
}

async fn refundable_of(app: &PgPool, payment: PaymentId) -> Result<i64, Box<dyn Error>> {
    let payment = find_payment(app, payment).await?;
    Ok(payment.ok_or("the payment exists")?.refundable)
}

#[sqlx::test]
async fn a_partial_refund_keeps_the_status_and_lowers_what_is_left(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    let command = refund_command(1, payment, 3_000);
    assert_eq!(
        refund(&app, &command).await?,
        Outcome::Applied(Recorded {
            seq: 3,
            from: PaymentStatus::Captured,
            to: PaymentStatus::Captured,
            refundable: AUTHORIZED - 3_000,
        })
    );
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Captured);
    assert_eq!(refundable_of(&app, payment).await?, AUTHORIZED - 3_000);
    // ADR-0020's lines, on top of the capture's: the merchant pays it all.
    assert_eq!(
        balances(&app, merchant).await?,
        [AUTHORIZED - 3_000, FEE - AUTHORIZED + 3_000, -FEE]
    );
    let step = sqlx::query!(
        "SELECT seq, prev_seq, kind, from_status, to_status, amount, prev_refundable, refundable \
           FROM payment_transitions WHERE id = $1",
        command.step.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((step.seq, step.prev_seq), (3, 2));
    assert_eq!(step.kind, "refund");
    assert_eq!(
        (step.from_status.as_str(), step.to_status.as_str()),
        ("captured", "captured")
    );
    assert_eq!(step.amount, 3_000);
    assert_eq!(
        (step.prev_refundable, step.refundable),
        (AUTHORIZED, AUTHORIZED - 3_000)
    );
    let record = sqlx::query!(
        "SELECT payment_id, amount, refunded_at, ledger_transaction_id \
           FROM payment_refunds WHERE transition_id = $1",
        command.step.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(record.payment_id, payment.get());
    assert_eq!(record.amount, 3_000);
    assert_eq!(record.refunded_at, command.refunded_at.get());
    assert_eq!(record.ledger_transaction_id, command.posting.get());
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn after_a_full_refund_the_merchant_owes_the_platform_its_fee(pool: PgPool) -> TestResult {
    // ADR-0020's consequence: the PSP's receivable is back at 0, the merchant's
    // payable stands at +F, and the fee revenue at -F.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    refund(&app, &refund_command(1, payment, 3_000)).await?;
    assert_eq!(
        refund(&app, &refund_command(2, payment, AUTHORIZED - 3_000)).await?,
        Outcome::Applied(Recorded {
            seq: 4,
            from: PaymentStatus::Captured,
            to: PaymentStatus::Refunded,
            refundable: 0,
        })
    );
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Refunded);
    assert_eq!(balances(&app, merchant).await?, [0, FEE, -FEE]);
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_settled_payment_can_be_refunded(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    settle(&app, &settle_command(1, payment)).await?;
    assert_eq!(
        refund(&app, &refund_command(1, payment, 3_000)).await?,
        Outcome::Applied(Recorded {
            seq: 4,
            from: PaymentStatus::Settled,
            to: PaymentStatus::Settled,
            refundable: AUTHORIZED - 3_000,
        })
    );
    assert_eq!(
        refund(&app, &refund_command(2, payment, AUTHORIZED - 3_000)).await?,
        Outcome::Applied(Recorded {
            seq: 5,
            from: PaymentStatus::Settled,
            to: PaymentStatus::Refunded,
            refundable: 0,
        })
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_amount_is_at_most_what_is_left(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    let above = |n, minor, left| {
        move |err: RefundError| {
            let refused = matches!(
                err,
                RefundError::Refused(Refused::AboveLimit { amount, limit })
                    if amount == minor && limit == left
            );
            assert!(refused, "command {n}: {err:?}");
        }
    };
    let before = postings(&pool).await?;
    above(1, AUTHORIZED + 1, AUTHORIZED)(
        refund(&app, &refund_command(1, payment, AUTHORIZED + 1))
            .await
            .unwrap_err(),
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(postings(&pool).await?, before);
    // The limit falls with each refund.
    refund(&app, &refund_command(2, payment, 5_000)).await?;
    above(3, 3_001, 3_000)(
        refund(&app, &refund_command(3, payment, 3_001))
            .await
            .unwrap_err(),
    );
    assert_eq!(refundable_of(&app, payment).await?, 3_000);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn only_a_captured_or_settled_payment_can_be_refunded(pool: PgPool) -> TestResult {
    // The other statuses refuse it whatever the amount (ADR-0016). An amount at
    // or above the limit, here 0, is tried as a final refund.
    let books = books(&pool).await?;
    let app = runtime_pool(&pool).await?;
    let mut n = 0;
    for status in [
        PaymentStatus::Created,
        PaymentStatus::Authorized,
        PaymentStatus::Failed,
        PaymentStatus::Voided,
        PaymentStatus::Refunded,
    ] {
        n += 1;
        let newest = committed_at(&pool, &books, status).await?;
        let payment = PaymentId::try_from(newest.payment_id)?;
        let before = postings(&pool).await?;
        let err = refund(&app, &refund_command(n, payment, 1))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RefundError::Refused(Refused::Illegal(IllegalTransition {
                    status: got,
                    event: Event::FinalRefund,
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
    let err = refund(&app, &refund_command(1, unknown, 1))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RefundError::NotFound { payment } if payment == unknown),
        "{err:?}"
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_of_an_older_refund_gets_its_own_reply(pool: PgPool) -> TestResult {
    // Two refunds of the same size, and a retry of the first after the second.
    // The state would refuse it, or take it for a new refund, so identity decides,
    // on the one connection of this pool (ADR-0019).
    let app = runtime_pool_of_one(&pool).await?;
    let merchant = merchant_id(1);
    register_merchant(&app, merchant, fee_rate()).await?;
    let payment = captured(&app, merchant, 1).await?;
    let first = refund_command(1, payment, 3_000);
    let first_reply = Recorded {
        seq: 3,
        from: PaymentStatus::Captured,
        to: PaymentStatus::Captured,
        refundable: AUTHORIZED - 3_000,
    };
    assert_eq!(refund(&app, &first).await?, Outcome::Applied(first_reply));
    refund(&app, &refund_command(2, payment, 3_000)).await?;
    let money = balances(&app, merchant).await?;
    assert_eq!(
        refund(&app, &first).await?,
        Outcome::AlreadyApplied(first_reply)
    );
    assert_eq!(balances(&app, merchant).await?, money);
    assert_eq!(steps(&pool, payment).await?, 5);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_changed_retry_is_a_mismatch_for_each_field(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    refund(&app, &refund_command(1, payment, 3_000)).await?;
    // Another refund of the same size for the same merchant: its posting has the
    // very lines of the first, so only the record's link can tell them apart.
    let twin = captured(&app, merchant, 2).await?;
    refund(&app, &refund_command(2, twin, 3_000)).await?;
    let other = captured(&app, merchant, 3).await?;
    let money = balances(&app, merchant).await?;
    let later = BusinessTime::new("2026-01-20T12:00:00Z".parse()?)?;
    for changed in [
        Refund {
            amount: PositiveAmount::new(2_999)?,
            ..refund_command(1, payment, 3_000)
        },
        Refund {
            refunded_at: later,
            ..refund_command(1, payment, 3_000)
        },
        Refund {
            posting: transaction_id(99),
            ..refund_command(1, payment, 3_000)
        },
        Refund {
            posting: transaction_id(1002),
            ..refund_command(1, payment, 3_000)
        },
        Refund {
            payment: other,
            ..refund_command(1, payment, 3_000)
        },
    ] {
        let err = refund(&app, &changed).await.unwrap_err();
        let RefundError::Mismatch { step } = err else {
            panic!("expected Mismatch for {changed:?}, got {err:?}");
        };
        assert_eq!(step, changed.step.get());
    }
    assert_eq!(refundable_of(&app, other).await?, AUTHORIZED);
    assert_eq!(balances(&app, merchant).await?, money);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn of_many_refunds_of_one_exactly_what_is_left_apply(pool: PgPool) -> TestResult {
    // ADR-0019's race. Twelve refunds of 1 against 5: they queue on the lock, each
    // reads what the one before it left, exactly 5 apply, and the rest are told so
    // by the table, with no database error.
    let (app, merchant) = app_with_merchant(&pool).await?;
    let left = 5;
    let payment = captured_for(&app, merchant, 1, left).await?;
    let commands: Vec<Refund> = (1..=12).map(|n| refund_command(n, payment, 1)).collect();
    let results = join_all(commands.iter().map(|command| refund(&app, command))).await;
    let mut replies = Vec::new();
    for result in results {
        match result {
            Ok(Outcome::Applied(recorded)) => replies.push(recorded.refundable),
            Err(RefundError::Refused(Refused::Illegal(illegal))) => {
                assert_eq!(illegal.status, PaymentStatus::Refunded);
            }
            other => panic!("{other:?}"),
        }
    }
    replies.sort_unstable();
    assert_eq!(replies, [0, 1, 2, 3, 4]);
    assert_eq!(status_of(&app, payment).await?, PaymentStatus::Refunded);
    assert_eq!(refundable_of(&app, payment).await?, 0);
    // A fee of 2.5% of 5 rounds to 0, so every account is back at 0.
    assert_eq!(balances(&app, merchant).await?, [0, 0, 0]);
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_partial_refund_racing_a_settle_applies_both(pool: PgPool) -> TestResult {
    // Neither refuses the other, so whichever gets the lock first, both apply and
    // the payment ends settled with less to refund.
    let (app, merchant) = app_with_merchant(&pool).await?;
    for n in 1..=8 {
        let payment = captured(&app, merchant, n).await?;
        let refunding = refund_command(n, payment, 3_000);
        let settling = settle_command(n, payment);
        let (refunded, settled) = join(refund(&app, &refunding), settle(&app, &settling)).await;
        assert!(matches!(refunded?, Outcome::Applied(_)));
        assert!(matches!(settled?, Outcome::Applied(_)));
        assert_eq!(status_of(&app, payment).await?, PaymentStatus::Settled);
        assert_eq!(refundable_of(&app, payment).await?, AUTHORIZED - 3_000);
    }
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_final_refund_racing_a_settle_ends_refunded_either_way(pool: PgPool) -> TestResult {
    // If the refund goes first, a refunded payment refuses the settle. If the settle
    // goes first, the refund still applies. The money is the same in both orders.
    let (app, merchant) = app_with_merchant(&pool).await?;
    for n in 1..=8 {
        let payment = captured(&app, merchant, n).await?;
        let refunding = refund_command(n, payment, AUTHORIZED);
        let settling = settle_command(n, payment);
        let (refunded, settled) = join(refund(&app, &refunding), settle(&app, &settling)).await;
        assert!(matches!(refunded?, Outcome::Applied(_)));
        match settled {
            Ok(Outcome::Applied(_)) => {}
            Err(StepError::Illegal(illegal)) => assert_eq!(
                illegal,
                IllegalTransition {
                    status: PaymentStatus::Refunded,
                    event: Event::Settle,
                }
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(status_of(&app, payment).await?, PaymentStatus::Refunded);
    }
    // Eight full refunds: each leaves the merchant owing its fee.
    assert_eq!(balances(&app, merchant).await?, [0, 8 * FEE, -8 * FEE]);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_posting_that_a_trigger_corrupts_fails_at_commit(pool: PgPool) -> TestResult {
    // Still balanced, so the ledger's own check passes: only the check of the
    // record against its posting can notice, and LG006 proves a rollback. The
    // step, the posting and the record all go (ADR-0014, ADR-0021).
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    let before = postings(&pool).await?;
    sqlx::query!(
        "CREATE FUNCTION grow_a_refund() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
             NEW.amount := NEW.amount + sign(NEW.amount)::bigint; \
             RETURN NEW; \
         END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER grow_a_refund BEFORE INSERT ON ledger_entries \
         FOR EACH ROW EXECUTE FUNCTION grow_a_refund()"
    )
    .execute(&pool)
    .await?;
    let err = refund(&app, &refund_command(1, payment, 3_000))
        .await
        .unwrap_err();
    let RefundError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(
        source,
        "LG006",
        "payment_refunds_commit_check",
        "payment_refunds",
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(postings(&pool).await?, before);
    assert_eq!(refundable_of(&app, payment).await?, AUTHORIZED);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_loses_fails_at_commit(pool: PgPool) -> TestResult {
    let (app, merchant) = app_with_merchant(&pool).await?;
    let payment = captured(&app, merchant, 1).await?;
    let before = postings(&pool).await?;
    sqlx::query!(
        "CREATE FUNCTION lose_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER lose_record BEFORE INSERT ON payment_refunds \
         FOR EACH ROW EXECUTE FUNCTION lose_row()"
    )
    .execute(&pool)
    .await?;
    let err = refund(&app, &refund_command(1, payment, 3_000))
        .await
        .unwrap_err();
    let RefundError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_guard(
        source,
        "LG004",
        "payment_transitions_commit_check",
        "payment_transitions",
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(postings(&pool).await?, before);
    app.close().await;
    Ok(())
}
