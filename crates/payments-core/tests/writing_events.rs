// Author: Nicola Antonello

//! Events and their deliveries, as the catch-up writes them (ADR-0049, ADR-0053,
//! ADR-0057): one event for each fact that it applies, and one delivery for each
//! endpoint of the payment's merchant enabled when the catch-up reads them, in the
//! fact's transaction.
//!
//! Each test catches up as the system role against a fake PSP over real HTTP
//! (ADR-0029). Endpoints are registered and disabled with raw SQL as the system role,
//! as the operator's routes will write them (ADR-0050).

mod common;

use std::collections::BTreeSet;
use std::error::Error;

use common::payments::{AUTHORIZED, REQUESTED, create_command, fee_rate};
use common::psp::{client_of, send};
use common::{merchant_id, request_id, runtime_pool, tenant_pool, transition_id};
use mock_psp::MockPsp;
use mock_psp::wire::Detail;
use payments_core::catch_up::{CaughtUp, Stop, StopReason, catch_up};
use payments_core::deliveries::Handover;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{StepCommand, cancel, create_payment, register_merchant};
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// The address of a merchant's handler, as the operator would register it.
const URL: &str = "http://localhost:8080/webhooks";

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
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

    /// Catches the payment up, and checks that it applied `applied` new facts.
    async fn caught_up(&self, payment: PaymentId, applied: u32) -> TestResult {
        let caught_up = catch_up(
            &self.system,
            &client_of(&self.fake),
            payment,
            &Handover::none(),
        )
        .await?;
        assert_eq!(caught_up, CaughtUp::Current { applied }, "{payment}");
        Ok(())
    }

    /// Payment `n`, authorized by the PSP for [`AUTHORIZED`] and caught up. Its requests
    /// are numbered from `10 * n`.
    async fn authorized(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.created(n).await?;
        let authorize = self
            .ask(10 * n, payment, Ask::Authorize(amount(REQUESTED)))
            .await?;
        self.fake.approve_for(authorize, AUTHORIZED);
        self.caught_up(payment, 1).await?;
        Ok(payment)
    }

    /// Payment `n`, captured by the PSP for [`AUTHORIZED`] and caught up.
    async fn captured(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.authorized(n).await?;
        let capture = self
            .ask(10 * n + 1, payment, Ask::Capture(amount(AUTHORIZED)))
            .await?;
        self.fake.approve(capture);
        self.caught_up(payment, 1).await?;
        Ok(payment)
    }

    async fn close(self) {
        self.system.close().await;
        self.tenant.close().await;
    }
}

/// Registers an endpoint of this merchant and returns its ID.
async fn register_endpoint(system: &PgPool, merchant: MerchantId) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
        id,
        merchant.get(),
        URL
    )
    .execute(system)
    .await?;
    Ok(id)
}

/// Disables an endpoint, as the operator's route will.
async fn disable_endpoint(system: &PgPool, endpoint: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason) VALUES ($1, $2)",
        endpoint,
        "operator"
    )
    .execute(system)
    .await?;
    Ok(())
}

/// The kinds of the facts held, by position.
async fn kinds_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        "SELECT kind FROM payment_facts WHERE payment_id = $1 ORDER BY position",
        payment.get()
    )
    .fetch_all(pool)
    .await
}

/// The payment's events: the position of the fact that each announces, and its merchant.
async fn events_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Vec<(i32, Uuid)>> {
    let rows = sqlx::query!(
        "SELECT position, merchant_id FROM payment_events WHERE payment_id = $1 ORDER BY position",
        payment.get()
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.position, row.merchant_id))
        .collect())
}

/// Every delivery: the payment and the position of its event's fact, its endpoint, and
/// its merchant.
async fn deliveries(pool: &PgPool) -> sqlx::Result<BTreeSet<(Uuid, i32, Uuid, Uuid)>> {
    let rows = sqlx::query!(
        "SELECT e.payment_id, e.position, d.endpoint_id, d.merchant_id
           FROM webhook_deliveries d
           JOIN payment_events e ON e.id = d.event_id"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.payment_id,
                row.position,
                row.endpoint_id,
                row.merchant_id,
            )
        })
        .collect())
}

