// Author: Nicola Antonello

//! Capturing a payment (ADR-0016, ADR-0020, ADR-0021): the first move that moves money.
//! The PSP's capture reaches the payment through a catch-up (ADR-0031, ADR-0041), and its
//! step, its posting and its record commit together or not at all, so these tests check
//! each account's change, not only the total, and that nothing is ever half written. The
//! merchant's request for one meets the table of moves before its limit (ADR-0028). The
//! path that every command shares is tried in `payment_steps.rs`.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, NewPayment, REQUESTED, Step, balance_of, books, capture_time, committed_at,
    create_command, deadline, eur, fee_rate, requests_of, status_of, steps, unbalanced_currencies,
    uncaused_postings,
};
use common::psp::{PspSide, client_of, decided, hand_over_and_step};
use common::{
    CHECK_VIOLATION, assert_guard, assert_violation, merchant_id, request_id, runtime_pool,
    runtime_transaction, tenant_pool,
};
use futures_util::future::{join, join_all};
use mock_psp::wire::Detail;
use payments_core::accounts::{AccountRef, Psp};
use payments_core::catch_up::{CatchUpError, CaughtUp, catch_up};
use payments_core::db::commit;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::{BasisPoints, PositiveAmount};
use payments_core::payments::{CreatePayment, create_payment, find_payment, register_merchant};
use payments_core::requests::{
    Ask, Conflict, RecordRequest, Refusal, RequestError, RequestKind, RequestOutcome,
    record_request,
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

/// Creates payment `n` at this fee rate, as its merchant, which asks for its
/// authorization, and the PSP authorizes [`AUTHORIZED`].
async fn authorized_at(
    psp: &mut PspSide,
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
    fee_bps: u16,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = CreatePayment {
        fee_rate: BasisPoints::new(fee_bps)?,
        ..create_command(n, merchant)
    };
    create_payment(me, &command).await?;
    psp.authorized(me, command.id).await;
    Ok(command.id)
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("valid amount")
}

/// Request `n`: the capture of this much of a payment.
fn capture_request(n: u32, payment: PaymentId, minor: i64) -> RecordRequest {
    RecordRequest {
        request: request_id(n),
        payment,
        ask: Ask::Capture(amount(minor)),
    }
}

/// Asks for the capture of all of [`AUTHORIZED`], as its merchant, and the PSP approves it.
/// The catch-up that applies it is the test's.
async fn capture_approved(psp: &mut PspSide, me: Tenant<'_>, payment: PaymentId) {
    let capturing = capture_request(1, payment, AUTHORIZED);
    psp.sent(me, &capturing).await;
    psp.fake.approve(capturing.request.get());
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

#[sqlx::test]
async fn a_capture_stores_its_step_its_posting_and_its_record(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = authorized_at(&mut psp, me, merchant, 1, 250).await?;
    capture_approved(&mut psp, me, payment).await;
    psp.caught_up(payment).await;
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Captured
    );
    // ADR-0020's lines: 80.00 from the PSP, 2.5% of it to the platform.
    assert_eq!(
        balances(&psp.system, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    let step = sqlx::query!(
        "SELECT id, prev_seq, kind, from_status, to_status, amount, prev_refundable, refundable \
           FROM payment_transitions WHERE payment_id = $1 AND seq = 2",
        payment.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(step.prev_seq, 1);
    assert_eq!(step.kind, "capture");
    assert_eq!(
        (step.from_status.as_str(), step.to_status.as_str()),
        ("authorized", "captured")
    );
    assert_eq!(step.amount, AUTHORIZED);
    assert_eq!((step.prev_refundable, step.refundable), (0, AUTHORIZED));
    let record = sqlx::query!(
        "SELECT payment_id, amount, authorized_amount, fee, captured_at \
           FROM payment_captures WHERE transition_id = $1",
        step.id
    )
    .fetch_one(&pool)
    .await?;
    // The time comes from the PSP's fact (ADR-0031).
    let Detail::Captured { captured_at, .. } = decided(&psp.fake, payment, &request_id(1)) else {
        panic!("the fake captured it");
    };
    assert_eq!(record.payment_id, payment.get());
    assert_eq!(record.amount, AUTHORIZED);
    assert_eq!(record.authorized_amount, AUTHORIZED);
    assert_eq!(record.fee, FEE);
    assert_eq!(record.captured_at, BusinessTime::new(captured_at)?.get());
    // One posting, and a record caused it (ADR-0021).
    assert_eq!(postings(&pool).await?, 1);
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_fee_follows_the_rate_and_rounds_half_up(pool: PgPool) -> TestResult {
    // Each on a merchant of its own, so that the balances are this capture's:
    // (rate, captured) -> fee. A fee of 0 or of the whole capture leaves a line
    // out, and the check at COMMIT still finds exactly its lines (ADR-0020).
    let tenant = tenant_pool(&pool).await?;
    let mut psp = PspSide::start(&pool).await?;
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
        register_merchant(&psp.system, merchant, fee_rate()).await?;
        let me = Tenant::new(&tenant, merchant);
        let before = balance_of(&psp.system, AccountRef::FeeRevenue(eur())).await?;
        let receivable_before =
            balance_of(&psp.system, AccountRef::PspReceivable(Psp::Mock, eur())).await?;
        let payment = authorized_at(&mut psp, me, merchant, n, rate).await?;
        psp.applied(me, payment, Ask::Capture(amount(captured)))
            .await;
        let [receivable, payable, fees] = balances(&psp.system, merchant).await?;
        let context = format!("{captured} at {rate} basis points");
        assert_eq!(receivable - receivable_before, captured, "{context}");
        assert_eq!(payable, fee - captured, "{context}");
        assert_eq!(fees - before, -fee, "{context}");
    }
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_amount_is_at_most_the_authorization(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    // A partial capture and the whole authorization.
    for (n, minor) in [(1, 1), (2, AUTHORIZED)] {
        let payment = authorized_at(&mut psp, me, merchant, n, 250).await?;
        psp.applied(me, payment, Ask::Capture(amount(minor))).await;
        let stored = find_payment(me, payment).await?.ok_or("stored")?;
        assert_eq!(
            (stored.status, stored.refundable),
            (PaymentStatus::Captured, minor)
        );
    }
    // Above the authorization, though within the payment's amount: refused at once.
    let payment = authorized_at(&mut psp, me, merchant, 3, 250).await?;
    let before = postings(&pool).await?;
    let err = record_request(me, &capture_request(3, payment, AUTHORIZED + 1))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Refused(Refused::AboveLimit { amount, limit }))
                if amount == AUTHORIZED + 1 && limit == AUTHORIZED
        ),
        "{err:?}"
    );
    assert_eq!(
        status_of(&psp.system, payment).await?,
        PaymentStatus::Authorized
    );
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(requests_of(&pool, payment).await?, 1);
    assert_eq!(postings(&pool).await?, before);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_status_is_judged_before_the_amount(pool: PgPool) -> TestResult {
    // Every status but `authorized`, with an amount above any limit as well: the
    // answer is the illegal move, and nothing is recorded or posted (ADR-0016).
    let books = books(&pool).await?;
    let tenant = tenant_pool(&pool).await?;
    let me = Tenant::new(&tenant, MerchantId::try_from(books.merchant)?);
    let mut n = 0;
    for &status in PaymentStatus::ALL {
        if status == PaymentStatus::Authorized {
            continue;
        }
        n += 1;
        let newest = committed_at(&pool, &books, status).await?;
        let payment = PaymentId::try_from(newest.payment_id)?;
        let before = postings(&pool).await?;
        let err = record_request(me, &capture_request(n, payment, REQUESTED + 1))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                RequestError::Refused(Refusal::Refused(Refused::Illegal(IllegalTransition {
                    status: got,
                    event: Event::Capture,
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
async fn a_payment_written_without_its_accounts_is_reported(pool: PgPool) -> TestResult {
    // A manual session wrote the payment, so nothing opened its accounts. Posting
    // never creates them: a typo would become an account (ADR-0005).
    let mut tx = runtime_transaction(&pool).await?;
    let merchant = common::payments::register_merchant(&mut tx).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    Step::first(payment.id).insert(&mut tx).await?;
    commit(tx).await?;
    let psp = PspSide::start(&pool).await?;
    // The PSP authorized and captured it, so the catch-up must post.
    psp.fake.append(
        payment.id,
        Detail::Authorized {
            request_id: request_id(1).get(),
            amount: AUTHORIZED,
            expires_at: deadline().get(),
        },
    );
    psp.fake.append(
        payment.id,
        Detail::Captured {
            request_id: request_id(2).get(),
            amount: AUTHORIZED,
            captured_at: capture_time().get(),
        },
    );
    let id = PaymentId::try_from(payment.id)?;
    let err = catch_up(&psp.system, &client_of(&psp.fake), id)
        .await
        .unwrap_err();
    let CatchUpError::MissingAccount { key } = err else {
        panic!("expected MissingAccount, got {err:?}");
    };
    assert_eq!(key.to_string(), "psp_receivable/mock/EUR");
    assert_eq!(status_of(&psp.system, id).await?, PaymentStatus::Created);
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn eight_concurrent_copies_apply_once(pool: PgPool) -> TestResult {
    // Eight catch-ups of one capture: one applies it, the others find it held.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = authorized_at(&mut psp, me, merchant, 1, 250).await?;
    capture_approved(&mut psp, me, payment).await;
    let client = client_of(&psp.fake);
    let results = join_all((0..8).map(|_| catch_up(&psp.system, &client, payment))).await;
    let mut applied = 0;
    for result in results {
        let CaughtUp::Current { applied: these } = result? else {
            panic!("a copy stopped the payment");
        };
        applied += these;
    }
    assert_eq!(applied, 1);
    assert_eq!(steps(&pool, payment).await?, 3);
    assert_eq!(postings(&pool).await?, 1);
    assert_eq!(
        balances(&psp.system, merchant).await?,
        [AUTHORIZED, FEE - AUTHORIZED, -FEE]
    );
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_capture_racing_a_void_gets_one_winner(pool: PgPool) -> TestResult {
    // One capture or void waits for the PSP at a time (ADR-0039), so of the two requests
    // one is recorded and the other is told so, whichever order the lock gives. The
    // money moves only if the capture won.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let mut captures = 0;
    for n in 1..=8 {
        let payment = authorized_at(&mut psp, me, merchant, n, 250).await?;
        let capturing = capture_request(n, payment, AUTHORIZED);
        let voiding = RecordRequest {
            request: request_id(n + 100),
            payment,
            ask: Ask::Void,
        };
        let results = join(record_request(me, &capturing), record_request(me, &voiding)).await;
        let (won, winner) = match results {
            (
                Ok(RequestOutcome::Recorded(_)),
                Err(RequestError::Refused(Refusal::Conflict(Conflict::Pending(
                    RequestKind::Capture,
                )))),
            ) => {
                captures += 1;
                (&capturing, PaymentStatus::Captured)
            }
            (
                Err(RequestError::Refused(Refusal::Conflict(Conflict::Pending(RequestKind::Void)))),
                Ok(RequestOutcome::Recorded(_)),
            ) => (&voiding, PaymentStatus::Voided),
            other => panic!("{other:?}"),
        };
        // The PSP approves the one that it was sent.
        hand_over_and_step(&mut psp.sender, &psp.system, &won.request).await;
        psp.fake.approve(won.request.get());
        psp.caught_up(payment).await;
        assert_eq!(status_of(&psp.system, payment).await?, winner);
        assert_eq!(steps(&pool, payment).await?, 3);
    }
    assert_eq!(postings(&pool).await?, captures);
    assert_eq!(
        balances(&psp.system, merchant).await?,
        [
            captures * AUTHORIZED,
            captures * (FEE - AUTHORIZED),
            -captures * FEE
        ]
    );
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
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
    let payment = authorized_at(&mut psp, me, merchant, 1, 250).await?;
    capture_approved(&mut psp, me, payment).await;
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
    let err = catch_up(&psp.system, &client_of(&psp.fake), payment)
        .await
        .unwrap_err();
    let CatchUpError::Invariant { source, .. } = err else {
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
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_loses_fails_at_commit(pool: PgPool) -> TestResult {
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = authorized_at(&mut psp, me, merchant, 1, 250).await?;
    capture_approved(&mut psp, me, payment).await;
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
    assert_eq!(steps(&pool, payment).await?, 2);
    assert_eq!(postings(&pool).await?, 0);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_record_that_a_trigger_corrupts_is_a_bug(pool: PgPool) -> TestResult {
    // The fee is above the capture, which only the CHECK can refuse.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = authorized_at(&mut psp, me, merchant, 1, 250).await?;
    capture_approved(&mut psp, me, payment).await;
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
    let err = catch_up(&psp.system, &client_of(&psp.fake), payment)
        .await
        .unwrap_err();
    let CatchUpError::Invariant { source, .. } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_violation(source, CHECK_VIOLATION, "payment_captures_fee_check");
    assert_eq!(postings(&pool).await?, 0);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unproven_commit_failure_is_settled_by_catching_up_again(pool: PgPool) -> TestResult {
    // A deferred trigger ends the session during COMMIT, which proves nothing
    // (ADR-0014). Here nothing was stored, and the next catch-up applies the capture
    // once (ADR-0039).
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let payment = authorized_at(&mut psp, me, merchant, 1, 250).await?;
    capture_approved(&mut psp, me, payment).await;
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
    let client = client_of(&psp.fake);
    let err = catch_up(&psp.system, &client, payment).await.unwrap_err();
    assert!(matches!(err, CatchUpError::NotConfirmed { .. }), "{err:?}");
    assert_eq!(postings(&pool).await?, 0);
    sqlx::query!("DROP TRIGGER zz_end_session ON payment_captures")
        .execute(&pool)
        .await?;
    assert_eq!(
        catch_up(&psp.system, &client, payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    assert_eq!(postings(&pool).await?, 1);
    tenant.close().await;
    psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_partial_authorization_limits_the_capture(pool: PgPool) -> TestResult {
    // The limit is what the PSP authorized, read under the lock, not the request.
    let (tenant, merchant) = registered(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let mut psp = PspSide::start(&pool).await?;
    let command = create_command(1, merchant);
    create_payment(me, &command).await?;
    let payment = command.id;
    let authorizing = RecordRequest {
        request: request_id(100),
        payment,
        ask: Ask::Authorize(amount(REQUESTED)),
    };
    psp.sent(me, &authorizing).await;
    psp.fake.approve_for(authorizing.request.get(), 500);
    psp.caught_up(payment).await;
    let err = record_request(me, &capture_request(1, payment, 501))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Refused(Refused::AboveLimit {
                amount: 501,
                limit: 500,
            }))
        ),
        "{err:?}"
    );
    let within = capture_request(2, payment, 500);
    psp.sent(me, &within).await;
    psp.fake.approve(within.request.get());
    psp.caught_up(payment).await;
    let stored = find_payment(me, payment).await?.ok_or("stored")?;
    assert_eq!(
        (stored.status, stored.refundable),
        (PaymentStatus::Captured, 500)
    );
    tenant.close().await;
    psp.close().await;
    Ok(())
}
