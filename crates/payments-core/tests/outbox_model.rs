// Author: Nicola Antonello

//! Random runs of the outbox against a small model (ADR-0054, ADR-0065; invariants I6, I7
//! and I10).
//!
//! Each run draws its operations from a fixed seed. Catch-ups apply the PSP's approval of
//! a new payment of one of three merchants: most hand their deliveries over to the relay's
//! task, some lose the handover, some die at a crash point and are caught up later, and some
//! are held open by a lock on `webhook_deliveries` while the task searches, after a later
//! transaction has completed, so that the search's snapshot lists theirs as in progress, and
//! half of those lose their handover too, so that only the next search can find them. The
//! task ticks, takes handovers, reads its attempts' ends, and restarts, dropping its
//! attempts and all that it knew. For a while the database refuses its records of
//! successes. Endpoints are registered, disabled by the operator, or set to answer `200`,
//! `500`, `410`, `200` after the client's timeout, or `503` with `Retry-After`; each is a
//! fake merchant of its own, over real HTTP. A run ends with every endpoint answering `200`
//! while the task steps until nothing is unfinished.
//!
//! The model holds, for each event, the endpoints that must get it: its merchant's, among
//! those that the run registered, without a disablement when its catch-up ran. At the end
//! the deliveries are exactly those; each has a success or its endpoint a disablement; each
//! disablement has its cause, the operator's choice or a `410`; each success follows an
//! answer of `2xx` that the task could read, not one after its timeout; and each fake
//! received only its own merchant's events and only events that a catch-up wrote. Across a
//! batch's runs, every operation and outcome must have come up.
//!
//! The times stay near the clock, since a fake refuses a signature more than five minutes
//! from its own (ADR-0065): delays are drawn under three seconds, and `Retry-After` asks for
//! three at most. The operations and their choices come from one generator and the task's
//! draws from another, so a seed fixes the operations; the threads' timing it does not fix,
//! so a failure may need several runs to recur: it is a suspected race, never noise.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error;
use std::ops::RangeInclusive;
use std::time::Duration;

use axum::http::StatusCode;
use common::crashes::{
    AT_COMMIT, EVENT_POINTS, FACT_POINTS, STEP_POINTS, install_crash_points, plan_death,
};
use common::outbox::{disable, handed, insert_endpoint, master};
use common::payments::{REQUESTED, create_command, fee_rate};
use common::psp::{PspSide, client_of};
use common::{
    endpoint_id, merchant_id, request_id, runtime_pool_of_one, tenant_pool, wait_for_lock_wait,
};
use futures_util::future::join;
use mock_merchant::{Answer, MockMerchant, Verdict};
use payments_core::catch_up::{CatchUpError, CaughtUp, catch_up};
use payments_core::deliveries::{DeliveryClient, HANDOVERS, Handover};
use payments_core::ids::{EndpointId, MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::psp::PspClient;
use payments_core::relay::{Delivery, Effect, Relay};
use payments_core::relaying::{Relaying, Report};
use payments_core::requests::{Ask, RecordRequest};
use payments_core::retries::RetryBudget;
use payments_core::signing::MasterKey;
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// How many operations a run draws.
const OPERATIONS: u32 = 80;
/// The client's timeout here, short, so that a run is quick.
const TIMEOUT: Duration = Duration::from_millis(250);
/// An answer that comes after that timeout, from a merchant that took the event.
const LATE: Duration = Duration::from_millis(400);
/// The longest delay drawn, in milliseconds, so that the times stay near the clock.
const DRAWN: u64 = 3_000;
/// The most endpoints of one merchant.
const MOST_ENDPOINTS: usize = 2;
/// How many rounds the end of a run may take: a delivery still unfinished then is lost.
const ROUNDS: u32 = 60;
/// Mixed into a run's seed for the generator of the task's draws, whose number depends on
/// the threads' timing, so that they leave the operations' generator alone.
const DRAWS: u64 = 0x5DEE_CE66_D1CE_4E5B;

/// SplitMix64, as in the model and concurrency tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// One of the choices, with equal chances.
    fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
        let at = self.below(u64::try_from(choices.len()).expect("a short list"));
        choices[usize::try_from(at).expect("a short list")]
    }
}

/// How a catch-up runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Way {
    /// It hands its deliveries over to the task.
    HandedOver,
    /// Its handover is lost, as when the channel is full: only a search can find them.
    Dropped,
    /// It dies at a crash point, and a later catch-up applies the fact.
    Killed,
    /// A lock holds its transaction open while the task searches.
    HeldOpen,
}

