// Author: Nicola Antonello

//! Refunding a payment (ADR-0016, ADR-0020). What can be refunded falls with each refund,
//! so a refund's limit depends on every refund before it, and on the refunds that wait for
//! the PSP when the merchant asks (ADR-0039). The PSP's refund reaches the payment through
//! a catch-up (ADR-0031, ADR-0041). The path that every command shares is tried in
//! `payment_steps.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, balance_of, books, committed_at, create_command, eur, fee_rate, requests_of,
    status_of, steps, unbalanced_currencies, uncaused_postings,
};
use common::psp::{PspSide, client_of, decided, hand_over_and_step};
use common::{assert_guard, merchant_id, request_id, runtime_pool, tenant_pool};
use futures_util::future::join_all;
use mock_psp::wire::Detail;
use payments_core::accounts::{AccountRef, Psp};
use payments_core::catch_up::{CatchUpError, CaughtUp, catch_up};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, find_payment, register_merchant};
use payments_core::requests::{
    Ask, RecordRequest, Refusal, RequestError, RequestOutcome, record_request,
};
use payments_core::tenancy::Tenant;
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

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

/// Takes payment `n` to `captured` through the PSP, for [`AUTHORIZED`] with a fee of
/// [`FEE`].
async fn captured(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    captured_for(psp, me, merchant, n, AUTHORIZED).await
}

/// Takes payment `n` to `captured` through the PSP, for `minor`, at a fee rate of 2.5%.
async fn captured_for(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
    minor: i64,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    psp.authorized(me, command.id).await;
    psp.applied(me, command.id, Ask::Capture(amount(minor)))
        .await;
    Ok(command.id)
}

/// Request `n`: the refund of this much of a payment.
fn refund_request(n: u32, payment: PaymentId, minor: i64) -> RecordRequest {
    RecordRequest {
        request: request_id(n),
        payment,
        ask: Ask::Refund(amount(minor)),
    }
}

