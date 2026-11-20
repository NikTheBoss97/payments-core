// Author: Nicola Antonello

//! The catch-up hands the deliveries that it committed to the relay's task (ADR-0052,
//! ADR-0057): once its `COMMIT` is confirmed and never before, also when it stops the
//! payment after facts that it applied, through a channel that drops what it cannot hold,
//! which the relay's search then finds (ADR-0062). Events come from catch-ups against a
//! fake PSP over real HTTP, as in `writing_events.rs`.

mod common;

use std::error::Error;

use common::crashes::{AT_COMMIT, install_crash_points, plan_death};
use common::outbox::{deliveries_of, handed, insert_endpoint};
use common::payments::{REQUESTED, create_command, fee_rate};
use common::psp::{client_of, send};
use common::{
    endpoint_id, merchant_id, request_id, runtime_pool, runtime_pool_of_one, tenant_pool,
};
use mock_psp::MockPsp;
use mock_psp::wire::Detail;
use payments_core::catch_up::{CatchUpError, CaughtUp, Stop, StopReason, catch_up};
use payments_core::deliveries::{HANDOVERS, Handover, search};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::relay::Search;
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use tokio::sync::mpsc;

type TestResult = Result<(), Box<dyn Error>>;

/// Where the endpoints are. No test here posts to them.
const URL: &str = "http://localhost:8080/webhooks";

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// A merchant with two endpoints, its pool, the system role's pool, and a fake PSP.
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
        for n in 1..=2 {
            insert_endpoint(&system, endpoint_id(n), merchant.get(), URL).await?;
        }
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

    /// Payment `n`, whose authorization for [`REQUESTED`] the PSP approved, not caught up
    /// yet.
    async fn approved(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let command = create_command(n, self.merchant);
        create_payment(self.me(), &command).await?;
        let ask = RecordRequest {
            request: request_id(n),
            payment: command.id,
            ask: Ask::Authorize(amount(REQUESTED)),
        };
        record_request(self.me(), &ask).await?;
        send(&self.fake, &ask).await;
        self.fake.approve(ask.request.get());
        Ok(command.id)
    }

    async fn close(self) {
        self.system.close().await;
        self.tenant.close().await;
    }
}