/// What an endpoint answers to an event that it has not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Ok,
    Error,
    Gone,
    Late,
    Later,
}

/// One operation of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Op {
    /// A new payment's approval, caught up this way.
    CatchUp(Way),
    /// The payment whose catch-up died last is caught up.
    CatchUpKilled,
    /// The task searches and steps.
    Tick,
    /// The task takes what the catch-ups handed over.
    HandOver,
    /// The task reads the next attempt that ends.
    EndOne,
    /// The task reads every attempt until none is in flight.
    Settle,
    /// The task stops, and a new one starts.
    Restart,
    /// The database refuses the task's records of successes.
    RefuseRecords,
    /// It takes them again.
    AllowRecords,
    /// The operator registers an endpoint of a merchant.
    Register,
    /// The operator disables an endpoint.
    Disable,
    /// An endpoint answers new events this way.
    Answer(Kind),
    /// Time passes.
    Wait,
}

/// Each operation, and how often it comes.
const WEIGHTS: [(Op, u64); 20] = [
    (Op::CatchUp(Way::HandedOver), 10),
    (Op::CatchUp(Way::Dropped), 4),
    (Op::CatchUp(Way::Killed), 4),
    (Op::CatchUp(Way::HeldOpen), 3),
    (Op::CatchUpKilled, 3),
    (Op::Tick, 10),
    (Op::HandOver, 8),
    (Op::EndOne, 10),
    (Op::Settle, 6),
    (Op::Restart, 4),
    (Op::RefuseRecords, 3),
    (Op::AllowRecords, 3),
    (Op::Register, 6),
    (Op::Disable, 2),
    (Op::Answer(Kind::Ok), 4),
    (Op::Answer(Kind::Error), 3),
    (Op::Answer(Kind::Gone), 3),
    (Op::Answer(Kind::Late), 2),
    (Op::Answer(Kind::Later), 2),
    (Op::Wait, 2),
];

/// What a batch's runs met, for the check that each case came up.
#[derive(Default)]
struct Seen {
    operations: BTreeSet<Op>,
    outcomes: BTreeSet<&'static str>,
}

/// The outcomes that a batch must meet.
const OUTCOMES: [&str; 13] = [
    "an answer of 500",
    "an answer of 503 with Retry-After",
    "an answer after the timeout",
    "an event taken late, then delivered as a copy",
    "a copy answered 200",
    "an endpoint disabled as gone",
    "an endpoint disabled by the operator",
    "a delivery ended by its endpoint's disablement",
    "a restart with attempts in flight",
    "a success lost to a restart",
    "an endpoint registered after its merchant's events",
    "a search during a catch-up's transaction",
    "a search during a catch-up's transaction, its handover lost",
];

/// An endpoint of the run, and the fake behind it.
struct Endpoint {
    id: EndpointId,
    merchant: MerchantId,
    fake: MockMerchant,
}

/// An event that a catch-up wrote, and the endpoints that must get it.
struct Written {
    merchant: MerchantId,
    expected: BTreeSet<Uuid>,
}

/// What every run of a batch shares: the test's own pool, the PSP's side, the merchants'
/// pool, a pool of one session of the system role on which a catch-up may die, and the
/// master key.
struct World {
    pool: PgPool,
    psp: PspSide,
    client: PspClient,
    tenant: PgPool,
    one: PgPool,
    master: MasterKey,
}

/// One run: the model, the task, and what the run has done.
struct Run<'w> {
    world: &'w mut World,
    seen: &'w mut Seen,
    seed: u64,
    /// The operations and their choices.
    rng: Rng,
    /// The task's draws, for its delays.
    draws: Rng,
    start: OffsetDateTime,
    /// Milliseconds since `start`.
    elapsed: u64,
    next: u32,
    merchants: Vec<MerchantId>,
    endpoints: Vec<Endpoint>,
    events: BTreeMap<Uuid, Written>,
    killed: Vec<(MerchantId, PaymentId)>,
    /// The endpoints that the operator disabled.
    chosen: BTreeSet<Uuid>,
    relaying: Relaying,
    channel: mpsc::Receiver<Delivery>,
    handover: Handover,
    refusing: bool,
    /// Whether a success may be held: a record of one failed, and none has passed since.
    held: bool,
    in_flight: usize,
    done: Vec<String>,
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a small positive amount")
}

