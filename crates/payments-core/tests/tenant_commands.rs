// Author: Nicola Antonello

//! The commands as a merchant's requests run them (ADR-0025): logged in as the
//! tenant role, which sees nothing until a transaction names its merchant.
//!
//! The merchants are registered by the system role, because the tenant role cannot
//! create one. Most of these tests use a pool of one connection: it shows that no
//! path needs two at once, and it hands each merchant the connection that the last
//! one used, which is what a setting left behind would leak through.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, Step, authorize_command, balance_of, books, capture_command, committed_at,
    create_command, eur, fee_rate, refund_command, steps, unbalanced_currencies, uncaused_postings,
};
use common::{
    merchant_id, runtime_pool, runtime_transaction, tenant_pool, tenant_pool_of_one, transition_id,
    wait_for_lock_wait,
};
use futures_util::future::{join, join_all};
use payments_core::accounts::{AccountRef, Psp};
use payments_core::db::commit;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::PaymentStatus;
use payments_core::money::PositiveAmount;
use payments_core::payments::{
    AuthorizeError, CaptureError, CreateError, CreatePayment, Outcome, Payment, Recorded,
    RefundError, StepCommand, StepError, authorize, cancel, capture, create_payment, decline,
    expire, find_payment, refund, register_merchant, settle, void,
};
use payments_core::tenancy::Tenant;
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

/// Creates payment `n` and authorizes it, as this merchant.
async fn authorized(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    authorize(me, &authorize_command(n, command.id)).await?;
    Ok(command.id)
}

