// Author: Nicola Antonello

//! Catching a payment up with the PSP's history (ADR-0031, ADR-0039, ADR-0041), as the
//! system role runs it, against a fake PSP over real HTTP (ADR-0029).
//!
//! Each test records its requests as the merchant, posts them to its fake as the sender
//! will, has the fake decide them, and catches up. It checks the steps, records and
//! postings that the facts made, the facts, decisions and links that the catch-up wrote,
//! and where it stopped a payment. A history that the fake cannot serve, such as one that
//! lost a fact, comes from a PSP whose answer the test writes.

mod common;

use std::error::Error;

use axum::http::StatusCode;
use common::payments::{
    AUTHORIZED, REQUESTED, create_command, fee_rate, unbalanced_currencies, uncaused_postings,
};
use common::psp::{ScriptedPsp, client_of, send};
use common::{merchant_id, payment_id, request_id, runtime_pool, tenant_pool, wait_for_lock_wait};
use futures_util::future::{join, join_all};
use mock_psp::wire::{Detail, History};
use mock_psp::{Failure, MockPsp, Route};
use payments_core::catch_up::{CatchUpError, CaughtUp, Stop, StopReason, apply, catch_up, read};
use payments_core::db::commit;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, lock_key, register_merchant};
use payments_core::psp::{ReadError, Unreadable};
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::tenancy::Tenant;
use serde_json::json;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// A step: its kind, its amount, and the status that it moved the payment to.
type StepRow = (String, i64, String);

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// 2026-01-01T00:00:00Z, for the facts that a test appends.
fn at() -> DateTime<Utc> {
    DateTime::from_timestamp_micros(1_767_225_600_000_000).expect("a time in range")
}

fn step(kind: &str, minor: i64, to: &str) -> StepRow {
    (kind.to_owned(), minor, to.to_owned())
}

fn stopped(applied: u32, position: i32, reason: StopReason) -> CaughtUp {
    CaughtUp::Stopped {
        applied,
        stop: Stop { position, reason },
    }
}

/// A merchant with its pool, the system role's pool, which catches up, and a fake PSP.
struct World {
    system: PgPool,
    tenant: PgPool,
    merchant: MerchantId,
    fake: MockPsp,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let system = runtime_pool(pool).await?;
        let merchant = merchant_id(1);
        register_merchant(&system, merchant, fee_rate()).await?;
        Ok(Self {
            tenant: tenant_pool(pool).await?,
            system,
            merchant,
            fake: MockPsp::start().await?,
        })
    }

    fn me(&self) -> Tenant<'_> {
        Tenant::new(&self.tenant, self.merchant)
    }

    /// Payment `n`, for [`REQUESTED`].
    async fn created(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let command = create_command(n, self.merchant);
        create_payment(self.me(), &command).await?;
        Ok(command.id)
    }

    /// Records request `n` as the merchant and posts it to the fake. Returns its ID.
    async fn ask(&self, n: u32, payment: PaymentId, ask: Ask) -> Result<Uuid, Box<dyn Error>> {
        let command = RecordRequest {
            request: request_id(n),
            payment,
            ask,
        };
        record_request(self.me(), &command).await?;
        send(&self.fake, &command).await;
        Ok(command.request.get())
    }

    async fn catch_up(&self, payment: PaymentId) -> Result<CaughtUp, CatchUpError> {
        catch_up(&self.system, &client_of(&self.fake), payment).await
    }

    /// Payment `n`, authorized by the PSP for [`AUTHORIZED`] and caught up. Its requests
    /// are numbered from `10 * n`.
    async fn authorized(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.created(n).await?;
        let authorize = self
            .ask(10 * n, payment, Ask::Authorize(amount(REQUESTED)))
            .await?;
        self.fake.approve_for(authorize, AUTHORIZED);
        assert_eq!(
            self.catch_up(payment).await?,
            CaughtUp::Current { applied: 1 }
        );
        Ok(payment)
    }

    /// Payment `n`, captured by the PSP for [`AUTHORIZED`] and caught up.
    async fn captured(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.authorized(n).await?;
        let capture = self
            .ask(10 * n + 1, payment, Ask::Capture(amount(AUTHORIZED)))
            .await?;
        self.fake.approve(capture);
        assert_eq!(
            self.catch_up(payment).await?,
            CaughtUp::Current { applied: 1 }
        );
        Ok(payment)
    }

    /// The fake's history of this payment, which a scripted PSP can serve changed.
    fn history(&self, payment: PaymentId) -> History {
        self.fake
            .history(payment.get())
            .expect("the fake holds the payment")
    }

    async fn close(self) {
        self.system.close().await;
        self.tenant.close().await;
    }
}