/// A task that knows nothing, as a server starts it at `now`.
fn task(system: &PgPool, master: &MasterKey, now: OffsetDateTime) -> Relaying {
    Relaying::new(
        system.clone(),
        DeliveryClient::new(master.clone(), TIMEOUT),
        Relay::with_budget(RetryBudget::standard(now)),
    )
}

/// The ID of the one event of `payment`, its approval's.
async fn event_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<Uuid> {
    sqlx::query_scalar!(
        "SELECT id FROM payment_events WHERE payment_id = $1",
        payment.get()
    )
    .fetch_one(pool)
    .await
}

/// Whether the task could read this answer as a success: a `2xx` in time, or a copy, which
/// the fake answers `200` at once.
fn read_as_success(verdict: Verdict) -> bool {
    match verdict {
        Verdict::Copy => true,
        Verdict::Taken(Answer::Status(status) | Answer::RetryAfter { status, .. }) => {
            status.is_success()
        }
        _ => false,
    }
}

impl<'w> Run<'w> {
    async fn start(
        world: &'w mut World,
        seen: &'w mut Seen,
        seed: u64,
    ) -> Result<Self, Box<dyn Error>> {
        let start = OffsetDateTime::now_utc();
        let (sender, channel) = mpsc::channel(HANDOVERS);
        let relaying = task(&world.psp.system, &world.master, start);
        let mut run = Self {
            world,
            seen,
            seed,
            rng: Rng(seed),
            draws: Rng(seed ^ DRAWS),
            start,
            elapsed: 0,
            next: u32::try_from(seed)?
                .checked_mul(1_000)
                .ok_or("a small seed")?,
            merchants: Vec::new(),
            endpoints: Vec::new(),
            events: BTreeMap::new(),
            killed: Vec::new(),
            chosen: BTreeSet::new(),
            relaying,
            channel,
            handover: Handover::new(sender),
            refusing: false,
            held: false,
            in_flight: 0,
            done: Vec::new(),
        };
        for _ in 0..3 {
            let merchant = merchant_id(run.number());
            register_merchant(&run.world.psp.system, merchant, fee_rate()).await?;
            run.merchants.push(merchant);
            run.register_for(merchant).await?;
        }
        Ok(run)
    }

    fn number(&mut self) -> u32 {
        self.next = self.next.checked_add(1).expect("a few numbers per run");
        self.next
    }

    fn now(&self) -> OffsetDateTime {
        self.start + Duration::from_millis(self.elapsed)
    }

    fn pass(&mut self, millis: u64) {
        self.elapsed = self.elapsed.checked_add(millis).expect("a short run");
    }

    /// The seed and the operations so far, for a failure's message.
    fn history(&self) -> String {
        format!("seed {}: {}", self.seed, self.done.join(", "))
    }

    /// Counts the attempts that `report` started and ended, and a success held.
    fn track(&mut self, report: &Report) {
        self.in_flight = self
            .in_flight
            .checked_add(report.started.len())
            .expect("few attempts");
        if report.ended.is_some() {
            self.in_flight = self
                .in_flight
                .checked_sub(1)
                .expect("an attempt that started");
        }
        match report.not_recorded {
            Some((Effect::Record(_), _)) => self.held = true,
            Some(_) => {}
            None => {
                if report
                    .recorded
                    .iter()
                    .any(|effect| matches!(effect, Effect::Record(_)))
                {
                    self.held = false;
                }
            }
        }
    }

    async fn draw_and_apply(&mut self) -> TestResult {
        let total: u64 = WEIGHTS.iter().map(|(_, weight)| weight).sum();
        let mut at = self.rng.below(total);
        let mut chosen = WEIGHTS[0].0;
        for (op, weight) in WEIGHTS {
            if at < weight {
                chosen = op;
                break;
            }
            at -= weight;
        }
        if let Some(what) = self.apply(chosen).await? {
            self.seen.operations.insert(chosen);
            self.done.push(what);
        }
        let between = self.rng.below(1_000);
        self.pass(between);
        Ok(())
    }

