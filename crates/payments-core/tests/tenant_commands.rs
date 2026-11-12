// Author: Nicola Antonello

//! The commands as a merchant's requests run them (ADR-0025): logged in as the
//! tenant role, which sees nothing until a transaction names its merchant.
//!
//! The merchants are registered by the system role, because the tenant role cannot
//! create one. Most of these tests use a pool of one connection: it shows that no
//! path needs two at once, and it hands each merchant the connection that the last
//! one used, which is what a setting left behind would leak through. A merchant asks
//! the PSP for what only the PSP's word does, and the system role applies that word,
//! as the server does (ADR-0034, ADR-0043).

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, REQUESTED, Step, balance_of, books, committed_at, create_command, eur,
    fee_rate, requests_of, steps, unbalanced_currencies, uncaused_postings,
};
use common::psp::{PspSide, decided, hand_over_and_step};
use common::{
    merchant_id, request_id, runtime_pool, runtime_transaction, tenant_pool, tenant_pool_of_one,
    transition_id, wait_for_lock_wait,
};
use futures_util::future::{join, join_all};
use mock_psp::wire::Detail;
use payments_core::accounts::{AccountRef, Psp};
use payments_core::db::commit;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::PaymentStatus;
use payments_core::money::{BasisPoints, PositiveAmount};
use payments_core::payments::{
    Authorization, CreateError, CreatePayment, Outcome, Payment, Recorded, StepCommand, StepError,
    cancel, create_payment, find_payment, merchant_fee_rate, register_merchant,
};
use payments_core::requests::{
    Ask, RecordRequest, RequestError, RequestOutcome, RequestRecorded, record_request,
};
use payments_core::tenancy::Tenant;
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// Registers these merchants, as the system role.
async fn register(pool: &PgPool, merchants: &[MerchantId]) -> TestResult {
    let system = runtime_pool(pool).await?;
    for merchant in merchants {
        register_merchant(&system, *merchant, fee_rate()).await?;
    }
    system.close().await;
    Ok(())
}