async fn steps_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Vec<StepRow>> {
    let rows = sqlx::query!(
        "SELECT kind, amount, to_status FROM payment_transitions WHERE payment_id = $1 ORDER BY seq",
        payment.get()
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.kind, row.amount, row.to_status))
        .collect())
}

/// The facts held: position, the PSP's ID, kind and amount.
async fn facts_of(
    pool: &PgPool,
    payment: PaymentId,
) -> sqlx::Result<Vec<(i32, String, String, i64)>> {
    let rows = sqlx::query!(
        "SELECT position, psp_fact_id, kind, amount FROM payment_facts
          WHERE payment_id = $1 ORDER BY position",
        payment.get()
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.position, row.psp_fact_id, row.kind, row.amount))
        .collect())
}

/// The facts held, by kind alone.
async fn kinds_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Vec<String>> {
    Ok(facts_of(pool, payment)
        .await?
        .into_iter()
        .map(|(_, _, kind, _)| kind)
        .collect())
}

/// The decisions: the request, the position of the fact that decided it, and its kind.
async fn decisions_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Vec<(Uuid, i32, String)>> {
    let rows = sqlx::query!(
        "SELECT request_id, position, fact_kind FROM payment_request_decisions
          WHERE payment_id = $1 ORDER BY position",
        payment.get()
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.request_id, row.position, row.fact_kind))
        .collect())
}

/// The links of steps to the facts that made them: the step's `seq`, the fact's
/// position, and their kind.
async fn links_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Vec<(i32, i32, String)>> {
    let rows = sqlx::query!(
        "SELECT t.seq, l.position, l.kind FROM payment_transition_facts l
           JOIN payment_transitions t ON t.id = l.transition_id
          WHERE l.payment_id = $1 ORDER BY l.position",
        payment.get()
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.seq, row.position, row.kind))
        .collect())
}

/// Where the payment stopped, if it did: the position and the reason.
async fn stop_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Option<(i32, String)>> {
    let row = sqlx::query!(
        "SELECT position, reason FROM payment_stops WHERE payment_id = $1",
        payment.get()
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| (row.position, row.reason)))
}