    /// Applies `op`, drawing the same number of times whatever happens: what it did, if
    /// anything.
    async fn apply(&mut self, op: Op) -> Result<Option<String>, Box<dyn Error>> {
        let what = format!("{op:?}");
        match op {
            Op::CatchUp(way) => {
                let detail = self.catch_up(way).await?;
                Ok(Some(format!("{what}{detail}")))
            }
            Op::CatchUpKilled => {
                let Some((merchant, payment)) = self.killed.pop() else {
                    return Ok(None);
                };
                let handover = self.handover.clone();
                self.applied(merchant, payment, &handover).await?;
                Ok(Some(what))
            }
            Op::Tick => {
                self.tick().await;
                Ok(Some(what))
            }
            Op::HandOver => Ok(self.hand_over().await.then_some(what)),
            Op::EndOne => Ok(self.end_one().await.then_some(what)),
            Op::Settle => Ok(self.settle().await.then_some(what)),
            Op::Restart => {
                self.restart();
                Ok(Some(what))
            }
            Op::RefuseRecords => Ok(self.refuse_records().await?.then_some(what)),
            Op::AllowRecords => Ok(self.allow_records().await?.then_some(what)),
            Op::Register => {
                let merchant = self.rng.pick(&self.merchants);
                Ok(self
                    .register_for(merchant)
                    .await?
                    .map(|id| format!("{what} {id}")))
            }
            Op::Disable => Ok(self.disable_one().await?.map(|id| format!("{what} {id}"))),
            Op::Answer(kind) => {
                let at = self.rng.below(u64::try_from(self.endpoints.len())?);
                let seconds = u32::try_from(self.rng.below(3))? + 1;
                let answer = match kind {
                    Kind::Ok => Answer::Status(StatusCode::OK),
                    Kind::Error => Answer::Status(StatusCode::INTERNAL_SERVER_ERROR),
                    Kind::Gone => Answer::Status(StatusCode::GONE),
                    Kind::Late => Answer::Delay(LATE),
                    Kind::Later => Answer::RetryAfter {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        seconds,
                    },
                };
                let endpoint = &self.endpoints[usize::try_from(at)?];
                endpoint.fake.answer(answer);
                Ok(Some(format!("{what} at {}", endpoint.id)))
            }
            Op::Wait => {
                let millis = self.rng.below(5_000);
                self.pass(millis);
                Ok(Some(what))
            }
        }
    }

    /// A new payment of `merchant` whose authorization the PSP has approved, not caught up.
    async fn approved(&mut self, merchant: MerchantId) -> Result<PaymentId, Box<dyn Error>> {
        let n = self.number();
        let command = create_command(n, merchant);
        create_payment(Tenant::new(&self.world.tenant, merchant), &command).await?;
        let request = RecordRequest {
            request: request_id(n),
            payment: command.id,
            ask: Ask::Authorize(amount(REQUESTED)),
        };
        self.world
            .psp
            .sent(Tenant::new(&self.world.tenant, merchant), &request)
            .await;
        self.world.psp.fake.approve(request.request.get());
        Ok(command.id)
    }

    /// The run's endpoints without a disablement, as a catch-up reads them.
    async fn enabled(&self) -> sqlx::Result<Vec<&Endpoint>> {
        let disabled: HashSet<Uuid> =
            sqlx::query_scalar!("SELECT endpoint_id FROM webhook_endpoint_disablements")
                .fetch_all(&self.world.pool)
                .await?
                .into_iter()
                .collect();
        Ok(self
            .endpoints
            .iter()
            .filter(|endpoint| !disabled.contains(&endpoint.id.get()))
            .collect())
    }

    /// The IDs of `merchant`'s endpoints without a disablement.
    async fn enabled_of(&self, merchant: MerchantId) -> sqlx::Result<BTreeSet<Uuid>> {
        Ok(self
            .enabled()
            .await?
            .into_iter()
            .filter(|endpoint| endpoint.merchant == merchant)
            .map(|endpoint| endpoint.id.get())
            .collect())
    }

    /// Catches `payment` up, handing over through `handover`, and notes its event with the
    /// endpoints that must get it.
    async fn applied(
        &mut self,
        merchant: MerchantId,
        payment: PaymentId,
        handover: &Handover,
    ) -> TestResult {
        let expected = self.enabled_of(merchant).await?;
        let caught = catch_up(
            &self.world.psp.system,
            &self.world.client,
            payment,
            handover,
        )
        .await?;
        assert_eq!(
            caught,
            CaughtUp::Current { applied: 1 },
            "{}",
            self.history()
        );
        let event = event_of(&self.world.pool, payment).await?;
        self.events.insert(event, Written { merchant, expected });
        Ok(())
    }

