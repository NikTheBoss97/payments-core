// Author: Nicola Antonello

//! Authorizing a payment (ADR-0016, ADR-0022). The PSP's authorization reaches the payment
//! through a catch-up, which stores its step and its record (ADR-0031, ADR-0041), and the
//! merchant's request for one meets the table of moves before its limit (ADR-0016,
//! ADR-0028). The path that every command shares is tried in `payment_steps.rs`, and the
//! catch-up's own rules in `catching_up.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, REQUESTED, books, committed_at, create_command, deadline, fee_rate, requests_of,
    status_of, steps,
};
use common::psp::{PspSide, client_of, decided};
use common::{
    CHECK_VIOLATION, assert_guard, assert_violation, merchant_id, request_id, runtime_pool,
    tenant_pool, transition_id,
};
use futures_util::future::join;
use mock_psp::wire::Detail;
use payments_core::catch_up::{CatchUpError, CaughtUp, Stop, StopReason, catch_up};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::PositiveAmount;
use payments_core::payments::{
    Outcome, StepCommand, StepError, cancel, create_payment, find_payment, register_merchant,
};
use payments_core::requests::{
    Ask, RecordRequest, Refusal, RequestError, RequestOutcome, record_request,
};
use payments_core::tenancy::Tenant;
use payments_core::times::BusinessTime;
use sqlx::PgPool;

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

/// Creates payment `n`, as its merchant.
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