#[sqlx::test]
async fn a_merchant_runs_a_payment_through_its_life(pool: PgPool) -> TestResult {
    // If any statement of any command forgot to name the merchant, the tenant role
    // would see nothing there, and the command would stop.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, merchant);

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
    assert_eq!(
        authorize(me, &authorize_command(1, payment)).await?,
        Outcome::Applied(Recorded {
            seq: 1,
            from: PaymentStatus::Created,
            to: PaymentStatus::Authorized,
            refundable: 0,
        })
    );
    assert_eq!(
        capture(me, &capture_command(1, payment)).await?,
        Outcome::Applied(Recorded {
            seq: 2,
            from: PaymentStatus::Authorized,
            to: PaymentStatus::Captured,
            refundable: 8_000,
        })
    );
    assert_eq!(
        refund(me, &refund_command(1, payment, 3_000)).await?,
        Outcome::Applied(Recorded {
            seq: 3,
            from: PaymentStatus::Captured,
            to: PaymentStatus::Captured,
            refundable: 5_000,
        })
    );
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
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchant_runs_the_commands_that_carry_nothing(pool: PgPool) -> TestResult {
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool_of_one(&pool).await?;
    let me = Tenant::new(&tenant, merchant);

    let plain = |n, payment| StepCommand {
        step: transition_id(n),
        payment,
    };
    let created = create_command(1, merchant);
    create_payment(me, &created).await?;
    cancel(me, &plain(101, created.id)).await?;
    let stored = find_payment(me, created.id).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Voided);

    let created = create_command(2, merchant);
    create_payment(me, &created).await?;
    decline(me, &plain(102, created.id)).await?;
    let stored = find_payment(me, created.id).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Failed);

    let payment = authorized(me, merchant, 3).await?;
    void(me, &plain(103, payment)).await?;
    let stored = find_payment(me, payment).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Voided);

    let payment = authorized(me, merchant, 4).await?;
    expire(me, &plain(104, payment)).await?;
    let stored = find_payment(me, payment).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Voided);

    let payment = authorized(me, merchant, 5).await?;
    capture(me, &capture_command(5, payment)).await?;
    settle(me, &plain(105, payment)).await?;
    let stored = find_payment(me, payment).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Settled);
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
    let them = Tenant::new(&tenant, theirs);
    let me = Tenant::new(&tenant, mine);

    let payment = authorized(them, theirs, 1).await?;
    capture(them, &capture_command(1, payment)).await?;
    assert_eq!(steps(&pool, payment).await?, 3);

    assert_eq!(find_payment(me, payment).await?, None);
    let err = authorize(me, &authorize_command(2, payment))
        .await
        .unwrap_err();
    assert!(matches!(err, AuthorizeError::NotFound { .. }), "{err:?}");
    let err = capture(me, &capture_command(2, payment)).await.unwrap_err();
    assert!(matches!(err, CaptureError::NotFound { .. }), "{err:?}");
    let err = refund(me, &refund_command(2, payment, 1_000))
        .await
        .unwrap_err();
    assert!(matches!(err, RefundError::NotFound { .. }), "{err:?}");
    let plain = |n| StepCommand {
        step: transition_id(n),
        payment,
    };
    for err in [
        cancel(me, &plain(201)).await.unwrap_err(),
        decline(me, &plain(202)).await.unwrap_err(),
        void(me, &plain(203)).await.unwrap_err(),
        expire(me, &plain(204)).await.unwrap_err(),
        settle(me, &plain(205)).await.unwrap_err(),
    ] {
        assert!(matches!(err, StepError::NotFound { .. }), "{err:?}");
    }

    assert_eq!(steps(&pool, payment).await?, 3);
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
    let payment = authorized(me, merchant, 1).await?;
    let reply = Recorded {
        seq: 2,
        from: PaymentStatus::Authorized,
        to: PaymentStatus::Captured,
        refundable: AUTHORIZED,
    };

    let command = capture_command(1, payment);
    assert_eq!(capture(me, &command).await?, Outcome::Applied(reply));
    assert_eq!(capture(me, &command).await?, Outcome::AlreadyApplied(reply));

    let changed = capture_command(1, payment);
    let changed = payments_core::payments::Capture {
        amount: PositiveAmount::new(AUTHORIZED - 1)?,
        ..changed
    };
    let err = capture(me, &changed).await.unwrap_err();
    assert!(matches!(err, CaptureError::Mismatch { .. }), "{err:?}");
    assert_eq!(steps(&pool, payment).await?, 3);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_apply_once_for_a_merchant(pool: PgPool) -> TestResult {
    // Copies of a create meet a duplicate key, and look their step up through the
    // policies, so that lookup must name the merchant too. Copies of a capture queue on
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
    authorize(me, &authorize_command(1, payment)).await?;

    let captures: Vec<_> = (0..8).map(|_| capture_command(1, payment)).collect();
    let results = join_all(captures.iter().map(|copy| capture(me, copy))).await;
    let mut applied = 0;
    for result in results {
        if let Outcome::Applied(_) = result? {
            applied += 1;
        }
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        AUTHORIZED
    );
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
    let command = StepCommand {
        step: transition_id(1),
        payment: PaymentId::try_from(first.payment_id)?,
    };
    let mut writer = runtime_transaction(&pool).await?;
    let stored = Step {
        id: command.step.get(),
        ..first.then("decline", "failed", 0, 0)
    };
    stored.insert(&mut writer).await?;

    let (answer, committed) = join(decline(me, &command), async {
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
            to: PaymentStatus::Failed,
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

    authorize(me, &authorize_command(1, command.id)).await?;
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);

    capture(me, &capture_command(1, command.id)).await?;
    assert_eq!(visible_with_no_merchant(&tenant).await?, 0);

    let refused = cancel(
        me,
        &StepCommand {
            step: transition_id(301),
            payment: command.id,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(refused, StepError::Illegal(_)), "{refused:?}");
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

    let payment_of_a = authorized(of_a, a, 1).await?;
    assert_eq!(find_payment(of_b, payment_of_a).await?, None);
    let payment_of_b = authorized(of_b, b, 2).await?;
    assert_eq!(find_payment(of_a, payment_of_b).await?, None);
    assert!(find_payment(of_a, payment_of_a).await?.is_some());
    assert!(find_payment(of_b, payment_of_b).await?.is_some());
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_system_role_still_runs_a_command_with_no_merchant(pool: PgPool) -> TestResult {
    // Later phases act on any merchant's payment, such as a webhook settling a capture.
    let merchant = merchant_id(1);
    register(&pool, &[merchant]).await?;
    let tenant = tenant_pool(&pool).await?;
    let system = runtime_pool(&pool).await?;
    let payment = authorized(Tenant::new(&tenant, merchant), merchant, 1).await?;

    capture(&system, &capture_command(1, payment)).await?;
    let stored = find_payment(&system, payment).await?.ok_or("stored")?;
    assert_eq!(stored.status, PaymentStatus::Captured);
    system.close().await;
    tenant.close().await;
    Ok(())
}