    /// Catches a new payment up `way`: what to add to the history.
    async fn catch_up(&mut self, way: Way) -> Result<String, Box<dyn Error>> {
        let merchant = self.rng.pick(&self.merchants);
        let payment = self.approved(merchant).await?;
        match way {
            Way::HandedOver => {
                let handover = self.handover.clone();
                self.applied(merchant, payment, &handover).await?;
                Ok(String::new())
            }
            Way::Dropped => {
                self.applied(merchant, payment, &Handover::none()).await?;
                Ok(String::new())
            }
            Way::Killed => {
                // A delivery's points are reached only when the merchant has an endpoint,
                // and the draw is made either way.
                let reached = !self.enabled_of(merchant).await?.is_empty();
                let mut points: Vec<&str> = STEP_POINTS
                    .into_iter()
                    .chain([
                        "die_before payment_authorizations",
                        "die_after payment_authorizations",
                    ])
                    .chain(FACT_POINTS)
                    .chain(EVENT_POINTS[..2].iter().copied())
                    .chain([AT_COMMIT])
                    .collect();
                if reached {
                    points.extend_from_slice(&EVENT_POINTS[2..]);
                }
                let point = self.rng.pick(&points);
                plan_death(&self.world.one, point).await?;
                let died =
                    catch_up(&self.world.one, &self.world.client, payment, &self.handover).await;
                assert!(
                    matches!(died, Err(CatchUpError::NotConfirmed { .. })),
                    "{point}: {died:?}, {}",
                    self.history()
                );
                self.killed.push((merchant, payment));
                Ok(format!(" at {point}"))
            }
            Way::HeldOpen => {
                // Half of them lose their handover too, so that only the next search's
                // list of transactions in progress can find their deliveries.
                let lost = self.rng.below(2) == 0;
                self.held_open(merchant, payment, lost).await?;
                Ok(if lost {
                    " without its handover".to_owned()
                } else {
                    String::new()
                })
            }
        }
    }

