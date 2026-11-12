// Author: Nicola Antonello

//! The sender's task (ADR-0043, ADR-0044), run in the test's process on a short cadence,
//! against a fake PSP over real HTTP.
//!
//! Each wait is bounded. A step makes its first attempt at a request that it finds after
//! a jittered delay of up to two seconds, since only a request handed over is sent at
//! once (ADR-0042).

mod common;

use std::convert::Infallible;
use std::error::Error;
use std::time::{Duration, Instant};

use common::payments::{AUTHORIZED, REQUESTED, create_command, fee_rate, status_of};
use common::psp::{client_of, send};
use common::{merchant_id, request_id, runtime_pool, tenant_pool};
use mock_psp::MockPsp;
use payments_core::background::{Cadence, HANDOVERS, run};
use payments_core::catch_up::{CaughtUp, catch_up};
use payments_core::ids::{PaymentId, RequestId};
use payments_core::lifecycle::PaymentStatus;
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::sender::Sender;
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// Long enough that only the first step or pass runs during a test.
const HOUR: Duration = Duration::from_secs(3600);
/// Short enough that a test sees many steps or passes.
const SHORT: Duration = Duration::from_millis(20);

/// The system role's pool, which the task runs on, a merchant's pool, and a fake PSP.
struct World {
    system: PgPool,
    tenant: PgPool,
    fake: MockPsp,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            system: runtime_pool(pool).await?,
            tenant: tenant_pool(pool).await?,
            fake: MockPsp::start().await?,
        })
    }

    /// Payment `n` of merchant `n`, created and asked to be authorized as that merchant.
    async fn requested(&self, n: u32) -> Result<(PaymentId, RecordRequest), Box<dyn Error>> {
        let merchant = merchant_id(n);
        register_merchant(&self.system, merchant, fee_rate()).await?;
        let command = create_command(n, merchant);
        create_payment(Tenant::new(&self.tenant, merchant), &command).await?;
        let request = RecordRequest {
            request: request_id(n),
            payment: command.id,
            ask: Ask::Authorize(PositiveAmount::new(REQUESTED)?),
        };
        record_request(Tenant::new(&self.tenant, merchant), &request).await?;
        Ok((command.id, request))
    }

    /// Asks, as merchant `n`, for a capture of the payment, which the fake then holds.
    async fn capture(&self, n: u32, payment: PaymentId) -> Result<Uuid, Box<dyn Error>> {
        let request = RecordRequest {
            request: request_id(100 + n),
            payment,
            ask: Ask::Capture(PositiveAmount::new(AUTHORIZED)?),
        };
        record_request(Tenant::new(&self.tenant, merchant_id(n)), &request).await?;
        send(&self.fake, &request).await;
        Ok(request.request.get())
    }

    /// The task, on this world's system pool and fake, at this cadence, with no route to
    /// hand it a request.
    fn task(&self, cadence: Cadence) -> JoinHandle<Infallible> {
        self.handing_over(cadence).0
    }

    /// The task, and the routes' end of its handovers.
    fn handing_over(&self, cadence: Cadence) -> (JoinHandle<Infallible>, mpsc::Sender<RequestId>) {
        let sender = Sender::new(client_of(&self.fake), Utc::now());
        let (handovers, handed_over) = mpsc::channel(HANDOVERS);
        let task = tokio::spawn(run(self.system.clone(), sender, cadence, handed_over));
        (task, handovers)
    }

    async fn close(self, task: JoinHandle<Infallible>) {
        task.abort();
        self.system.close().await;
        self.tenant.close().await;
    }
}

/// Waits, at most 10 seconds, until the payment has this status.
async fn until_status(
    system: &PgPool,
    payment: PaymentId,
    status: PaymentStatus,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while status_of(system, payment).await? != status {
        assert!(
            Instant::now() < deadline,
            "payment {payment} never became {status:?}"
        );
        sleep(SHORT).await;
    }
    Ok(())
}

/// The answer recorded for a request, if any.
async fn answer_of(system: &PgPool, request: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar!(
        "SELECT answer FROM payment_request_answers WHERE request_id = $1",
        request
    )
    .fetch_optional(system)
    .await
}

#[sqlx::test]
async fn a_step_sends_a_request_that_it_finds_and_records_the_answer(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let (_, request) = world.requested(1).await?;
    let request = request.request.get();
    let task = world.task(Cadence {
        step: SHORT,
        pass: HOUR,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while answer_of(&world.system, request).await?.is_none() {
        assert!(Instant::now() < deadline, "the request was never sent");
        sleep(SHORT).await;
    }
    assert_eq!(
        answer_of(&world.system, request).await?.as_deref(),
        Some("acknowledged")
    );
    // The steps that follow leave a request with an answer.
    sleep(10 * SHORT).await;
    assert_eq!(world.fake.times_taken(request), 1);
    world.close(task).await;
    Ok(())
}

#[sqlx::test]
async fn the_first_pass_runs_as_the_task_starts(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let (payment, authorize) = world.requested(1).await?;
    send(&world.fake, &authorize).await;
    world.fake.approve_for(authorize.request.get(), AUTHORIZED);
    let client = client_of(&world.fake);
    assert_eq!(
        catch_up(&world.system, &client, payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    let capture = world.capture(1, payment).await?;
    world.fake.approve(capture);
    assert_eq!(
        catch_up(&world.system, &client, payment).await?,
        CaughtUp::Current { applied: 1 }
    );
    // The PSP settles, and its notification never comes. No request waits, so no step
    // can catch the payment up: only a pass can.
    world.fake.settle(payment.get());
    let task = world.task(Cadence {
        step: HOUR,
        pass: HOUR,
    });
    until_status(&world.system, payment, PaymentStatus::Settled).await?;
    world.close(task).await;
    Ok(())
}

#[sqlx::test]
async fn passes_follow_one_another_at_their_cadence(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let (payment, authorize) = world.requested(1).await?;
    send(&world.fake, &authorize).await;
    let task = world.task(Cadence {
        step: HOUR,
        pass: SHORT,
    });
    // The first pass found nothing decided. The PSP decides later, with no
    // notification, and a later pass applies it.
    sleep(5 * SHORT).await;
    world.fake.approve_for(authorize.request.get(), AUTHORIZED);
    until_status(&world.system, payment, PaymentStatus::Authorized).await?;
    world.close(task).await;
    Ok(())
}

#[sqlx::test]
async fn a_handover_takes_a_step_between_ticks(pool: PgPool) -> TestResult {
    // Steps an hour apart: once the first has run, with nothing to find, only a handover can
    // make another (ADR-0043).
    let world = World::new(&pool).await?;
    let (task, handovers) = world.handing_over(Cadence {
        step: HOUR,
        pass: HOUR,
    });
    sleep(Duration::from_secs(1)).await;
    let (_, request) = world.requested(1).await?;
    handovers.send(request_id(1)).await?;

    // Sent at once, and its answer recorded, an hour before the next tick.
    let deadline = Instant::now() + Duration::from_secs(10);
    while answer_of(&world.system, request.request.get())
        .await?
        .as_deref()
        != Some("acknowledged")
    {
        assert!(Instant::now() < deadline, "the handover made no step");
        sleep(SHORT).await;
    }
    world.close(task).await;
    Ok(())
}