/// How many payments a session sees with no merchant named. A setting left behind on
/// the connection would show here: the tenant role sees none without one.
async fn visible_with_no_merchant(tenant: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payments"#)
        .fetch_one(tenant)
        .await
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

/// A step of the merchant's own, with step ID `n`.
fn plain(n: u32, payment: PaymentId) -> StepCommand {
    StepCommand {
        step: transition_id(n),
        payment,
    }
}

/// Creates payment `n`, as this merchant.
async fn created(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    Ok(command.id)
}

/// Creates payment `n` as this merchant, which asks for its authorization, and the PSP
/// authorizes it.
async fn authorized(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(me, merchant, n).await?;
    psp.authorized(me, payment).await;
    Ok(payment)
}

/// Creates payment `n` and asks for its authorization, as this merchant: two writes of
/// its own.
async fn asked(me: Tenant<'_>, merchant: MerchantId, n: u32) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(me, merchant, n).await?;
    record_request(me, &request(n, payment, Ask::Authorize(amount(REQUESTED)))).await?;
    Ok(payment)
}

#[sqlx::test]
async fn a_merchant_reads_the_fee_rate_that_the_operator_set_for_it(pool: PgPool) -> TestResult {
    let (a, b) = (merchant_id(1), merchant_id(2));
    let (rate_a, rate_b) = (BasisPoints::new(250)?, BasisPoints::new(300)?);
    let system = runtime_pool(&pool).await?;
    register_merchant(&system, a, rate_a).await?;
    register_merchant(&system, b, rate_b).await?;
    let tenant = tenant_pool_of_one(&pool).await?;

    assert_eq!(
        merchant_fee_rate(Tenant::new(&tenant, a)).await?,
        Some(rate_a)
    );
    assert_eq!(
        merchant_fee_rate(Tenant::new(&tenant, b)).await?,
        Some(rate_b)
    );
    assert_eq!(
        merchant_fee_rate(Tenant::new(&tenant, merchant_id(3))).await?,
        None
    );
    // The system role skips the policy, so this shows that the query names its
    // merchant too (ADR-0025): the first of two rows would be the wrong rate.
    assert_eq!(
        merchant_fee_rate(Tenant::new(&system, b)).await?,
        Some(rate_b)
    );
    tenant.close().await;
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_fee_rate_that_cannot_be_read_is_an_error_and_not_no_merchant(
    pool: PgPool,
) -> TestResult {
    let tenant = tenant_pool(&pool).await?;
    tenant.close().await;
    let read = merchant_fee_rate(Tenant::new(&tenant, merchant_id(1))).await;
    assert!(read.is_err(), "{read:?}");
    Ok(())
}

#[sqlx::test]
async fn a_merchant_runs_a_payment_through_its_life(pool: PgPool) -> TestResult {
    // If any statement of any command forgot to name the merchant, the tenant role
    // would see nothing there, and the command would stop.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;

    let command = create_command(1, merchant);
    let payment = command.id;
    assert_eq!(
        create_payment(me, &command).await?,
        Outcome::Applied(Recorded {
            seq: 0,
            from: PaymentStatus::Created,
            to: PaymentStatus::Created,
            refundable: 0,
        })
    );
    // Each request moves nothing until the PSP's word, applied by the system role, does.
    for (n, ask, status, refundable) in [
        (
            1,
            Ask::Authorize(amount(REQUESTED)),
            PaymentStatus::Authorized,
            0,
        ),
        (
            2,
            Ask::Capture(amount(AUTHORIZED)),
            PaymentStatus::Captured,
            8_000,
        ),
        (
            3,
            Ask::Refund(amount(3_000)),
            PaymentStatus::Captured,
            5_000,
        ),
    ] {
        let asked = request(n, payment, ask);
        assert_eq!(
            record_request(me, &asked).await?,
            RequestOutcome::Recorded(RequestRecorded {
                request: request_id(n).get(),
                payment,
            })
        );
        hand_over_and_step(&mut psp.sender, &psp.system, &asked.request).await;
        match ask {
            // The PSP authorizes less than asked (ADR-0016).
            Ask::Authorize(_) => psp.fake.approve_for(request_id(n).get(), AUTHORIZED),
            _ => psp.fake.approve(request_id(n).get()),
        }
        psp.caught_up(payment).await;
        let stored = find_payment(me, payment).await?.ok_or("stored")?;
        assert_eq!((stored.status, stored.refundable), (status, refundable));
    }
    let Detail::Authorized { expires_at, .. } = decided(&psp.fake, payment, &request_id(1)) else {
        panic!("the fake authorized it");
    };
    assert_eq!(
        find_payment(me, payment).await?,
        Some(Payment {
            merchant,
            psp: Psp::Mock,
            currency: eur(),
            amount: PositiveAmount::new(10_000)?,
            fee_rate: fee_rate(),
            status: PaymentStatus::Captured,
            refundable: 5_000,
            authorization: Some(Authorization {
                amount: PositiveAmount::new(AUTHORIZED)?,
                expires_at: BusinessTime::new(expires_at)?,
            }),
        })
    );

    // The money moved, in the books that the system role reads.
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        5_000
    );
    assert_eq!(
        balance_of(&pool, AccountRef::MerchantPayable(merchant, eur())).await?,
        FEE - AUTHORIZED + 3_000
    );
    assert_eq!(
        balance_of(&pool, AccountRef::FeeRevenue(eur())).await?,
        -FEE
    );
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_moves_that_carry_nothing_reach_a_merchant(pool: PgPool) -> TestResult {
    // The merchant cancels one payment and asks for another's void, as the tenant role,
    // and the PSP's decline, lapse and settlement reach the payments that it reads.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;

    let cancelled = created(me, merchant, 1).await?;
    cancel(me, &plain(101, cancelled)).await?;

    let declined = created(me, merchant, 2).await?;
    let authorize = request(2, declined, Ask::Authorize(amount(REQUESTED)));
    psp.sent(me, &authorize).await;
    psp.fake.decline(authorize.request.get());
    psp.caught_up(declined).await;

    let voided = authorized(&mut psp, me, merchant, 3).await?;
    psp.applied(me, voided, Ask::Void).await;

    let lapsed = authorized(&mut psp, me, merchant, 4).await?;
    psp.fake.expire(lapsed.get());
    psp.caught_up(lapsed).await;

    let settled = authorized(&mut psp, me, merchant, 5).await?;
    psp.captured(me, settled).await;
    psp.fake.settle(settled.get());
    psp.caught_up(settled).await;

    for (payment, status) in [
        (cancelled, PaymentStatus::Voided),
        (declined, PaymentStatus::Failed),
        (voided, PaymentStatus::Voided),
        (lapsed, PaymentStatus::Voided),
        (settled, PaymentStatus::Settled),
    ] {
        let stored = find_payment(me, payment).await?.ok_or("stored")?;
        assert_eq!(stored.status, status);
    }
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn another_merchants_payment_is_not_found(pool: PgPool) -> TestResult {
    // A command reads through the policies, so a stranger's payment looks like no
    // payment at all, and an ID's existence does not leak.
    let (mine, theirs) = (merchant_id(1), merchant_id(2));
    register(&pool, &[mine, theirs]).await?;
    let tenant = tenant_pool(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let them = Tenant::new(&tenant, theirs);
    let me = Tenant::new(&tenant, mine);

    let payment = authorized(&mut psp, them, theirs, 1).await?;
    psp.captured(them, payment).await;
    assert_eq!(steps(&pool, payment).await?, 3);
    let held = requests_of(&pool, payment).await?;

    assert_eq!(find_payment(me, payment).await?, None);
    // A refund would be recorded and an authorization refused, were it mine: each
    // answers as no payment.
    for (n, ask) in [
        (11, Ask::Authorize(amount(1))),
        (12, Ask::Capture(amount(1))),
        (13, Ask::Refund(amount(1_000))),
        (14, Ask::Void),
    ] {
        let err = record_request(me, &request(n, payment, ask))
            .await
            .unwrap_err();
        assert!(matches!(err, RequestError::NotFound { .. }), "{err:?}");
    }
    let err = cancel(me, &plain(201, payment)).await.unwrap_err();
    assert!(matches!(err, StepError::NotFound { .. }), "{err:?}");

    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(requests_of(&pool, payment).await?, held);
    psp.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchant_cannot_create_a_payment_for_another(pool: PgPool) -> TestResult {
    // The stranger's row is invisible, so it reads as unknown, before any account
    // opens or anything is written.
    let (mine, theirs) = (merchant_id(1), merchant_id(2));
    register(&pool, &[mine, theirs]).await?;
    let tenant = tenant_pool(&pool).await?;
    let command: CreatePayment = create_command(1, theirs);

    let err = create_payment(Tenant::new(&tenant, mine), &command)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CreateError::UnknownMerchant { merchant } if merchant == theirs),
        "{err:?}"
    );
    let stored = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM payments) AS "payments!",
                  (SELECT count(*) FROM accounts) AS "accounts!""#
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((stored.payments, stored.accounts), (0, 0));
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_gets_the_stored_reply_and_a_changed_one_a_mismatch(pool: PgPool) -> TestResult {
    // Identity first still holds through the policies: the read that finds the step
    // runs with the merchant named.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    let other = created(me, merchant, 2).await?;
    let reply = Recorded {
        seq: 1,
        from: PaymentStatus::Created,
        to: PaymentStatus::Voided,
        refundable: 0,
    };

    assert_eq!(
        cancel(me, &plain(101, payment)).await?,
        Outcome::Applied(reply)
    );
    assert_eq!(
        cancel(me, &plain(101, payment)).await?,
        Outcome::AlreadyApplied(reply)
    );

    // The same step's ID, sent for another payment of the merchant's.
    let err = cancel(me, &plain(101, other)).await.unwrap_err();
    assert!(matches!(err, StepError::Mismatch { .. }), "{err:?}");
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(steps(&pool, other).await?, 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_apply_once_for_a_merchant(pool: PgPool) -> TestResult {
    // Copies of a create meet a duplicate key, and look their step up through the
    // policies, so that lookup must name the merchant too. Copies of a cancel queue on
    // the lock, and each reads what the one before it committed.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool(&pool).await?;
    let me = Tenant::new(&tenant, merchant);

    let creates: Vec<CreatePayment> = (0..8).map(|_| create_command(1, merchant)).collect();
    let results = join_all(creates.iter().map(|copy| create_payment(me, copy))).await;
    let mut applied = 0;
    for result in results {
        if let Outcome::Applied(_) = result? {
            applied += 1;
        }
    }
    assert_eq!(applied, 1);
    let payment = creates[0].id;

    let cancels: Vec<StepCommand> = (0..8).map(|_| plain(101, payment)).collect();
    let results = join_all(cancels.iter().map(|copy| cancel(me, copy))).await;
    let mut applied = 0;
    for result in results {
        if let Outcome::Applied(_) = result? {
            applied += 1;
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, payment).await?, 2);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_duplicate_key_is_settled_for_a_merchant_too(pool: PgPool) -> TestResult {
    // A writer that skips the lock stores the command's own ID after the command's
    // read, so the command meets a duplicate key and looks its step up under the lock
    // again (ADR-0019). That lookup must name the merchant, or the merchant's own
    // payment would read as not found.
    let books = books(&pool).await?;
    let first = committed_at(&pool, &books, PaymentStatus::Created).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, MerchantId::try_from(books.merchant)?);
    let command = plain(1, PaymentId::try_from(first.payment_id)?);
    let mut writer = runtime_transaction(&pool).await?;
    let stored = Step {
        id: command.step.get(),
        ..first.then("cancel", "voided", 0, 0)
    };
    stored.insert(&mut writer).await?;

    let (answer, committed) = join(cancel(me, &command), async {
        wait_for_lock_wait(&pool, "transactionid").await?;
        commit(writer).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    committed?;
    assert_eq!(
        answer?,
        Outcome::AlreadyApplied(Recorded {
            seq: 1,
            from: PaymentStatus::Created,
            to: PaymentStatus::Voided,
            refundable: 0,
        })
    );
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_connection_keeps_no_merchant_once_a_command_ends(pool: PgPool) -> TestResult {
    // The one connection goes back to the pool after each call. A setting that outlived
    // its transaction would let the next request see this merchant's rows, whether
    // the command applied, read, or was refused.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, merchant);

    let command = create_command(1, merchant);
    create_payment(me, &command).await?;
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);

    assert!(find_payment(me, command.id).await?.is_some());
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);

    record_request(
        me,
        &request(1, command.id, Ask::Authorize(amount(REQUESTED))),
    )
    .await?;
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);

    let refused = record_request(me, &request(2, command.id, Ask::Capture(amount(1))))
        .await
        .unwrap_err();
    assert!(matches!(refused, RequestError::Refused(_)), "{refused:?}");
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);

    let refused = cancel(me, &plain(301, command.id)).await.unwrap_err();
    assert!(
        matches!(refused, StepError::AuthorizationRequested),
        "{refused:?}"
    );
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchant_sees_what_it_wrote_and_the_next_one_does_not(pool: PgPool) -> TestResult {
    // Two merchants taking turns on one connection, which is what a pool does to them.
    let (a, b) = (merchant_id(1), merchant_id(2));
    register(&pool, &[a, b]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let (of_a, of_b) = (Tenant::new(&tenant, a), Tenant::new(&tenant, b));

    let payment_of_a = asked(of_a, a, 1).await?;
    assert_eq!(find_payment(of_b, payment_of_a).await?, None);
    let payment_of_b = asked(of_b, b, 2).await?;
    assert_eq!(find_payment(of_a, payment_of_b).await?, None);
    assert!(find_payment(of_a, payment_of_a).await?.is_some());
    assert!(find_payment(of_b, payment_of_b).await?.is_some());
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_system_role_catches_up_any_merchants_payment(pool: PgPool) -> TestResult {
    // The PSP's word reaches a merchant's payment through the system role, which names no
    // merchant (ADR-0025, ADR-0031).
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = authorized(&mut psp, me, merchant, 1).await?;

    psp.captured(me, payment).await;
    let stored = find_payment(&psp.system, payment).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Captured);
    psp.close().await;
    tenant.close().await;
    Ok(())
}
