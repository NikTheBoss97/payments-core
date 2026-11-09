// Author: Nicola Antonello

//! Recording a merchant's requests to the PSP (ADR-0028, ADR-0039), as the tenant role
//! runs them.
//!
//! A request moves nothing. These tests check what is recorded, what is refused at once,
//! and that a request counts while it is in flight, until a fact of the PSP's decides it.
//! Phase 3's commands set the payments up, and the system role writes the facts that
//! decide a request, as the catch-up will (ADR-0037).

mod common;

use std::error::Error;

use common::keys::{answered, key, print, render};
use common::payments::{
    AUTHORIZED, REQUESTED, authorize_command, capture_command, create_command, fee_rate, steps,
};
use common::{
    INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION, assert_violation, merchant_id, request_id,
    runtime_pool, runtime_transaction, sqlstate, tenant_pool, tenant_transaction, transition_id,
    wait_for_lock_wait,
};
use futures_util::future::{join, join_all};
use payments_core::catch_up::StopReason;
use payments_core::db::commit;
use payments_core::idempotency::{Seal, recall};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{Event, IllegalTransition, PaymentStatus, Refused};
use payments_core::money::PositiveAmount;
use payments_core::payments::{
    StepCommand, StepError, authorize, cancel, capture, create_payment, lock_key, register_merchant,
};
use payments_core::requests::{
    Ask, Conflict, RecordRequest, Refusal, RequestError, RequestKind, RequestOutcome,
    RequestRecorded, record_request,
};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// A stored request: its ID, its payment, its kind and its amount.
type Stored = (Uuid, Uuid, String, i64);

/// The tenant role's pool, and two merchants that the system role registered.
async fn two_merchants(pool: &PgPool) -> Result<(PgPool, MerchantId, MerchantId), Box<dyn Error>> {
    let system = runtime_pool(pool).await?;
    let (a, b) = (merchant_id(1), merchant_id(2));
    register_merchant(&system, a, fee_rate()).await?;
    register_merchant(&system, b, fee_rate()).await?;
    system.close().await;
    Ok((tenant_pool(pool).await?, a, b))
}

/// Payment `n` of this merchant, for [`REQUESTED`].
async fn created(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let command = create_command(n, merchant);
    create_payment(me, &command).await?;
    Ok(command.id)
}

/// Payment `n`, authorized for [`AUTHORIZED`] by phase 3's step.
async fn authorized(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = created(me, merchant, n).await?;
    authorize(me, &authorize_command(n, payment)).await?;
    Ok(payment)
}