#[sqlx::test]
async fn an_authorization_a_capture_and_refunds_make_their_steps_records_and_postings(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .ask(1, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve_for(authorize, AUTHORIZED);
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    let capture = world
        .ask(2, payment, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.approve(capture);
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    let partial = world.ask(3, payment, Ask::Refund(amount(3_000))).await?;
    let rest = world.ask(4, payment, Ask::Refund(amount(5_000))).await?;
    world.fake.approve(partial);
    world.fake.approve(rest);
    // Both refunds in one catch-up, in the PSP's order.
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 2 }
    );

    assert_eq!(
        steps_of(&pool, payment).await?,
        [
            step("create", 0, "created"),
            step("authorize", AUTHORIZED, "authorized"),
            step("capture", AUTHORIZED, "captured"),
            step("refund", 3_000, "captured"),
            step("refund", 5_000, "refunded"),
        ]
    );
    let theirs = world.history(payment);
    let held: Vec<_> = theirs
        .facts
        .iter()
        .zip([
            ("authorize", AUTHORIZED),
            ("capture", AUTHORIZED),
            ("refund", 3_000),
            ("refund", 5_000),
        ])
        .map(|(fact, (kind, minor))| {
            let position = i32::try_from(fact.position).expect("a small position");
            (position, fact.id.clone(), kind.to_owned(), minor)
        })
        .collect();
    assert_eq!(facts_of(&pool, payment).await?, held);
    assert_eq!(
        decisions_of(&pool, payment).await?,
        [
            (authorize, 1, "authorize".to_owned()),
            (capture, 2, "capture".to_owned()),
            (partial, 3, "refund".to_owned()),
            (rest, 4, "refund".to_owned()),
        ]
    );
    // A payment's steps count from 0, its creation's, and its facts from 1.
    assert_eq!(
        links_of(&pool, payment).await?,
        [
            (1, 1, "authorize".to_owned()),
            (2, 2, "capture".to_owned()),
            (3, 3, "refund".to_owned()),
            (4, 4, "refund".to_owned()),
        ]
    );
    // The records carry the PSP's business times, never a clock of ours (ADR-0022).
    let times: Vec<DateTime<Utc>> = theirs
        .facts
        .iter()
        .map(|fact| match fact.detail {
            Detail::Authorized { expires_at, .. } => expires_at,
            Detail::Captured { captured_at, .. } => captured_at,
            Detail::Refunded { refunded_at, .. } => refunded_at,
            ref other => panic!("this test made no {other:?}"),
        })
        .collect();
    let mut stored = vec![
        sqlx::query_scalar!(
            "SELECT expires_at FROM payment_authorizations WHERE payment_id = $1",
            payment.get()
        )
        .fetch_one(&pool)
        .await?,
        sqlx::query_scalar!(
            "SELECT captured_at FROM payment_captures WHERE payment_id = $1",
            payment.get()
        )
        .fetch_one(&pool)
        .await?,
    ];
    stored.extend(
        sqlx::query_scalar!(
            "SELECT r.refunded_at FROM payment_refunds r
               JOIN payment_transitions t ON t.id = r.transition_id
              WHERE r.payment_id = $1 ORDER BY t.seq",
            payment.get()
        )
        .fetch_all(&pool)
        .await?,
    );
    assert_eq!(stored, times);
    assert!(unbalanced_currencies(&pool).await?.is_empty());
    assert!(uncaused_postings(&pool).await?.is_empty());
    let postings = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM ledger_transactions"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(postings, 3, "the capture and the two refunds");
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_decline_a_void_and_a_lapse_make_their_steps(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let declined = world.created(1).await?;
    let authorize = world
        .ask(1, declined, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.decline(authorize);
    let voided = world.authorized(2).await?;
    let void = world.ask(2, voided, Ask::Void).await?;
    world.fake.approve(void);
    let lapsed = world.authorized(3).await?;
    world.fake.expire(lapsed.get());
    for payment in [declined, voided, lapsed] {
        assert_eq!(
            world.catch_up(payment).await?,
            CaughtUp::Current { applied: 1 }
        );
    }

    assert_eq!(
        steps_of(&pool, declined).await?,
        [step("create", 0, "created"), step("decline", 0, "failed")]
    );
    assert_eq!(
        decisions_of(&pool, declined).await?,
        [(authorize, 1, "decline".to_owned())]
    );
    assert_eq!(
        links_of(&pool, declined).await?,
        [(1, 1, "decline".to_owned())]
    );
    for (payment, kind) in [(voided, "void"), (lapsed, "expire")] {
        assert_eq!(
            steps_of(&pool, payment).await?,
            [
                step("create", 0, "created"),
                step("authorize", AUTHORIZED, "authorized"),
                step(kind, 0, "voided"),
            ]
        );
        assert_eq!(links_of(&pool, payment).await?[1], (2, 2, kind.to_owned()));
    }
    assert_eq!(
        decisions_of(&pool, voided).await?[1],
        (void, 2, "void".to_owned())
    );
    // A lapse answers no request.
    assert_eq!(decisions_of(&pool, lapsed).await?.len(), 1);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refusal_decides_its_request_and_makes_no_step(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let held = world.authorized(1).await?;
    let capture = world.ask(1, held, Ask::Capture(amount(AUTHORIZED))).await?;
    world.fake.refuse(capture);
    let kept = world.authorized(2).await?;
    let void = world.ask(2, kept, Ask::Void).await?;
    world.fake.refuse(void);
    let paid = world.captured(3).await?;
    let refund = world.ask(3, paid, Ask::Refund(amount(AUTHORIZED))).await?;
    world.fake.refuse(refund);
    for payment in [held, kept, paid] {
        assert_eq!(
            world.catch_up(payment).await?,
            CaughtUp::Current { applied: 1 }
        );
    }

    for (payment, request, kind, steps) in [
        (held, capture, "capture_refused", 2),
        (kept, void, "void_refused", 2),
        (paid, refund, "refund_refused", 3),
    ] {
        let position = i32::try_from(steps).expect("a small position");
        assert_eq!(steps_of(&pool, payment).await?.len(), steps, "{kind}");
        assert_eq!(
            kinds_of(&pool, payment).await?.last(),
            Some(&kind.to_owned())
        );
        assert_eq!(
            decisions_of(&pool, payment).await?.last(),
            Some(&(request, position, kind.to_owned()))
        );
        // A refusal makes no step, so nothing links to it.
        assert_eq!(links_of(&pool, payment).await?.len(), steps - 1, "{kind}");
    }
    // Decided, the capture is no longer in flight, so another may be asked.
    world.ask(4, held, Ask::Capture(amount(AUTHORIZED))).await?;
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_settlement_and_a_full_refund_apply_in_the_psps_order(pool: PgPool) -> TestResult {
    // ADR-0031's example: both orders are legal when the facts arrive, and only the
    // PSP's history tells them apart.
    let world = World::new(&pool).await?;
    let settled_first = world.captured(1).await?;
    let refunded_first = world.captured(2).await?;
    let first = world
        .ask(1, settled_first, Ask::Refund(amount(AUTHORIZED)))
        .await?;
    let second = world
        .ask(2, refunded_first, Ask::Refund(amount(AUTHORIZED)))
        .await?;
    world.fake.settle(settled_first.get());
    world.fake.approve(first);
    world.fake.approve(second);
    world.fake.settle(refunded_first.get());
    for payment in [settled_first, refunded_first] {
        assert_eq!(
            world.catch_up(payment).await?,
            CaughtUp::Current { applied: 2 }
        );
    }

    let paid = [
        step("create", 0, "created"),
        step("authorize", AUTHORIZED, "authorized"),
        step("capture", AUTHORIZED, "captured"),
    ];
    let mut then_settled = paid.to_vec();
    then_settled.extend([
        step("settle", 0, "settled"),
        step("refund", AUTHORIZED, "refunded"),
    ]);
    assert_eq!(steps_of(&pool, settled_first).await?, then_settled);
    // A settlement after a full refund is recorded without a step (ADR-0016).
    let mut then_refunded = paid.to_vec();
    then_refunded.push(step("refund", AUTHORIZED, "refunded"));
    assert_eq!(steps_of(&pool, refunded_first).await?, then_refunded);
    assert_eq!(
        kinds_of(&pool, refunded_first).await?,
        ["authorize", "capture", "refund", "settle"]
    );
    assert_eq!(
        links_of(&pool, refunded_first)
            .await?
            .into_iter()
            .map(|(_, position, _)| position)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_catch_up_applies_only_the_facts_after_those_held(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.authorized(1).await?;
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 0 }
    );
    let capture = world
        .ask(1, payment, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.approve(capture);
    world.fake.settle(payment.get());
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 2 }
    );
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 0 }
    );
    assert_eq!(
        kinds_of(&pool, payment).await?,
        ["authorize", "capture", "settle"]
    );
    assert_eq!(steps_of(&pool, payment).await?.len(), 4);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_read_older_than_the_facts_held_applies_nothing(pool: PgPool) -> TestResult {
    // Two catch-ups cross: the first reads, the second reads a longer history and
    // commits, and then the first applies (ADR-0041).
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .ask(1, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    let client = client_of(&world.fake);
    let early = read(&world.system, &client, payment).await?;
    world.fake.expire(payment.get());
    let late = read(&world.system, &client, payment).await?;
    assert_eq!(
        apply(&world.system, late).await?,
        CaughtUp::Current { applied: 2 }
    );

    // The early read holds fewer facts than are held now, but not fewer than were held
    // when it began: it is stale, and nothing changed at the PSP.
    assert_eq!(
        apply(&world.system, early).await?,
        CaughtUp::Current { applied: 0 }
    );
    assert_eq!(stop_of(&pool, payment).await?, None);
    assert_eq!(kinds_of(&pool, payment).await?, ["authorize", "expire"]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_history_shorter_than_the_facts_counted_before_it_was_read_has_changed(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let lost_one = world.authorized(1).await?;
    world.fake.expire(lost_one.get());
    assert_eq!(
        world.catch_up(lost_one).await?,
        CaughtUp::Current { applied: 1 }
    );
    let forgotten = world.authorized(2).await?;
    let scripted = ScriptedPsp::start().await;

    // The PSP serves the history without its last fact, which the service held before
    // this read began.
    let mut history = world.history(lost_one);
    history.facts.pop();
    scripted.answer(StatusCode::OK, None, serde_json::to_vec(&history)?);
    let read_lost = read(&world.system, &scripted.client(), lost_one).await?;
    assert_eq!(
        apply(&world.system, read_lost).await?,
        stopped(0, 2, StopReason::HistoryChanged)
    );
    // It forgot the other payment entirely: a 404, so a history without facts.
    scripted.answer(StatusCode::NOT_FOUND, None, Vec::new());
    assert_eq!(
        catch_up(&world.system, &scripted.client(), forgotten).await?,
        stopped(0, 1, StopReason::HistoryChanged)
    );

    assert_eq!(
        stop_of(&pool, lost_one).await?,
        Some((2, "history_changed".to_owned()))
    );
    assert_eq!(
        stop_of(&pool, forgotten).await?,
        Some((1, "history_changed".to_owned()))
    );
    // What was held stays held.
    assert_eq!(kinds_of(&pool, lost_one).await?, ["authorize", "expire"]);
    assert_eq!(kinds_of(&pool, forgotten).await?, ["authorize"]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_fact_whose_id_changed_stops_the_payment_at_its_position(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let second = world.authorized(1).await?;
    world.fake.expire(second.get());
    assert_eq!(
        world.catch_up(second).await?,
        CaughtUp::Current { applied: 1 }
    );
    let first = world.authorized(2).await?;
    world.fake.expire(first.get());
    let scripted = ScriptedPsp::start().await;

    let mut history = world.history(second);
    history.facts[1].id = "fct_other".to_owned();
    scripted.answer(StatusCode::OK, None, serde_json::to_vec(&history)?);
    assert_eq!(
        catch_up(&world.system, &scripted.client(), second).await?,
        stopped(0, 2, StopReason::HistoryChanged)
    );
    // A change at a position held stops the payment before any new fact applies.
    let mut history = world.history(first);
    history.facts[0].id = "fct_other".to_owned();
    scripted.answer(StatusCode::OK, None, serde_json::to_vec(&history)?);
    assert_eq!(
        catch_up(&world.system, &scripted.client(), first).await?,
        stopped(0, 1, StopReason::HistoryChanged)
    );
    assert_eq!(kinds_of(&pool, first).await?, ["authorize"]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_fact_without_a_place_stops_the_payment_after_the_facts_before_it(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    // A settlement of a payment that was never captured: no step takes it.
    let unsettled = world.created(1).await?;
    let authorize = world
        .ask(1, unsettled, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    world.fake.append(unsettled.get(), Detail::Settled);
    assert_eq!(
        world.catch_up(unsettled).await?,
        stopped(1, 2, StopReason::NoPlace)
    );
    assert_eq!(kinds_of(&pool, unsettled).await?, ["authorize"]);
    assert_eq!(
        steps_of(&pool, unsettled).await?.last(),
        Some(&step("authorize", REQUESTED, "authorized"))
    );

    // A kind that the service does not know.
    let unknown = world.authorized(2).await?;
    let mut history = serde_json::to_value(world.history(unknown))?;
    history["facts"]
        .as_array_mut()
        .expect("an array")
        .push(json!({"id": "fct_chargeback", "position": 2, "kind": "chargeback"}));
    let scripted = ScriptedPsp::start().await;
    scripted.answer(StatusCode::OK, None, serde_json::to_vec(&history)?);
    assert_eq!(
        catch_up(&world.system, &scripted.client(), unknown).await?,
        stopped(0, 2, StopReason::NoPlace)
    );

    // An amount that no step can hold is checked before the request that it names.
    let empty = world.created(3).await?;
    world.fake.append(
        empty.get(),
        Detail::Authorized {
            request_id: Uuid::now_v7(),
            amount: 0,
            expires_at: at(),
        },
    );
    assert_eq!(
        world.catch_up(empty).await?,
        stopped(0, 1, StopReason::NoPlace)
    );

    // A second settlement: only a capture that the PSP never settled can be settled
    // after its full refund without a step (ADR-0016).
    let twice = world.captured(4).await?;
    world.fake.settle(twice.get());
    let refund = world.ask(4, twice, Ask::Refund(amount(AUTHORIZED))).await?;
    world.fake.approve(refund);
    world.fake.append(twice.get(), Detail::Settled);
    assert_eq!(
        world.catch_up(twice).await?,
        stopped(2, 5, StopReason::NoPlace)
    );
    assert_eq!(
        kinds_of(&pool, twice).await?,
        ["authorize", "capture", "settle", "refund"]
    );
    for (payment, position) in [(unsettled, 2), (unknown, 2), (empty, 1), (twice, 5)] {
        assert_eq!(
            stop_of(&pool, payment).await?,
            Some((position, "no_place".to_owned()))
        );
    }
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_decision_that_contradicts_the_requests_stops_the_payment(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let mut expected = Vec::new();

    // A request that the payment never recorded.
    let unknown = world.created(1).await?;
    world.fake.append(
        unknown.get(),
        Detail::Declined {
            request_id: Uuid::now_v7(),
        },
    );
    expected.push((unknown, stopped(0, 1, StopReason::Contradiction)));

    // A request decided already, in an earlier catch-up or in the same one.
    let again = world.authorized(2).await?;
    world.fake.append(
        again.get(),
        Detail::Declined {
            request_id: request_id(20).get(),
        },
    );
    expected.push((again, stopped(0, 2, StopReason::Contradiction)));
    let twice = world.created(3).await?;
    let authorize = world
        .ask(30, twice, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    world.fake.append(
        twice.get(),
        Detail::Declined {
            request_id: authorize,
        },
    );
    expected.push((twice, stopped(1, 2, StopReason::Contradiction)));

    // A decision of another kind than its request.
    let other_kind = world.authorized(4).await?;
    let capture = world
        .ask(41, other_kind, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.append(
        other_kind.get(),
        Detail::Voided {
            request_id: capture,
        },
    );
    expected.push((other_kind, stopped(0, 2, StopReason::Contradiction)));

    // A capture of another amount than its request.
    let other_amount = world.authorized(5).await?;
    let capture = world
        .ask(51, other_amount, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.append(
        other_amount.get(),
        Detail::Captured {
            request_id: capture,
            amount: AUTHORIZED - 1,
            captured_at: at(),
        },
    );
    expected.push((other_amount, stopped(0, 2, StopReason::Contradiction)));

    // An authorization above its request.
    let above = world.created(6).await?;
    let authorize = world
        .ask(60, above, Ask::Authorize(amount(REQUESTED - 1)))
        .await?;
    world.fake.append(
        above.get(),
        Detail::Authorized {
            request_id: authorize,
            amount: REQUESTED,
            expires_at: at(),
        },
    );
    expected.push((above, stopped(0, 1, StopReason::Contradiction)));

    // Another payment's request.
    let owner = world.created(7).await?;
    let elsewhere = world
        .ask(70, owner, Ask::Authorize(amount(REQUESTED)))
        .await?;
    let stranger = world.created(8).await?;
    world.fake.append(
        stranger.get(),
        Detail::Declined {
            request_id: elsewhere,
        },
    );
    expected.push((stranger, stopped(0, 1, StopReason::Contradiction)));

    for (payment, outcome) in expected {
        assert_eq!(world.catch_up(payment).await?, outcome, "{payment}");
        let CaughtUp::Stopped { stop, .. } = outcome else {
            unreachable!("every case stops")
        };
        assert_eq!(
            stop_of(&pool, payment).await?,
            Some((stop.position, "contradiction".to_owned()))
        );
    }
    // The authorization before the second decision of one request stays applied.
    assert_eq!(kinds_of(&pool, twice).await?, ["authorize"]);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_stopped_payment_takes_no_more_facts(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    world.fake.append(
        payment.get(),
        Detail::Declined {
            request_id: Uuid::now_v7(),
        },
    );
    assert_eq!(
        world.catch_up(payment).await?,
        stopped(0, 1, StopReason::Contradiction)
    );
    world.fake.append(payment.get(), Detail::Expired);
    assert_eq!(world.catch_up(payment).await?, CaughtUp::AlreadyStopped);
    let client = client_of(&world.fake);
    let read = read(&world.system, &client, payment).await?;
    assert_eq!(apply(&world.system, read).await?, CaughtUp::AlreadyStopped);
    assert!(facts_of(&pool, payment).await?.is_empty());
    assert_eq!(
        stop_of(&pool, payment).await?,
        Some((1, "contradiction".to_owned()))
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn copies_of_one_catch_up_racing_apply_each_fact_once(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .ask(1, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    world.fake.expire(payment.get());
    let results = join_all((0..8).map(|_| world.catch_up(payment))).await;

    let mut applied = 0;
    for result in results {
        match result {
            Ok(CaughtUp::Current { applied: count }) => applied += count,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(applied, 2);
    assert_eq!(kinds_of(&pool, payment).await?, ["authorize", "expire"]);
    assert_eq!(steps_of(&pool, payment).await?.len(), 3);
    assert_eq!(stop_of(&pool, payment).await?, None);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_catch_up_waits_for_the_payments_lock(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .ask(1, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    let mut holder = pool.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", lock_key(payment))
        .execute(&mut *holder)
        .await?;

    let (result, released) = join(world.catch_up(payment), async {
        wait_for_lock_wait(&pool, "advisory").await?;
        // While the lock is held, nothing is applied.
        assert!(facts_of(&pool, payment).await?.is_empty());
        commit(holder).await?;
        Ok::<_, Box<dyn Error>>(())
    })
    .await;
    released?;
    assert_eq!(result?, CaughtUp::Current { applied: 1 });
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_failed_read_applies_nothing(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .ask(1, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    world
        .fake
        .fail_next(Route::History, [Failure::InternalError]);
    match world.catch_up(payment).await {
        Err(CatchUpError::Psp {
            source: ReadError::Status { status },
            ..
        }) => assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR),
        other => panic!("{other:?}"),
    }
    // An answer wrong as a whole applies nothing either, and stops nothing (ADR-0041).
    let scripted = ScriptedPsp::start().await;
    let mut history = serde_json::to_value(world.history(payment))?;
    history["facts"][0]["position"] = json!(2);
    scripted.answer(StatusCode::OK, None, serde_json::to_vec(&history)?);
    match catch_up(&world.system, &scripted.client(), payment).await {
        Err(CatchUpError::Psp {
            source: ReadError::Unreadable { source },
            ..
        }) => assert_eq!(
            source,
            Unreadable::Position {
                expected: 1,
                found: 2
            }
        ),
        other => panic!("{other:?}"),
    }
    assert!(facts_of(&pool, payment).await?.is_empty());
    assert_eq!(stop_of(&pool, payment).await?, None);
    // The next read applies what the failed ones did not.
    assert_eq!(
        world.catch_up(payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_payment_that_does_not_exist_is_not_found(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let unknown = payment_id(99);
    match world.catch_up(unknown).await {
        Err(CatchUpError::NotFound { payment }) => assert_eq!(payment, unknown),
        other => panic!("{other:?}"),
    }
    world.close().await;
    Ok(())
}