async fn count_events(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM payment_events"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn each_fact_that_the_catch_up_applies_has_one_event(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let mut expected = Vec::new();

    // A capture and two refunds, the second of the rest, which ends the payment.
    let refunded = world.captured(1).await?;
    let partial = world.ask(12, refunded, Ask::Refund(amount(3_000))).await?;
    let rest = world.ask(13, refunded, Ask::Refund(amount(5_000))).await?;
    world.fake.approve(partial);
    world.fake.approve(rest);
    world.caught_up(refunded, 2).await?;
    expected.push((refunded, vec!["authorize", "capture", "refund", "refund"]));

    let declined = world.created(2).await?;
    let authorize = world
        .ask(20, declined, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.decline(authorize);
    world.caught_up(declined, 1).await?;
    expected.push((declined, vec!["decline"]));

    let voided = world.authorized(3).await?;
    let void = world.ask(31, voided, Ask::Void).await?;
    world.fake.approve(void);
    world.caught_up(voided, 1).await?;
    expected.push((voided, vec!["authorize", "void"]));

    let lapsed = world.authorized(4).await?;
    world.fake.expire(lapsed.get());
    world.caught_up(lapsed, 1).await?;
    expected.push((lapsed, vec!["authorize", "expire"]));

    // Refusals close their requests without a step (ADR-0037). A capture and a void
    // are never in flight together, so each has its payment.
    let uncaptured = world.authorized(5).await?;
    let capture = world
        .ask(51, uncaptured, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.refuse(capture);
    world.caught_up(uncaptured, 1).await?;
    expected.push((uncaptured, vec!["authorize", "capture_refused"]));
    let unvoided = world.authorized(6).await?;
    let void = world.ask(61, unvoided, Ask::Void).await?;
    world.fake.refuse(void);
    world.caught_up(unvoided, 1).await?;
    expected.push((unvoided, vec!["authorize", "void_refused"]));

    // A refused refund, then a settlement that makes its step.
    let settled = world.captured(7).await?;
    let refund = world
        .ask(72, settled, Ask::Refund(amount(AUTHORIZED)))
        .await?;
    world.fake.refuse(refund);
    world.fake.settle(settled.get());
    world.caught_up(settled, 2).await?;
    expected.push((
        settled,
        vec!["authorize", "capture", "refund_refused", "settle"],
    ));

    // A settlement after a full refund, recorded without a step (ADR-0016).
    let late = world.captured(8).await?;
    let refund = world.ask(82, late, Ask::Refund(amount(AUTHORIZED))).await?;
    world.fake.approve(refund);
    world.fake.settle(late.get());
    world.caught_up(late, 2).await?;
    expected.push((late, vec!["authorize", "capture", "refund", "settle"]));

    let mut every_kind = BTreeSet::new();
    let mut facts = 0;
    for (payment, kinds) in expected {
        assert_eq!(kinds_of(&pool, payment).await?, kinds, "{payment}");
        // One event for each fact, at its position, of the payment's merchant.
        let announced: Vec<_> = (1..)
            .zip(&kinds)
            .map(|(position, _)| (position, world.merchant.get()))
            .collect();
        assert_eq!(events_of(&pool, payment).await?, announced, "{payment}");
        facts += kinds.len();
        every_kind.extend(kinds);
    }
    assert_eq!(every_kind.len(), 10, "every kind of fact: {every_kind:?}");
    assert_eq!(count_events(&pool).await?, i64::try_from(facts)?);
    // A merchant without endpoints gets its events and no delivery.
    assert!(deliveries(&pool).await?.is_empty());
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_creation_a_cancellation_and_a_stopping_fact_have_no_event(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let created = world.created(1).await?;

    let cancelled = world.created(2).await?;
    let command = StepCommand {
        step: transition_id(100),
        payment: cancelled,
    };
    cancel(world.me(), &command).await?;

    // A settlement of a payment that was never captured: no step takes it, so the
    // catch-up stops the payment at it, after the authorization before it.
    let stopped = world.created(3).await?;
    let authorize = world
        .ask(30, stopped, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.approve(authorize);
    world.fake.append(stopped.get(), Detail::Settled);
    let caught_up = catch_up(
        &world.system,
        &client_of(&world.fake),
        stopped,
        &Handover::none(),
    )
    .await?;
    let stop = Stop {
        position: 2,
        reason: StopReason::NoPlace,
    };
    assert_eq!(caught_up, CaughtUp::Stopped { applied: 1, stop });

    assert!(events_of(&pool, created).await?.is_empty());
    assert!(events_of(&pool, cancelled).await?.is_empty());
    assert_eq!(
        events_of(&pool, stopped).await?,
        [(1, world.merchant.get())]
    );
    assert_eq!(count_events(&pool).await?, 1);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_event_goes_to_each_enabled_endpoint_of_its_merchant(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let other = merchant_id(2);
    register_merchant(&world.system, other, fee_rate()).await?;
    let first = register_endpoint(&world.system, world.merchant).await?;
    let second = register_endpoint(&world.system, world.merchant).await?;
    let disabled = register_endpoint(&world.system, world.merchant).await?;
    disable_endpoint(&world.system, disabled).await?;
    register_endpoint(&world.system, other).await?;

    let payment = world.authorized(1).await?;

    let merchant = world.merchant.get();
    let expected = BTreeSet::from([
        (payment.get(), 1, first, merchant),
        (payment.get(), 1, second, merchant),
    ]);
    // These are every delivery: none to the disabled endpoint, and none to the other
    // merchant's.
    assert_eq!(deliveries(&pool).await?, expected);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_endpoint_gets_the_events_written_after_it_is_registered(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    // The authorization's event is written while the merchant has no endpoint.
    let payment = world.authorized(1).await?;
    assert!(deliveries(&pool).await?.is_empty());

    let first = register_endpoint(&world.system, world.merchant).await?;
    let capture = world
        .ask(11, payment, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.approve(capture);
    world.caught_up(payment, 1).await?;

    // Two refunds in one catch-up: each event goes to each endpoint.
    let second = register_endpoint(&world.system, world.merchant).await?;
    let partial = world.ask(12, payment, Ask::Refund(amount(3_000))).await?;
    let rest = world.ask(13, payment, Ask::Refund(amount(5_000))).await?;
    world.fake.approve(partial);
    world.fake.approve(rest);
    world.caught_up(payment, 2).await?;

    let (id, merchant) = (payment.get(), world.merchant.get());
    assert_eq!(
        deliveries(&pool).await?,
        BTreeSet::from([
            (id, 2, first, merchant),
            (id, 3, first, merchant),
            (id, 3, second, merchant),
            (id, 4, first, merchant),
            (id, 4, second, merchant),
        ])
    );
    world.close().await;
    Ok(())
}