    /// Catches `payment` up while a lock holds its transaction open before its deliveries,
    /// and the task searches meanwhile, once a later transaction has completed: the search's
    /// snapshot lists the catch-up's transaction as in progress (ADR-0062, ADR-0064). When
    /// `lost`, the catch-up hands nothing over.
    async fn held_open(
        &mut self,
        merchant: MerchantId,
        payment: PaymentId,
        lost: bool,
    ) -> TestResult {
        let expected = self.enabled_of(merchant).await?;
        let handover = if lost {
            Handover::none()
        } else {
            self.handover.clone()
        };
        let mut holder = self.world.pool.begin().await?;
        // SHARE conflicts with the ROW EXCLUSIVE lock of an INSERT, and not with the
        // ACCESS SHARE of a SELECT, nor with the ROW SHARE of a foreign key's check.
        sqlx::query!("LOCK TABLE webhook_deliveries IN SHARE MODE")
            .execute(&mut *holder)
            .await?;
        let now = self.now();
        let world = &*self.world;
        let relaying = &mut self.relaying;
        let draws = &mut self.draws;
        let (caught, searched) = join(
            catch_up(&world.psp.system, &world.client, payment, &handover),
            async {
                wait_for_lock_wait(&world.pool, "relation").await?;
                // A later transaction completes first, which puts the catch-up's below the
                // snapshot's xmax, in its list.
                let mut later = world.pool.begin().await?;
                sqlx::query_scalar!(r#"SELECT pg_current_xact_id()::text::bigint AS "id!""#)
                    .fetch_one(&mut *later)
                    .await?;
                later.commit().await?;
                let report = relaying.tick(now, &mut || draws.below(DRAWN)).await;
                holder.rollback().await?;
                Ok::<Report, sqlx::Error>(report)
            },
        )
        .await;
        let report = searched?;
        self.track(&report);
        assert_eq!(
            caught?,
            CaughtUp::Current { applied: 1 },
            "{}",
            self.history()
        );
        if matches!(report.searched, Some(Ok(()))) {
            self.seen
                .outcomes
                .insert("a search during a catch-up's transaction");
            if lost {
                self.seen
                    .outcomes
                    .insert("a search during a catch-up's transaction, its handover lost");
            }
        }
        let event = event_of(&self.world.pool, payment).await?;
        self.events.insert(event, Written { merchant, expected });
        Ok(())
    }

    async fn tick(&mut self) {
        let now = self.now();
        let report = self
            .relaying
            .tick(now, &mut || self.draws.below(DRAWN))
            .await;
        self.track(&report);
    }

    /// Whether anything was handed over.
    async fn hand_over(&mut self) -> bool {
        let deliveries = handed(&mut self.channel);
        if deliveries.is_empty() {
            return false;
        }
        let now = self.now();
        let report = self.relaying.hand_over(deliveries, now).await;
        self.track(&report);
        true
    }

    /// Whether an attempt ended.
    async fn end_one(&mut self) -> bool {
        let Some(ended) = self.relaying.join().await else {
            return false;
        };
        let now = self.now();
        let draw = self.draws.below(DRAWN);
        let report = self.relaying.ended(ended, now, draw).await;
        self.track(&report);
        true
    }

    /// Whether any attempt ended.
    async fn settle(&mut self) -> bool {
        let mut any = false;
        while self.end_one().await {
            any = true;
        }
        any
    }

    fn restart(&mut self) {
        if self.in_flight > 0 {
            self.seen
                .outcomes
                .insert("a restart with attempts in flight");
        }
        if self.refusing && self.held {
            self.seen.outcomes.insert("a success lost to a restart");
        }
        let now = self.now();
        let (sender, channel) = mpsc::channel(HANDOVERS);
        // The task that ends drops its attempts in flight with it.
        self.relaying = task(&self.world.psp.system, &self.world.master, now);
        self.channel = channel;
        self.handover = Handover::new(sender);
        self.in_flight = 0;
        self.held = false;
    }

    async fn refuse_records(&mut self) -> Result<bool, Box<dyn Error>> {
        if self.refusing {
            return Ok(false);
        }
        sqlx::query!("REVOKE INSERT (delivery_id) ON webhook_delivery_successes FROM payments_app")
            .execute(&self.world.pool)
            .await?;
        self.refusing = true;
        Ok(true)
    }

    async fn allow_records(&mut self) -> Result<bool, Box<dyn Error>> {
        if !self.refusing {
            return Ok(false);
        }
        sqlx::query!("GRANT INSERT (delivery_id) ON webhook_delivery_successes TO payments_app")
            .execute(&self.world.pool)
            .await?;
        self.refusing = false;
        Ok(true)
    }

    /// Registers an endpoint of `merchant` at a fake of its own, unless it has the most:
    /// the endpoint, if it did.
    async fn register_for(
        &mut self,
        merchant: MerchantId,
    ) -> Result<Option<EndpointId>, Box<dyn Error>> {
        let held = self
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.merchant == merchant)
            .count();
        if held >= MOST_ENDPOINTS {
            return Ok(None);
        }
        let id = endpoint_id(self.number());
        let fake = MockMerchant::start().await?;
        fake.trust(&self.world.master.secret_of(id).written())?;
        insert_endpoint(&self.world.pool, id, merchant.get(), fake.url()).await?;
        if self
            .events
            .values()
            .any(|written| written.merchant == merchant)
        {
            self.seen
                .outcomes
                .insert("an endpoint registered after its merchant's events");
        }
        self.endpoints.push(Endpoint { id, merchant, fake });
        Ok(Some(id))
    }

    /// The operator disables one of the endpoints still enabled: the endpoint, if there was
    /// one. It draws whether or not there is.
    async fn disable_one(&mut self) -> Result<Option<EndpointId>, Box<dyn Error>> {
        let draw = self.rng.next();
        let enabled: Vec<EndpointId> = self
            .enabled()
            .await?
            .into_iter()
            .map(|endpoint| endpoint.id)
            .collect();
        if enabled.is_empty() {
            return Ok(None);
        }
        let id = enabled[usize::try_from(draw % u64::try_from(enabled.len())?)?];
        disable(&self.world.pool, id, "operator").await?;
        self.chosen.insert(id.get());
        Ok(Some(id))
    }

    /// How many deliveries of the run have neither a success nor a disabled endpoint.
    async fn unfinished(&self) -> sqlx::Result<i64> {
        let merchants: Vec<Uuid> = self
            .merchants
            .iter()
            .map(|merchant| merchant.get())
            .collect();
        sqlx::query_scalar!(
            r#"SELECT count(*) AS "unfinished!" FROM webhook_deliveries d
                WHERE d.merchant_id = ANY($1)
                  AND NOT EXISTS (SELECT 1 FROM webhook_delivery_successes s
                                   WHERE s.delivery_id = d.id)
                  AND NOT EXISTS (SELECT 1 FROM webhook_endpoint_disablements x
                                   WHERE x.endpoint_id = d.endpoint_id)"#,
            &merchants
        )
        .fetch_one(&self.world.pool)
        .await
    }