/// Asks for request `n`, a refund of `minor`, as its merchant, and the PSP approves it.
/// The catch-up that applies it is the test's.
async fn refund_approved(
    psp: &mut PspSide,
    me: Tenant<'_>,
    n: u32,
    payment: PaymentId,
    minor: i64,
) {
    let refunding = refund_request(n, payment, minor);
    psp.sent(me, &refunding).await;
    psp.fake.approve(refunding.request.get());
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
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = captured(&mut psp, me, merchant, 1).await?;
    refund_approved(&mut psp, me, 1, payment, 3_000).await;
    psp.caught_up(payment).await;
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Captured
    );
    assert_eq!(
        refundable_of(&psp.system, payment).await?,
        AUTHORIZED - 3_000
    );
    // ADR-0020's lines, on top of the capture's: the merchant pays it all.
    assert_eq!(
        balances(&psp.system, merchant).await?,
        [AUTHORIZED - 3_000, FEE - AUTHORIZED + 3_000, -FEE]
    );
    let step = sqlx::query!(
        "SELECT id, prev_seq, kind, from_status, to_status, amount, prev_refundable, refundable \
           FROM payment_transitions WHERE payment_id = $1 AND seq = 3",
        payment.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(step.prev_seq, 2);
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
        "SELECT payment_id, amount, refunded_at FROM payment_refunds WHERE transition_id = $1",
        step.id
    )
    .fetch_one(&pool)
    .await?;
    // The time comes from the PSP's fact (ADR-0031).
    let Detail::Refunded { refunded_at, .. } = decided(&psp.fake, payment, &request_id(1)) else {
        panic!("the fake refunded it");
    };
    assert_eq!(record.payment_id, payment.get());
    assert_eq!(record.amount, 3_000);
    assert_eq!(record.refunded_at, BusinessTime::new(refunded_at)?.get());
    // The capture's posting and this one, each with a record that caused it (ADR-0021).
    assert_eq!(postings(&pool).await?, 2);
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn after_a_full_refund_the_merchant_owes_the_platform_its_fee(pool: PgPool) -> TestResult {
    // ADR-0020's consequence: the PSP's receivable is back at 0, the merchant's
    // payable stands at +F, and the fee revenue at -F.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = captured(&mut psp, me, merchant, 1).await?;
    psp.applied(me, payment, Ask::Refund(amount(3_000))).await;
    psp.applied(me, payment, Ask::Refund(amount(AUTHORIZED - 3_000)))
        .await;
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Refunded
    );
    assert_eq!(refundable_of(&psp.system, payment).await?, 0);
    assert_eq!(balances(&psp.system, merchant).await?, [0, FEE, -FEE]);
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_settled_payment_can_be_refunded(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = captured(&mut psp, me, merchant, 1).await?;
    psp.fake.settle(payment.get());
    psp.caught_up(payment).await;
    for (minor, status, left) in [
        (3_000, PaymentStatus::Settled, AUTHORIZED - 3_000),
        (AUTHORIZED - 3_000, PaymentStatus::Refunded, 0),
    ] {
        psp.applied(me, payment, Ask::Refund(amount(minor))).await;
        assert_eq!(status_of(&psp.system, payment).await?, status, "{minor}");
        assert_eq!(refundable_of(&psp.system, payment).await?, left, "{minor}");
    }
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_amount_is_at_most_what_is_left(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = captured(&mut psp, me, merchant, 1).await?;
    let above = |n, minor, left| {
        move |err: RequestError| {
            let refused = matches!(
                err,
                RequestError::Refused(Refusal::Refused(Refused::AboveLimit { amount, limit }))
                    if amount == minor && limit == left
            );
            assert!(refused, "request {n}: {err:?}");
        }
    };
    let before = postings(&pool).await?;
    above(1, AUTHORIZED + 1, AUTHORIZED)(
        record_request(me, &refund_request(1, payment, AUTHORIZED + 1))
            .await
            .unwrap_err(),
    );
    assert_eq!(steps(&pool, payment).await?, 3);
    // The authorization's request and the capture's.
    assert_eq!(requests_of(&pool, payment).await?, 2);
    assert_eq!(postings(&pool).await?, before);
    // The limit falls with each refund.
    psp.applied(me, payment, Ask::Refund(amount(5_000))).await;
    above(3, 3_001, 3_000)(
        record_request(me, &refund_request(3, payment, 3_001))
            .await
            .unwrap_err(),
    );
    assert_eq!(refundable_of(&psp.system, payment).await?, 3_000);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn only_a_captured_or_settled_payment_can_be_refunded(pool: PgPool) -> TestResult {
    // The other statuses refuse it whatever the amount (ADR-0016). An amount at
    // or above the limit, here 0, is tried as a final refund.
    let books = books(&pool).await?;
    let tenant = tenant_pool(&pool).await?;
    let me = Tenant::new(&tenant, MerchantId::try_from(books.merchant)?);
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
        let err = record_request(me, &refund_request(n, payment, 1))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RequestError::Refused(Refusal::Refused(Refused::Illegal(IllegalTransition {
                    status: got,
                    event: Event::FinalRefund,
                }))) if got == status
            ),
            "{status}: {err:?}"
        );
        assert_eq!(requests_of(&pool, payment).await?, 0);
        assert_eq!(steps(&pool, payment).await?, i64::from(newest.seq) + 1);
        assert_eq!(postings(&pool).await?, before);
    }
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn of_many_refunds_of_one_exactly_what_is_left_apply(pool: PgPool) -> TestResult {
    // ADR-0019's race, on the merchant's side (ADR-0039). Twelve refunds of 1 against
    // 5: they queue on the lock, each counts the refunds in flight before it, exactly 5
    // are recorded, and the rest are told so by the limit. The PSP refunds those 5.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let left = 5;
    let payment = captured_for(&mut psp, me, merchant, 1, left).await?;
    let requests: Vec<RecordRequest> = (1..=12).map(|n| refund_request(n, payment, 1)).collect();
    let results = join_all(requests.iter().map(|request| record_request(me, request))).await;
    let mut recorded = Vec::new();
    for (request, result) in requests.iter().zip(results) {
        match result {
            Ok(RequestOutcome::Recorded(_)) => recorded.push(request),
            Err(RequestError::Refused(Refusal::Refused(Refused::AboveLimit {
                amount: 1,
                limit: 0,
            }))) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(recorded.len(), 5);
    for request in recorded {
        hand_over_and_step(&mut psp.sender, &psp.system, &request.request).await;
        psp.fake.approve(request.request.get());
    }
    assert_eq!(
        catch_up(&psp.system, &client_of(&psp.fake), payment).await?,
        CaughtUp::Current { applied: 5 }
    );
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Refunded
    );
    assert_eq!(refundable_of(&psp.system, payment).await?, 0);
    // A fee of 2.5% of 5 rounds to 0, so every account is back at 0.
    assert_eq!(balances(&psp.system, merchant).await?, [0, 0, 0]);
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_partial_refund_and_a_settle_apply_in_either_order(pool: PgPool) -> TestResult {
    // Neither refuses the other, so in whichever order the PSP gives them, both apply
    // and the payment ends settled with less to refund.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    for (n, settled_first) in [(1, true), (2, false)] {
        let payment = captured(&mut psp, me, merchant, n).await?;
        if settled_first {
            psp.fake.settle(payment.get());
        }
        refund_approved(&mut psp, me, n, payment, 3_000).await;
        if !settled_first {
            psp.fake.settle(payment.get());
        }
        assert_eq!(
            catch_up(&psp.system, &client_of(&psp.fake), payment).await?,
            CaughtUp::Current { applied: 2 }
        );
        assert_eq!(
            status_of(&psp.system, payment).await?,
            PaymentStatus::Settled
        );
        assert_eq!(
            refundable_of(&psp.system, payment).await?,
            AUTHORIZED - 3_000
        );
    }
    // The same money in both orders.
    assert_eq!(
        balances(&psp.system, merchant).await?,
        [
            2 * (AUTHORIZED - 3_000),
            2 * (FEE - AUTHORIZED + 3_000),
            -2 * FEE
        ]
    );
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_final_refund_and_a_settle_end_refunded_in_either_order(pool: PgPool) -> TestResult {
    // If the refund comes first, the settle after it takes no step (ADR-0016). If the
    // settle comes first, the refund still applies. The money is the same in both
    // orders; `catching_up.rs` checks the steps.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    for (n, settled_first) in [(1, true), (2, false)] {
        let payment = captured(&mut psp, me, merchant, n).await?;
        if settled_first {
            psp.fake.settle(payment.get());
        }
        refund_approved(&mut psp, me, n, payment, AUTHORIZED).await;
        if !settled_first {
            psp.fake.settle(payment.get());
        }
        assert_eq!(
            catch_up(&psp.system, &client_of(&psp.fake), payment).await?,
            CaughtUp::Current { applied: 2 }
        );
        assert_eq!(
            status_of(&psp.system, payment).await?,
            PaymentStatus::Refunded
        );
    }
    // Two full refunds: each leaves the merchant owing its fee.
    assert_eq!(
        balances(&psp.system, merchant).await?,
        [0, 2 * FEE, -2 * FEE]
    );
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_posting_that_a_trigger_corrupts_fails_at_commit(pool: PgPool) -> TestResult {
    // Still balanced, so the ledger's own check passes: only the check of the
    // record against its posting can notice, and LG006 proves a rollback. The
    // step, the posting and the record all go (ADR-0014, ADR-0021).
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = captured(&mut psp, me, merchant, 1).await?;
    refund_approved(&mut psp, me, 1, payment, 3_000).await;
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
    let err = catch_up(&psp.system, &client_of(&psp.fake), payment)
        .await
        .unwrap_err();
    let CatchUpError::Invariant { source, .. } = err else {
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
    assert_eq!(refundable_of(&psp.system, payment).await?, AUTHORIZED);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_loses_fails_at_commit(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = captured(&mut psp, me, merchant, 1).await?;
    refund_approved(&mut psp, me, 1, payment, 3_000).await;
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
    let err = catch_up(&psp.system, &client_of(&psp.fake), payment)
        .await
        .unwrap_err();
    let CatchUpError::Invariant { source, .. } = err else {
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
    tenant.close().await;
    psp.close().await;
    Ok(())
}