#[sqlx::test]
async fn an_authorization_stores_its_step_and_its_record(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = created(me, merchant, 1).await?;
    psp.sent(me, &authorization(1, payment)).await;
    // Less than asked (ADR-0016).
    psp.fake.approve_for(request_id(1).get(), AUTHORIZED);
    psp.caught_up(payment).await;
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Authorized
    );
    let step = sqlx::query!(
        "SELECT id, prev_seq, kind, from_status, to_status, amount, prev_refundable, refundable \
           FROM payment_transitions WHERE payment_id = $1 AND seq = 1",
        payment.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(step.prev_seq, 0);
    assert_eq!(step.kind, "authorize");
    assert_eq!(
        (step.from_status.as_str(), step.to_status.as_str()),
        ("created", "authorized")
    );
    assert_eq!(step.amount, AUTHORIZED);
    assert_eq!((step.prev_refundable, step.refundable), (0, 0));
    let record = sqlx::query!(
        "SELECT payment_id, amount, payment_amount, expires_at \
           FROM payment_authorizations WHERE transition_id = $1",
        step.id
    )
    .fetch_one(&pool)
    .await?;
    // The time comes from the PSP's fact, never from the merchant (ADR-0031).
    let Detail::Authorized { expires_at, .. } = decided(&psp.fake, payment, &request_id(1)) else {
        panic!("the fake authorized it");
    };
    assert_eq!(record.payment_id, payment.get());
    assert_eq!(record.amount, AUTHORIZED);
    assert_eq!(record.payment_amount, REQUESTED);
    assert_eq!(record.expires_at, BusinessTime::new(expires_at)?.get());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_amount_is_at_most_the_request(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    // A partial authorization and the whole amount.
    for (n, minor) in [(1, 1), (2, AUTHORIZED), (3, REQUESTED)] {
        let payment = created(me, merchant, n).await?;
        psp.sent(me, &authorization(n, payment)).await;
        psp.fake.approve_for(request_id(n).get(), minor);
        psp.caught_up(payment).await;
        let stored = find_payment(&psp.system, payment).await?.ok_or("stored")?;
        assert_eq!(stored.status, PaymentStatus::Authorized, "{minor}");
        assert_eq!(
            stored.authorization.map(|held| held.amount.get()),
            Some(minor),
            "{minor}"
        );
    }
    // More than asked, which no PSP gives, so the fake is told to (ADR-0038): the payment
    // stops at it, and nothing moves (ADR-0041).
    let payment = created(me, merchant, 4).await?;
    psp.sent(me, &authorization(4, payment)).await;
    psp.fake.append(
        payment.get(),
        Detail::Authorized {
            request_id: request_id(4).get(),
            amount: REQUESTED + 1,
            expires_at: deadline().get(),
        },
    );
    assert_eq!(
        catch_up(&psp.system, &client_of(&psp.fake), payment).await?,
        CaughtUp::Stopped {
            applied: 0,
            stop: Stop {
                position: 1,
                reason: StopReason::Contradiction,
            },
        }
    );
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Created
    );
    assert_eq!(steps(&pool, payment).await?, 1);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_status_is_judged_before_the_amount(pool: PgPool) -> TestResult {
    // Every status but `created`, with an amount above the limit as well: the
    // answer is the illegal move (ADR-0016), and nothing is recorded.
    let books = books(&pool).await?;
    let tenant = tenant_pool(&pool).await?;
    let me = Tenant::new(&tenant, MerchantId::try_from(books.merchant)?);
    let mut n = 0;
    for &status in PaymentStatus::ALL {
        if status == PaymentStatus::Created {
            continue;
        }
        n += 1;
        let newest = committed_at(&pool, &books, status).await?;
        let payment = PaymentId::try_from(newest.payment_id)?;
        let command = RecordRequest {
            ask: Ask::Authorize(amount(REQUESTED + 1)),
            ..authorization(n, payment)
        };
        let err = record_request(me, &command).await.unwrap_err();
        assert!(
            matches!(
                err,
                RequestError::Refused(Refusal::Refused(Refused::Illegal(IllegalTransition {
                    status: got,
                    event: Event::Authorize,
                }))) if got == status
            ),
            "{status}: {err:?}"
        );
        assert_eq!(requests_of(&pool, payment).await?, 0);
        assert_eq!(steps(&pool, payment).await?, i64::from(newest.seq) + 1);
    }
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_authorization_racing_a_cancel_gets_one_winner(pool: PgPool) -> TestResult {
    // A cancel is refused once an authorization is asked for, and an authorization once
    // the payment is cancelled (ADR-0039): one of the two, whichever takes the lock first.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    for n in 1..=8 {
        let payment = created(me, merchant, n).await?;
        let cancelling = StepCommand {
            step: transition_id(n + 2000),
            payment,
        };
        let results = join(
            record_request(me, &authorization(n, payment)),
            cancel(me, &cancelling),
        )
        .await;
        let (winner, requests, steps_made) = match results {
            (Ok(RequestOutcome::Recorded(_)), Err(StepError::AuthorizationRequested)) => {
                (PaymentStatus::Created, 1, 1)
            }
            (
                Err(RequestError::Refused(Refusal::Refused(Refused::Illegal(illegal)))),
                Ok(Outcome::Applied(_)),
            ) => {
                assert_eq!(
                    illegal,
                    IllegalTransition {
                        status: PaymentStatus::Voided,
                        event: Event::Authorize,
                    }
                );
                (PaymentStatus::Voided, 0, 2)
            }
            other => panic!("{other:?}"),
        };
        let stored = find_payment(me, payment).await?.ok_or("stored")?;
        assert_eq!(stored.status, winner);
        assert_eq!(requests_of(&pool, payment).await?, requests);
        assert_eq!(steps(&pool, payment).await?, steps_made);
    }
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_loses_fails_at_commit(pool: PgPool) -> TestResult {
    // Only the check at COMMIT can notice a step without its record, and LG004
    // proves a rollback (ADR-0014): none of the catch-up stays.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = created(me, merchant, 1).await?;
    psp.sent(me, &authorization(1, payment)).await;
    psp.fake.approve(request_id(1).get());
    sqlx::query!(
        "CREATE FUNCTION lose_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RETURN NULL; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER lose_record BEFORE INSERT ON payment_authorizations \
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
    assert_eq!(steps(&pool, payment).await?, 1);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_corrupts_is_a_bug(pool: PgPool) -> TestResult {
    // The types allow the write; only the CHECK can refuse what reaches it.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = created(me, merchant, 1).await?;
    psp.sent(me, &authorization(1, payment)).await;
    psp.fake.approve(request_id(1).get());
    sqlx::query!(
        "CREATE FUNCTION raise_amount() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN NEW.amount := NEW.payment_amount + 1; RETURN NEW; END $$"
    )
    .execute(&pool)
    .await?;
    sqlx::query!(
        "CREATE TRIGGER raise_amount BEFORE INSERT ON payment_authorizations \
         FOR EACH ROW EXECUTE FUNCTION raise_amount()"
    )
    .execute(&pool)
    .await?;
    let err = catch_up(&psp.system, &client_of(&psp.fake), payment)
        .await
        .unwrap_err();
    let CatchUpError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(
        source,
        CHECK_VIOLATION,
        "payment_authorizations_amount_check",
    );
    assert_eq!(steps(&pool, payment).await?, 1);
    tenant.close().await;
    psp.close().await;
    Ok(())
}