    /// Every endpoint answers `200`, every death is caught up, and the task steps until no
    /// delivery is unfinished, within [`ROUNDS`].
    async fn finish(&mut self) -> TestResult {
        self.allow_records().await?;
        for endpoint in &self.endpoints {
            endpoint.fake.answer(Answer::Status(StatusCode::OK));
        }
        while let Some((merchant, payment)) = self.killed.pop() {
            let handover = self.handover.clone();
            self.applied(merchant, payment, &handover).await?;
        }
        for _ in 0..ROUNDS {
            self.hand_over().await;
            self.tick().await;
            self.settle().await;
            if self.unfinished().await? == 0 {
                return Ok(());
            }
            self.pass(DRAWN);
        }
        Err(format!(
            "{} deliveries unfinished after {ROUNDS} rounds; {}",
            self.unfinished().await?,
            self.history()
        )
        .into())
    }

    /// The checks against the model.
    async fn check(&mut self) -> TestResult {
        let merchants: Vec<Uuid> = self
            .merchants
            .iter()
            .map(|merchant| merchant.get())
            .collect();
        let rows = sqlx::query!(
            r#"SELECT d.event_id, d.endpoint_id,
                      EXISTS (SELECT 1 FROM webhook_delivery_successes s
                               WHERE s.delivery_id = d.id) AS "delivered!",
                      EXISTS (SELECT 1 FROM webhook_endpoint_disablements x
                               WHERE x.endpoint_id = d.endpoint_id) AS "disabled!"
                 FROM webhook_deliveries d
                WHERE d.merchant_id = ANY($1)"#,
            &merchants
        )
        .fetch_all(&self.world.pool)
        .await?;
        let history = self.history();

        // The deliveries are exactly the model's: each event to its merchant's endpoints
        // enabled when its catch-up ran (I6, I7).
        let written: BTreeSet<(Uuid, Uuid)> = rows
            .iter()
            .map(|row| (row.event_id, row.endpoint_id))
            .collect();
        let expected: BTreeSet<(Uuid, Uuid)> = self
            .events
            .iter()
            .flat_map(|(event, written)| {
                written.expected.iter().map(|endpoint| (*event, *endpoint))
            })
            .collect();
        assert_eq!(written, expected, "{history}");

        // Each one delivered, or its endpoint disabled (I10).
        for row in &rows {
            assert!(row.delivered || row.disabled, "{row:?}; {history}");
            if !row.delivered {
                self.seen
                    .outcomes
                    .insert("a delivery ended by its endpoint's disablement");
            }
        }

        let ids: Vec<Uuid> = self
            .endpoints
            .iter()
            .map(|endpoint| endpoint.id.get())
            .collect();
        let reasons: BTreeMap<Uuid, String> = sqlx::query!(
            "SELECT endpoint_id, reason FROM webhook_endpoint_disablements
              WHERE endpoint_id = ANY($1)",
            &ids
        )
        .fetch_all(&self.world.pool)
        .await?
        .into_iter()
        .map(|row| (row.endpoint_id, row.reason))
        .collect();

        for endpoint in &self.endpoints {
            // Each arrival, by the event that it brought.
            let mut arrived: BTreeMap<Uuid, Vec<Verdict>> = BTreeMap::new();
            for arrival in endpoint.fake.arrivals() {
                // Every delivery passes the fake's checks: its signature and its window.
                assert!(
                    matches!(
                        arrival.verdict,
                        Verdict::Taken(_) | Verdict::Declined(_) | Verdict::Copy
                    ),
                    "{arrival:?}; {history}"
                );
                let id = Uuid::parse_str(arrival.id.as_deref().ok_or("an arrival without an ID")?)?;
                // Only events that a catch-up wrote, and only of the endpoint's merchant (I7).
                let event = self
                    .events
                    .get(&id)
                    .ok_or_else(|| format!("an event that no catch-up wrote: {id}; {history}"))?;
                assert_eq!(event.merchant, endpoint.merchant, "{arrival:?}; {history}");
                match arrival.verdict {
                    Verdict::Copy => {
                        self.seen.outcomes.insert("a copy answered 200");
                    }
                    Verdict::Taken(Answer::Delay(_)) => {
                        self.seen.outcomes.insert("an answer after the timeout");
                    }
                    Verdict::Declined(Answer::Status(status))
                        if status == StatusCode::INTERNAL_SERVER_ERROR =>
                    {
                        self.seen.outcomes.insert("an answer of 500");
                    }
                    Verdict::Declined(Answer::RetryAfter { .. }) => {
                        self.seen
                            .outcomes
                            .insert("an answer of 503 with Retry-After");
                    }
                    _ => {}
                }
                arrived.entry(id).or_default().push(arrival.verdict);
            }

            // Each disablement has its cause: the operator chose it, or the endpoint
            // answered 410. Three days without a success cannot pass in a run.
            match reasons.get(&endpoint.id.get()).map(String::as_str) {
                None => {}
                Some("operator") => {
                    assert!(
                        self.chosen.contains(&endpoint.id.get()),
                        "{} disabled by no operator; {history}",
                        endpoint.id
                    );
                    self.seen
                        .outcomes
                        .insert("an endpoint disabled by the operator");
                }
                Some("gone") => {
                    let answered_410 = arrived.values().flatten().any(|verdict| {
                        matches!(verdict, Verdict::Declined(Answer::Status(status))
                            if *status == StatusCode::GONE)
                    });
                    assert!(
                        answered_410,
                        "{} gone without a 410; {history}",
                        endpoint.id
                    );
                    self.seen.outcomes.insert("an endpoint disabled as gone");
                }
                Some(other) => {
                    return Err(format!("{} disabled as {other}; {history}", endpoint.id).into());
                }
            }

            // Each success follows an answer of 2xx that the task could read, not one that
            // came after its timeout (ADR-0053).
            for row in rows
                .iter()
                .filter(|row| row.endpoint_id == endpoint.id.get() && row.delivered)
            {
                let verdicts = arrived.get(&row.event_id).map_or(&[][..], Vec::as_slice);
                assert!(
                    verdicts.iter().copied().any(read_as_success),
                    "{row:?}: {verdicts:?}; {history}"
                );
                if verdicts
                    .iter()
                    .any(|verdict| matches!(verdict, Verdict::Taken(Answer::Delay(_))))
                {
                    self.seen
                        .outcomes
                        .insert("an event taken late, then delivered as a copy");
                }
            }
        }
        Ok(())
    }