/// Payment `n`, captured for [`AUTHORIZED`], all of which is refundable.
async fn captured(
    me: Tenant<'_>,
    merchant: MerchantId,
    n: u32,
) -> Result<PaymentId, Box<dyn Error>> {
    let payment = authorized(me, merchant, n).await?;
    capture(me, &capture_command(n, payment)).await?;
    Ok(payment)
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

/// What recording request `n` of this payment returns.
fn recorded(n: u32, payment: PaymentId) -> RequestOutcome {
    RequestOutcome::Recorded(RequestRecorded {
        request: request_id(n).get(),
        payment,
    })
}

/// Why this request was refused, or a panic if it was not.
async fn refusal(me: Tenant<'_>, command: &RecordRequest) -> Refusal {
    match record_request(me, command).await {
        Err(RequestError::Refused(refusal)) => refusal,
        other => panic!("{other:?}"),
    }
}

fn illegal(status: PaymentStatus, event: Event) -> Refusal {
    IllegalTransition { status, event }.into()
}

fn above(amount: i64, limit: i64) -> Refusal {
    Refused::AboveLimit { amount, limit }.into()
}

fn pending(kind: RequestKind) -> Refusal {
    Conflict::Pending(kind).into()
}

/// Stops a payment as the catch-up does, as the system role (ADR-0041).
async fn stop(pool: &PgPool, payment: PaymentId) -> TestResult {
    let mut tx = runtime_transaction(pool).await?;
    sqlx::query!(
        "INSERT INTO payment_stops (payment_id, position, reason) VALUES ($1, $2, $3)",
        payment.get(),
        1,
        StopReason::NoPlace.as_str()
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Every request recorded, whoever owns it, in the order of the test's IDs.
async fn requests_stored(pool: &PgPool) -> sqlx::Result<Vec<Stored>> {
    let rows =
        sqlx::query!("SELECT id, payment_id, kind, amount FROM payment_requests ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.payment_id, row.kind, row.amount))
        .collect())
}

/// The PSP's first fact about this payment, which refuses request `n`, written by the
/// system role as the catch-up will write it (ADR-0037). A refusal makes no step.
async fn first_fact_refuses(
    pool: &PgPool,
    payment: PaymentId,
    n: u32,
    kind: RequestKind,
) -> TestResult {
    let fact_kind = format!("{kind}_refused");
    let mut tx = runtime_transaction(pool).await?;
    sqlx::query!(
        "INSERT INTO payment_facts (payment_id, position, prev_position, psp, psp_fact_id, kind, amount)
         VALUES ($1, 1, 1, $2, $3, $4, 0)",
        payment.get(),
        "mock",
        format!("fct_{n}"),
        fact_kind
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO payment_request_decisions (request_id, payment_id, request_kind, position, fact_kind)
         VALUES ($1, $2, $3, 1, $4)",
        request_id(n).get(),
        payment.get(),
        kind.as_str(),
        fact_kind
    )
    .execute(&mut *tx)
    .await?;
    commit(tx).await?;
    Ok(())
}

#[sqlx::test]
async fn a_request_of_each_kind_is_recorded_and_moves_nothing(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let new = created(me, merchant, 1).await?;
    let held = authorized(me, merchant, 2).await?;
    let unwanted = authorized(me, merchant, 3).await?;
    let paid = captured(me, merchant, 4).await?;

    for (n, payment, ask) in [
        (1, new, Ask::Authorize(amount(REQUESTED))),
        (2, held, Ask::Capture(amount(AUTHORIZED))),
        (3, unwanted, Ask::Void),
        (4, paid, Ask::Refund(amount(AUTHORIZED))),
    ] {
        assert_eq!(
            record_request(me, &request(n, payment, ask)).await?,
            recorded(n, payment)
        );
    }

    let expected = [
        (1, new, "authorize", REQUESTED),
        (2, held, "capture", AUTHORIZED),
        (3, unwanted, "void", 0),
        (4, paid, "refund", AUTHORIZED),
    ]
    .map(|(n, payment, kind, amount)| {
        (request_id(n).get(), payment.get(), kind.to_owned(), amount)
    });
    assert_eq!(requests_stored(&pool).await?, expected);
    // Only a fact of the PSP's makes a step (ADR-0028).
    for (payment, count) in [(new, 1), (held, 2), (unwanted, 2), (paid, 3)] {
        assert_eq!(steps(&pool, payment).await?, count);
    }
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_move_that_the_table_or_a_limit_refuses_records_nothing(pool: PgPool) -> TestResult {
    use PaymentStatus as S;
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let new = created(me, merchant, 1).await?;
    let held = authorized(me, merchant, 2).await?;
    let paid = captured(me, merchant, 3).await?;

    for (n, payment, ask, expected) in [
        (
            1,
            new,
            Ask::Capture(amount(1)),
            illegal(S::Created, Event::Capture),
        ),
        (2, new, Ask::Void, illegal(S::Created, Event::Void)),
        // With nothing refundable, a refund is tried as the final one.
        (
            3,
            new,
            Ask::Refund(amount(1)),
            illegal(S::Created, Event::FinalRefund),
        ),
        (
            4,
            new,
            Ask::Authorize(amount(REQUESTED + 1)),
            above(REQUESTED + 1, REQUESTED),
        ),
        (
            5,
            held,
            Ask::Authorize(amount(1)),
            illegal(S::Authorized, Event::Authorize),
        ),
        (
            6,
            held,
            Ask::Capture(amount(AUTHORIZED + 1)),
            above(AUTHORIZED + 1, AUTHORIZED),
        ),
        (7, paid, Ask::Void, illegal(S::Captured, Event::Void)),
        (
            8,
            paid,
            Ask::Refund(amount(AUTHORIZED + 1)),
            above(AUTHORIZED + 1, AUTHORIZED),
        ),
    ] {
        assert_eq!(
            refusal(me, &request(n, payment, ask)).await,
            expected,
            "request {n}"
        );
    }
    assert_eq!(requests_stored(&pool).await?, Vec::<Stored>::new());
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_payment_gets_one_authorization_request(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    record_request(me, &request(1, payment, Ask::Authorize(amount(REQUESTED)))).await?;

    // Whatever its amount, a second one conflicts while the first waits for the PSP.
    assert_eq!(
        refusal(me, &request(2, payment, Ask::Authorize(amount(1)))).await,
        Conflict::AuthorizationRequested.into()
    );
    // Once the payment is authorized, the status refuses it, as it refuses the step.
    authorize(me, &authorize_command(1, payment)).await?;
    assert_eq!(
        refusal(me, &request(3, payment, Ask::Authorize(amount(1)))).await,
        illegal(PaymentStatus::Authorized, Event::Authorize)
    );
    assert_eq!(requests_stored(&pool).await?.len(), 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_database_refuses_a_second_authorization_request_that_skipped_the_lock(
    pool: PgPool,
) -> TestResult {
    // A writer that skips the payment's lock, which no path of the service does, holds an
    // authorization request that it has not committed. The command cannot see it, so only
    // the database's key stops the second one (ADR-0037).
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    let mut writer = tenant_transaction(&pool, merchant.get()).await?;
    sqlx::query!(
        "INSERT INTO payment_requests (id, payment_id, kind, amount) VALUES ($1, $2, $3, $4)",
        request_id(1).get(),
        payment.get(),
        "authorize",
        REQUESTED
    )
    .execute(&mut *writer)
    .await?;

    let command = request(2, payment, Ask::Authorize(amount(REQUESTED)));
    let (result, written) = join(record_request(me, &command), async {
        // The command's insert waits for the writer's row under the key.
        wait_for_lock_wait(&pool, "transactionid").await?;
        commit(writer).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    written?;
    match result {
        Err(RequestError::Invariant { request, source }) => {
            assert_eq!(request, request_id(2).get());
            assert_violation(source, UNIQUE_VIOLATION, "payment_requests_authorize_key");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(requests_stored(&pool).await?.len(), 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_capture_or_a_void_waits_for_the_one_in_flight(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = authorized(me, merchant, 1).await?;
    record_request(me, &request(1, payment, Ask::Capture(amount(AUTHORIZED)))).await?;

    assert_eq!(
        refusal(me, &request(2, payment, Ask::Capture(amount(1)))).await,
        pending(RequestKind::Capture)
    );
    assert_eq!(
        refusal(me, &request(3, payment, Ask::Void)).await,
        pending(RequestKind::Capture)
    );
    // The PSP refuses the capture: the payment stays authorized, with nothing in flight.
    first_fact_refuses(&pool, payment, 1, RequestKind::Capture).await?;
    assert_eq!(
        record_request(me, &request(4, payment, Ask::Void)).await?,
        recorded(4, payment)
    );
    assert_eq!(
        refusal(me, &request(5, payment, Ask::Capture(amount(1)))).await,
        pending(RequestKind::Void)
    );
    assert_eq!(requests_stored(&pool).await?.len(), 2);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refund_counts_the_refunds_in_flight(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = captured(me, merchant, 1).await?;
    record_request(me, &request(1, payment, Ask::Refund(amount(5_000)))).await?;

    // 8000 refundable, of which 5000 are asked already.
    assert_eq!(
        refusal(me, &request(2, payment, Ask::Refund(amount(3_001)))).await,
        above(3_001, 3_000)
    );
    assert_eq!(
        record_request(me, &request(3, payment, Ask::Refund(amount(3_000)))).await?,
        recorded(3, payment)
    );
    assert_eq!(
        refusal(me, &request(4, payment, Ask::Refund(amount(1)))).await,
        above(1, 0)
    );
    // The PSP refuses the first, so what it asked can be asked again.
    first_fact_refuses(&pool, payment, 1, RequestKind::Refund).await?;
    assert_eq!(
        record_request(me, &request(5, payment, Ask::Refund(amount(5_000)))).await?,
        recorded(5, payment)
    );
    assert_eq!(requests_stored(&pool).await?.len(), 3);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_stopped_payment_takes_no_request(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let created = created(me, merchant, 1).await?;
    let authorized = authorized(me, merchant, 2).await?;
    let captured = captured(me, merchant, 3).await?;
    for payment in [created, authorized, captured] {
        stop(&pool, payment).await?;
    }

    // What each status would take, and a refund that the status would refuse: the stop
    // decides first (ADR-0042).
    for (n, payment, ask) in [
        (1, created, Ask::Authorize(amount(REQUESTED))),
        (2, authorized, Ask::Capture(amount(AUTHORIZED))),
        (3, authorized, Ask::Void),
        (4, captured, Ask::Refund(amount(1))),
        (5, created, Ask::Refund(amount(1))),
    ] {
        assert_eq!(
            refusal(me, &request(n, payment, ask)).await,
            Conflict::Stopped.into(),
            "request {n}"
        );
    }

    // Under a key the refusal is final, so it is stored (ADR-0026).
    let under_a_key = key("authorize-stopped");
    let seal = Seal {
        key: &under_a_key,
        fingerprint: print(6),
        answer: &render,
    };
    let err = record_request(
        me.sealed(&seal),
        &request(6, created, Ask::Authorize(amount(REQUESTED))),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Conflict(Conflict::Stopped))
        ),
        "{err:?}"
    );
    assert_eq!(
        recall(me, &under_a_key, print(6)).await?,
        answered(
            409,
            r#"{"type":"conflict","conflict":"this payment is stopped until an operator looks at it"}"#
        )
    );
    assert_eq!(requests_stored(&pool).await?, Vec::<Stored>::new());
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_stop_holds_only_its_payment_and_a_retry_still_finds_its_request(
    pool: PgPool,
) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    let other = created(me, merchant, 2).await?;
    let first = request(1, payment, Ask::Authorize(amount(REQUESTED)));
    assert_eq!(record_request(me, &first).await?, recorded(1, payment));
    stop(&pool, payment).await?;

    // Identity first (ADR-0019): a copy of a request recorded before the stop is the
    // same request, and a new one is refused.
    assert_eq!(
        record_request(me, &first).await?,
        RequestOutcome::AlreadyRecorded(RequestRecorded {
            request: request_id(1).get(),
            payment,
        })
    );
    assert_eq!(
        refusal(me, &request(2, payment, Ask::Void)).await,
        Conflict::Stopped.into()
    );
    // Another payment of the same merchant takes its requests.
    assert_eq!(
        record_request(me, &request(3, other, Ask::Authorize(amount(REQUESTED)))).await?,
        recorded(3, other)
    );
    assert_eq!(requests_stored(&pool).await?.len(), 2);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_retry_finds_its_request_before_anything_refuses_it(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    let other = created(me, merchant, 2).await?;
    let ask = Ask::Authorize(amount(REQUESTED));
    record_request(me, &request(1, payment, ask)).await?;

    // A new request would conflict now, but the same command finds its own.
    assert_eq!(
        record_request(me, &request(1, payment, ask)).await?,
        RequestOutcome::AlreadyRecorded(RequestRecorded {
            request: request_id(1).get(),
            payment,
        })
    );
    // The same ID with other content is a bug.
    for reused in [
        request(1, payment, Ask::Authorize(amount(REQUESTED - 1))),
        request(1, payment, Ask::Capture(amount(REQUESTED))),
        request(1, other, ask),
    ] {
        let err = record_request(me, &reused).await.unwrap_err();
        assert!(
            matches!(err, RequestError::Mismatch { request } if request == request_id(1).get()),
            "{err:?}"
        );
    }
    assert_eq!(requests_stored(&pool).await?.len(), 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchant_records_requests_only_on_its_own_payments(pool: PgPool) -> TestResult {
    let (tenant, a, b) = two_merchants(&pool).await?;
    let (me, them) = (Tenant::new(&tenant, a), Tenant::new(&tenant, b));
    let theirs = created(them, b, 1).await?;

    let err = record_request(me, &request(1, theirs, Ask::Authorize(amount(1))))
        .await
        .unwrap_err();
    assert!(
        matches!(err, RequestError::NotFound { payment } if payment == theirs),
        "{err:?}"
    );
    assert_eq!(requests_stored(&pool).await?, Vec::<Stored>::new());

    // Nor does identity first show a merchant another's request: an ID that it cannot see
    // reads as free, until the database's key refuses it.
    record_request(them, &request(2, theirs, Ask::Authorize(amount(1)))).await?;
    let mine = created(me, a, 2).await?;
    match record_request(me, &request(2, mine, Ask::Authorize(amount(1)))).await {
        Err(RequestError::Invariant { source, .. }) => {
            assert_violation(source, UNIQUE_VIOLATION, "payment_requests_pkey");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(requests_stored(&pool).await?.len(), 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn only_a_merchant_records_a_request(pool: PgPool) -> TestResult {
    // The system role runs the sender and the catch-up, and asks the PSP for nothing of
    // its own (ADR-0037).
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let payment = created(Tenant::new(&tenant, merchant), merchant, 1).await?;
    let system = runtime_pool(&pool).await?;

    match record_request(&system, &request(1, payment, Ask::Authorize(amount(1)))).await {
        Err(RequestError::Invariant { source, .. }) => {
            assert_eq!(sqlstate(source), INSUFFICIENT_PRIVILEGE);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(requests_stored(&pool).await?, Vec::<Stored>::new());
    system.close().await;
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_request_under_a_key_stores_its_answer_with_it(pool: PgPool) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = created(me, merchant, 1).await?;
    let first = key("authorize");
    let seal = Seal {
        key: &first,
        fingerprint: print(1),
        answer: &render,
    };
    assert_eq!(
        record_request(
            me.sealed(&seal),
            &request(1, payment, Ask::Authorize(amount(REQUESTED)))
        )
        .await?,
        recorded(1, payment)
    );
    assert_eq!(
        recall(me, &first, print(1)).await?,
        answered(202, &format!(r#"{{"request":"{}"}}"#, request_id(1).get()))
    );

    // A second authorization under a key of its own: the conflict is final, so it is
    // stored (ADR-0026).
    let second = key("authorize-again");
    let seal_again = Seal {
        key: &second,
        fingerprint: print(2),
        answer: &render,
    };
    let err = record_request(
        me.sealed(&seal_again),
        &request(2, payment, Ask::Authorize(amount(REQUESTED))),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RequestError::Refused(Refusal::Conflict(Conflict::AuthorizationRequested))
        ),
        "{err:?}"
    );
    assert_eq!(
        recall(me, &second, print(2)).await?,
        answered(
            409,
            r#"{"type":"conflict","conflict":"an authorization of this payment was requested already"}"#
        )
    );

    // Another attempt under the first key, with a fresh ID, rolls back whole.
    let err = record_request(
        me.sealed(&seal),
        &request(3, payment, Ask::Authorize(amount(REQUESTED))),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RequestError::KeyTaken), "{err:?}");
    assert_eq!(requests_stored(&pool).await?.len(), 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_one_request_racing_record_it_once(pool: PgPool) -> TestResult {
    // Each copy has a fresh ID, and each would be recorded on its own, because a refund of
    // 1 fits many times over: only the key can stop them (ADR-0026).
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = captured(me, merchant, 1).await?;
    let k = key("refund");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let commands: Vec<RecordRequest> = (1..=8)
        .map(|n| request(n, payment, Ask::Refund(amount(1))))
        .collect();
    let sealed = me.sealed(&seal);
    let results = join_all(
        commands
            .iter()
            .map(|command| record_request(sealed, command)),
    )
    .await;

    let mut winners = Vec::new();
    for result in results {
        match result {
            Ok(RequestOutcome::Recorded(recorded)) => winners.push(recorded.request),
            Err(RequestError::KeyTaken) => {}
            other => panic!("{other:?}"),
        }
    }
    let [winner] = winners[..] else {
        panic!("{winners:?}")
    };
    assert_eq!(
        requests_stored(&pool).await?,
        [(winner, payment.get(), "refund".to_owned(), 1)]
    );
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(202, &format!(r#"{{"request":"{winner}"}}"#))
    );
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_request_waits_for_the_payments_lock_and_reads_what_its_holder_committed(
    pool: PgPool,
) -> TestResult {
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let payment = authorized(me, merchant, 1).await?;
    // The holder takes the payment's lock and records a capture, as the command would.
    let mut holder = tenant_transaction(&pool, merchant.get()).await?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", lock_key(payment))
        .execute(&mut *holder)
        .await?;
    sqlx::query!(
        "INSERT INTO payment_requests (id, payment_id, kind, amount) VALUES ($1, $2, $3, $4)",
        request_id(1).get(),
        payment.get(),
        "capture",
        AUTHORIZED
    )
    .execute(&mut *holder)
    .await?;

    let command = request(2, payment, Ask::Void);
    let (result, released) = join(record_request(me, &command), async {
        wait_for_lock_wait(&pool, "advisory").await?;
        commit(holder).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    released?;
    // Its one read came after the lock, so it saw the capture in flight.
    assert!(
        matches!(
            result,
            Err(RequestError::Refused(Refusal::Conflict(Conflict::Pending(
                RequestKind::Capture
            ))))
        ),
        "{result:?}"
    );
    assert_eq!(requests_stored(&pool).await?.len(), 1);
    tenant.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_cancel_is_refused_once_an_authorization_is_requested(pool: PgPool) -> TestResult {
    // A cancellation is a step of our own, which the PSP never hears of: once it may hold
    // an authorization, only a void releases it (ADR-0039).
    let (tenant, merchant, _) = two_merchants(&pool).await?;
    let me = Tenant::new(&tenant, merchant);
    let asked = created(me, merchant, 1).await?;
    let unasked = created(me, merchant, 2).await?;
    record_request(me, &request(1, asked, Ask::Authorize(amount(REQUESTED)))).await?;
    let cancel_of = |n, payment| StepCommand {
        step: transition_id(n),
        payment,
    };

    let err = cancel(me, &cancel_of(101, asked)).await.unwrap_err();
    assert!(matches!(err, StepError::AuthorizationRequested), "{err:?}");
    assert_eq!(steps(&pool, asked).await?, 1);
    // Under a key, the conflict is final, so it is stored (ADR-0026).
    let k = key("cancel");
    let seal = Seal {
        key: &k,
        fingerprint: print(1),
        answer: &render,
    };
    let err = cancel(me.sealed(&seal), &cancel_of(102, asked))
        .await
        .unwrap_err();
    assert!(matches!(err, StepError::AuthorizationRequested), "{err:?}");
    assert_eq!(
        recall(me, &k, print(1)).await?,
        answered(
            409,
            r#"{"type":"conflict","conflict":"an authorization of this payment was requested already"}"#
        )
    );
    // Once the payment is authorized, the status refuses the cancel first, as before.
    authorize(me, &authorize_command(1, asked)).await?;
    let err = cancel(me, &cancel_of(103, asked)).await.unwrap_err();
    assert!(
        matches!(
            err,
            StepError::Illegal(IllegalTransition {
                status: PaymentStatus::Authorized,
                event: Event::Cancel
            })
        ),
        "{err:?}"
    );
    // A payment with no request is still cancelled.
    cancel(me, &cancel_of(104, unasked)).await?;
    assert_eq!(steps(&pool, unasked).await?, 2);
    tenant.close().await;
    Ok(())
}