#[sqlx::test]
async fn a_catch_up_hands_over_each_delivery_that_it_committed(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.approved(1).await?;
    let (channel, mut handed_over) = mpsc::channel(HANDOVERS);
    let handover = Handover::new(channel);
    let client = client_of(&world.fake);

    assert_eq!(
        catch_up(&world.system, &client, payment, &handover).await?,
        CaughtUp::Current { applied: 1 }
    );
    let committed = deliveries_of(&world.system, payment).await?;
    assert_eq!(committed.len(), 2, "one delivery to each endpoint");
    assert_eq!(handed(&mut handed_over), committed);
    // A catch-up that applies nothing hands over nothing.
    assert_eq!(
        catch_up(&world.system, &client, payment, &handover).await?,
        CaughtUp::Current { applied: 0 }
    );
    assert!(handed(&mut handed_over).is_empty());
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_catch_up_whose_commit_fails_hands_over_nothing(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    install_crash_points(&pool).await?;
    let one = runtime_pool_of_one(&pool).await?;
    let payment = world.approved(1).await?;
    let (channel, mut handed_over) = mpsc::channel(HANDOVERS);
    let handover = Handover::new(channel);
    let client = client_of(&world.fake);

    // The catch-up writes the event and its deliveries, then dies during its COMMIT.
    plan_death(&one, AT_COMMIT).await?;
    let err = catch_up(&one, &client, payment, &handover)
        .await
        .unwrap_err();
    assert!(matches!(err, CatchUpError::NotConfirmed { .. }), "{err:?}");
    assert!(handed(&mut handed_over).is_empty());
    // The next catch-up applies the fact again, and hands over what it committed.
    assert_eq!(
        catch_up(&one, &client, payment, &handover).await?,
        CaughtUp::Current { applied: 1 }
    );
    assert_eq!(
        handed(&mut handed_over),
        deliveries_of(&world.system, payment).await?
    );
    one.close().await;
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_catch_up_that_stops_hands_over_the_deliveries_of_the_facts_it_applied(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.approved(1).await?;
    // A settlement of a payment never captured: no step takes it, so the catch-up stops the
    // payment there, and commits the authorization before it.
    world.fake.append(payment.get(), Detail::Settled);
    let (channel, mut handed_over) = mpsc::channel(HANDOVERS);
    let handover = Handover::new(channel);

    let stop = Stop {
        position: 2,
        reason: StopReason::NoPlace,
    };
    assert_eq!(
        catch_up(&world.system, &client_of(&world.fake), payment, &handover).await?,
        CaughtUp::Stopped { applied: 1, stop }
    );
    let committed = deliveries_of(&world.system, payment).await?;
    assert_eq!(committed.len(), 2, "one delivery to each endpoint");
    assert_eq!(handed(&mut handed_over), committed);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_catch_up_whose_stop_fails_to_commit_hands_over_nothing(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    install_crash_points(&pool).await?;
    let one = runtime_pool_of_one(&pool).await?;
    let payment = world.approved(1).await?;
    world.fake.append(payment.get(), Detail::Settled);
    let (channel, mut handed_over) = mpsc::channel(HANDOVERS);
    let handover = Handover::new(channel);
    let client = client_of(&world.fake);

    // The catch-up applies the authorization, stops the payment at the settlement, and
    // dies during the COMMIT of both.
    plan_death(&one, AT_COMMIT).await?;
    let err = catch_up(&one, &client, payment, &handover)
        .await
        .unwrap_err();
    assert!(matches!(err, CatchUpError::NotConfirmed { .. }), "{err:?}");
    assert!(handed(&mut handed_over).is_empty());
    // The next catch-up stops it again, and hands over what it committed.
    let stop = Stop {
        position: 2,
        reason: StopReason::NoPlace,
    };
    assert_eq!(
        catch_up(&one, &client, payment, &handover).await?,
        CaughtUp::Stopped { applied: 1, stop }
    );
    assert_eq!(
        handed(&mut handed_over),
        deliveries_of(&world.system, payment).await?
    );
    one.close().await;
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_catch_up_that_applies_two_facts_hands_over_the_deliveries_of_both(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.approved(1).await?;
    // The authorization, then its expiry: two facts, so two events, in one catch-up.
    world.fake.append(payment.get(), Detail::Expired);
    let (channel, mut handed_over) = mpsc::channel(HANDOVERS);
    let handover = Handover::new(channel);

    assert_eq!(
        catch_up(&world.system, &client_of(&world.fake), payment, &handover).await?,
        CaughtUp::Current { applied: 2 }
    );
    let committed = deliveries_of(&world.system, payment).await?;
    assert_eq!(
        committed.len(),
        4,
        "one delivery to each endpoint, for each event"
    );
    assert_eq!(handed(&mut handed_over), committed);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_full_handover_drops_the_rest_and_the_next_search_finds_them(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.approved(1).await?;
    // The relay's search before the catch-up: the next one reads what commits after it.
    let (before, _) = search(&world.system, &Search::Full).await?;
    let (channel, mut handed_over) = mpsc::channel(1);
    let handover = Handover::new(channel);

    catch_up(&world.system, &client_of(&world.fake), payment, &handover).await?;
    let committed = deliveries_of(&world.system, payment).await?;
    let taken = handed(&mut handed_over);
    assert_eq!(taken.len(), 1, "a channel of one holds one");
    assert!(committed.contains(&taken[0]));
    // The one dropped is found by the next search, with the other written since.
    let (_, found) = search(&world.system, &Search::Since(before)).await?;
    assert_eq!(found, committed);
    world.close().await;
    Ok(())
}