    async fn close(self) {
        for endpoint in self.endpoints {
            endpoint.fake.stop().await;
        }
    }
}

/// Runs `seeds` one after the other in the test's database: each ends with nothing
/// unfinished, so the next one's task finds only its own deliveries. Then checks that the
/// runs met every operation and outcome.
async fn batch(pool: PgPool, seeds: RangeInclusive<u64>) -> TestResult {
    install_crash_points(&pool).await?;
    let psp = PspSide::start(&pool).await?;
    let mut world = World {
        client: client_of(&psp.fake),
        tenant: tenant_pool(&pool).await?,
        one: runtime_pool_of_one(&pool).await?,
        master: master(),
        pool,
        psp,
    };
    let mut seen = Seen::default();
    for seed in seeds {
        let mut run = Run::start(&mut world, &mut seen, seed).await?;
        for _ in 0..OPERATIONS {
            run.draw_and_apply().await?;
        }
        run.finish().await?;
        run.check().await?;
        run.close().await;
    }
    for (op, _) in WEIGHTS {
        assert!(
            seen.operations.contains(&op),
            "no run did {op:?}: {:?}",
            seen.operations
        );
    }
    for outcome in OUTCOMES {
        assert!(
            seen.outcomes.contains(outcome),
            "no run met {outcome}: {:?}",
            seen.outcomes
        );
    }
    world.tenant.close().await;
    world.one.close().await;
    world.psp.close().await;
    Ok(())
}

#[sqlx::test]
async fn random_runs_from_seeds_1_to_10_match_the_model(pool: PgPool) -> TestResult {
    batch(pool, 1..=10).await
}

#[sqlx::test]
async fn random_runs_from_seeds_11_to_20_match_the_model(pool: PgPool) -> TestResult {
    batch(pool, 11..=20).await
}

#[sqlx::test]
async fn random_runs_from_seeds_21_to_30_match_the_model(pool: PgPool) -> TestResult {
    batch(pool, 21..=30).await
}

#[sqlx::test]
async fn random_runs_from_seeds_31_to_40_match_the_model(pool: PgPool) -> TestResult {
    batch(pool, 31..=40).await
}
