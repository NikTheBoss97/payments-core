// Author: Nicola Antonello

//! The relay's task (ADR-0052, ADR-0053, ADR-0062): one task owns the relay. It takes the
//! deliveries that the catch-ups hand over, searches every second, starts the attempts
//! that each step asks for, and records what the relay decided. Each attempt runs in a
//! task of its own, in a set that the loop waits on, so that no endpoint holds up another;
//! the relay's rules bound how many run at once.
//!
//! Every event steps at once: a tick after its search, a handover, and the end of an
//! attempt, so that a freed slot is used at once. A step starts its attempts, then records
//! the effects that the relay hands back, in order, and stops at the first that fails:
//! the database is then most likely out of reach, and the rest wait for the next step. An
//! attempt that panics, a bug of ours, is read as not sent. The time and the draws are
//! inputs, so that tests drive each event; [`Relaying::run`] reads the clock and the
//! operating system's random bytes.
//!
//! The log holds each disablement with its reason, a failed search each time, a record
//! that fails and an attempt not sent once while they keep failing, and a panic as an
//! error; no line for an attempt that went as it should.

use std::collections::HashMap;
use std::convert::Infallible;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::{self, MissedTickBehavior};
use tracing::Level;

use crate::api::problem::chain;
use crate::deliveries::{self, DeliveryClient, NotSent, Outcome, RecordError, SearchError};
use crate::ids::{DeliveryId, EndpointId};
use crate::relay::{Delivery, Effect, Reason, Relay};
use crate::sender::os_random;

/// How often the task searches (ADR-0062).
pub const TICK: Duration = Duration::from_secs(1);

/// The relay's task: the relay, the client that posts the attempts, the system role's
/// pool, and the attempts in flight.
#[derive(Debug)]
pub struct Relaying {
    relay: Relay,
    client: Arc<DeliveryClient>,
    pool: PgPool,
    attempts: JoinSet<Result<Outcome, NotSent>>,
    /// The delivery of each attempt in the set, by its task's ID. Tokio gives an ID to a
    /// new task only once the task that held it has left the set, so the entry goes when
    /// its attempt is joined.
    deliveries: HashMap<task::Id, DeliveryId>,
}

/// An attempt that ended, as [`Relaying::join`] waits for it, for [`Relaying::ended`].
#[derive(Debug)]
pub struct Ended(Result<(task::Id, Result<Outcome, NotSent>), JoinError>);

/// How an attempt ended.
#[derive(Debug)]
pub enum Ending {
    /// The database gave what the attempt needed: it was sent, or it found its endpoint
    /// disabled.
    Made,
    /// The database did not give what the attempt needed: it waits like a retry, and its
    /// endpoint is not blamed (ADR-0053).
    NotSent(NotSent),
    /// Its task panicked, a bug of ours, as nothing aborts one: it is read as not sent.
    Panicked(JoinError),
}

/// What one event did, as the log reads it.
#[derive(Debug, Default)]
pub struct Report {
    /// How the search went, when the event searched.
    pub searched: Option<Result<(), SearchError>>,
    /// The attempt that ended, when the event was the end of one, and how.
    pub ended: Option<(DeliveryId, Ending)>,
    /// The attempts started.
    pub started: Vec<Delivery>,
    /// The effects recorded, in order.
    pub recorded: Vec<Effect>,
    /// The first effect whose record failed, and why. The step recorded nothing after it.
    pub not_recorded: Option<(Effect, RecordError)>,
}

impl Relaying {
    /// The task with this relay, posting through `client`, on `pool`, the system role's.
    #[must_use]
    pub fn new(pool: PgPool, client: DeliveryClient, relay: Relay) -> Self {
        Self {
            relay,
            client: Arc::new(client),
            pool,
            attempts: JoinSet::new(),
            deliveries: HashMap::new(),
        }
    }

    /// A tick at `now`: a search (ADR-0062), whose first one draws with `random` the
    /// delays of what it finds, then a step. A search that fails teaches the relay
    /// nothing, and the next one reads from the same snapshot.
    pub async fn tick(&mut self, now: OffsetDateTime, random: &mut impl FnMut() -> u64) -> Report {
        let search = self.relay.begin_search();
        let searched = match deliveries::search(&self.pool, &search).await {
            Ok((snapshot, found)) => {
                self.relay.searched(search, snapshot, found, now, random);
                Ok(())
            }
            Err(failed) => Err(failed),
        };
        Report {
            searched: Some(searched),
            ..self.step(now).await
        }
    }

    /// Takes the deliveries that a catch-up handed over, at `now`, then steps.
    pub async fn hand_over(
        &mut self,
        deliveries: impl IntoIterator<Item = Delivery>,
        now: OffsetDateTime,
    ) -> Report {
        self.relay.hand_over(deliveries, now);
        self.step(now).await
    }

    /// Waits for the next attempt to end, or returns `None` at once when none is in
    /// flight. It is cancel safe, as the set's own wait is: a `select!` that picks another
    /// branch loses no attempt.
    pub async fn join(&mut self) -> Option<Ended> {
        self.attempts.join_next_with_id().await.map(Ended)
    }

    /// Reads an attempt that ended, at `now`, with `draw` for its next delay, then steps.
    pub async fn ended(&mut self, ended: Ended, now: OffsetDateTime, draw: u64) -> Report {
        let (delivery, ending) = match ended.0 {
            Ok((id, outcome)) => {
                let delivery = self.delivery_of(id);
                let ending = match outcome {
                    Ok(Outcome::Answered(answer)) => {
                        // Its effect, if any, waits in the relay, and the step below records
                        // it.
                        self.relay.answered(delivery, answer, now, draw);
                        Ending::Made
                    }
                    Ok(Outcome::Disabled) => {
                        self.relay.found_disabled(delivery);
                        Ending::Made
                    }
                    Err(not_sent) => {
                        self.relay.not_sent(delivery, now, draw);
                        Ending::NotSent(not_sent)
                    }
                };
                (delivery, ending)
            }
            Err(failed) => {
                let delivery = self.delivery_of(failed.id());
                self.relay.not_sent(delivery, now, draw);
                (delivery, Ending::Panicked(failed))
            }
        };
        Report {
            ended: Some((delivery, ending)),
            ..self.step(now).await
        }
    }

    /// One step of the relay at `now`: starts its attempts, then records the effects that
    /// it hands back, in order, until one fails. The attempts start before anything is
    /// awaited, so that the relay never counts in flight an attempt that no task runs.
    pub async fn step(&mut self, now: OffsetDateTime) -> Report {
        let step = self.relay.step(now);
        for &delivery in &step.attempts {
            self.start(delivery, now);
        }
        let mut report = Report {
            started: step.attempts,
            ..Report::default()
        };
        for effect in step.effects {
            match deliveries::record(&self.pool, effect).await {
                Ok(()) => {
                    self.relay.recorded(effect);
                    report.recorded.push(effect);
                }
                Err(failed) => {
                    report.not_recorded = Some((effect, failed));
                    break;
                }
            }
        }
        report
    }

    /// Runs the task for as long as the process lives: a tick every `tick`, the first at
    /// once, a step at each handover of `handovers` and at the end of each attempt, and
    /// logs what changed. It never returns, so a task that ends has panicked, which the
    /// server reads as a crash (ADR-0044).
    pub async fn run(
        mut self,
        tick: Duration,
        mut handovers: mpsc::Receiver<Delivery>,
    ) -> Infallible {
        // A late tick delays the next one by a whole period, rather than running the
        // missed ones in a burst (ADR-0044).
        let mut ticks = time::interval(tick);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut watch = Watch::default();
        loop {
            // The select! only picks the next event. The work runs after it, outside the
            // select!, so that no step is ever dropped half done (ADR-0043).
            let event = tokio::select! {
                _ = ticks.tick() => Event::Tick,
                Some(delivery) = handovers.recv() => Event::Handover(delivery),
                Some(ended) = self.join() => Event::Ended(ended),
            };
            let now = OffsetDateTime::now_utc();
            let report = match event {
                Event::Tick => self.tick(now, &mut os_random).await,
                Event::Handover(first) => {
                    // Every delivery already queued goes into this one step (ADR-0052).
                    let mut handed = vec![first];
                    while let Ok(delivery) = handovers.try_recv() {
                        handed.push(delivery);
                    }
                    self.hand_over(handed, now).await
                }
                Event::Ended(ended) => self.ended(ended, now, os_random()).await,
            };
            for line in watch.lines(&report) {
                line.log();
            }
        }
    }

    /// Starts an attempt of `delivery` at `now`, in a task of its own.
    fn start(&mut self, delivery: Delivery, now: OffsetDateTime) {
        let client = Arc::clone(&self.client);
        let pool = self.pool.clone();
        let attempt = self
            .attempts
            .spawn(async move { client.attempt(&pool, delivery, now).await });
        self.deliveries.insert(attempt.id(), delivery.id);
    }

    /// The delivery of the attempt that task `id` ran, which leaves the map with it.
    fn delivery_of(&mut self, id: task::Id) -> DeliveryId {
        self.deliveries
            .remove(&id)
            .expect("each attempt in the set has its delivery in the map")
    }
}

/// The task's next piece of work.
#[derive(Debug)]
enum Event {
    Tick,
    /// A catch-up committed this delivery.
    Handover(Delivery),
    Ended(Ended),
}

/// One line of the task's log, and its level: an error is a bug of ours, and a warning is
/// something that failed or that ends an endpoint's deliveries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    level: Level,
    text: String,
}

impl Line {
    fn warn(text: String) -> Self {
        Self {
            level: Level::WARN,
            text,
        }
    }

    fn error(text: String) -> Self {
        Self {
            level: Level::ERROR,
            text,
        }
    }

    fn log(&self) {
        if self.level == Level::ERROR {
            tracing::error!("{}", self.text);
        } else {
            tracing::warn!("{}", self.text);
        }
    }
}

/// What the task has logged as failing, so that a failure is logged once while it lasts.
#[derive(Debug, Default)]
struct Watch {
    /// An attempt not sent, logged since the last attempt that was made.
    not_sent: bool,
    /// A record that failed, logged since the last that went through.
    not_recorded: bool,
}

impl Watch {
    /// The lines for one event: a failed search, each time; an attempt not sent, once
    /// until an attempt is made; an attempt that panicked, as an error; each disablement
    /// recorded, with its reason; and a record that fails, once until a record goes
    /// through.
    fn lines(&mut self, report: &Report) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(Err(failed)) = &report.searched {
            lines.push(Line::warn(format!(
                "the relay's search failed, and it searches again at the next tick: {}",
                chain(failed)
            )));
        }
        match &report.ended {
            Some((_, Ending::Made)) => self.not_sent = false,
            Some((delivery, Ending::NotSent(failed))) => {
                if !mem::replace(&mut self.not_sent, true) {
                    lines.push(Line::warn(format!(
                        "delivery {delivery} was not sent, and waits like a retry: {}",
                        chain(failed)
                    )));
                }
            }
            Some((delivery, Ending::Panicked(failed))) => lines.push(Line::error(format!(
                "the attempt at delivery {delivery} panicked, a bug of ours, so it waits like a retry: {failed}"
            ))),
            None => {}
        }
        for effect in &report.recorded {
            self.not_recorded = false;
            if let Effect::Disable(endpoint, reason) = *effect {
                lines.push(Line::warn(disabled(endpoint, reason)));
            }
        }
        if let Some((effect, failed)) = &report.not_recorded
            && !mem::replace(&mut self.not_recorded, true)
        {
            lines.push(Line::warn(format!(
                "the relay could not record {}, and tries again at each step: {}",
                what(*effect),
                chain(failed)
            )));
        }
        lines
    }
}

/// The line for an endpoint that the relay disabled.
fn disabled(endpoint: EndpointId, reason: Reason) -> String {
    match reason {
        Reason::Gone => {
            format!("endpoint {endpoint} answered 410, so it is disabled and its deliveries end")
        }
        Reason::Failing => format!(
            "endpoint {endpoint} has had a delivery fail for three days, so it is disabled and its deliveries end"
        ),
    }
}

/// What an effect records, in words.
fn what(effect: Effect) -> String {
    match effect {
        Effect::Record(delivery) => format!("the success of delivery {delivery}"),
        Effect::Disable(endpoint, _) => format!("the disablement of endpoint {endpoint}"),
    }
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;
    use crate::relay::Search;

    const D1: Uuid = Uuid::from_u128(0x0196_a5b4_c3d2_7e8f_9a0b_1c2d_3e4f_0001);
    const E1: Uuid = Uuid::from_u128(0x0196_a5b4_c3d2_7e8f_9a0b_1c2d_3e4f_1001);
    const E2: Uuid = Uuid::from_u128(0x0196_a5b4_c3d2_7e8f_9a0b_1c2d_3e4f_1002);

    fn delivery() -> DeliveryId {
        DeliveryId::try_from(D1).expect("a UUIDv7")
    }

    fn endpoint(id: Uuid) -> EndpointId {
        EndpointId::try_from(id).expect("a UUIDv7")
    }

    fn warn(text: &str) -> Line {
        Line::warn(text.to_owned())
    }

    fn error(text: &str) -> Line {
        Line::error(text.to_owned())
    }

    /// An event that was the end of an attempt at delivery `D1`.
    fn ended(ending: Ending) -> Report {
        Report {
            ended: Some((delivery(), ending)),
            ..Report::default()
        }
    }

    /// A pool that is closed, so that every query on it fails at once: the real errors of a
    /// search and of a record.
    async fn closed() -> PgPool {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:9/nothing")
            .expect("a URL that parses");
        pool.close().await;
        pool
    }

    /// An event whose record of `effect` failed, as it fails on `pool`.
    async fn not_recorded(pool: &PgPool, effect: Effect) -> Report {
        let failed = deliveries::record(pool, effect)
            .await
            .expect_err("a closed pool records nothing");
        Report {
            not_recorded: Some((effect, failed)),
            ..Report::default()
        }
    }

    /// A bug in an attempt.
    fn bug() {
        panic!("a bug");
    }

    #[test]
    fn an_event_that_did_nothing_logs_nothing() {
        assert_eq!(Watch::default().lines(&Report::default()), []);
        assert_eq!(Watch::default().lines(&ended(Ending::Made)), []);
    }

    #[test]
    fn each_disablement_recorded_is_a_warning_with_its_reason_and_a_success_is_not_logged() {
        let report = Report {
            recorded: vec![
                Effect::Record(delivery()),
                Effect::Disable(endpoint(E1), Reason::Gone),
                Effect::Disable(endpoint(E2), Reason::Failing),
            ],
            ..Report::default()
        };
        assert_eq!(
            Watch::default().lines(&report),
            [
                warn(&format!(
                    "endpoint {E1} answered 410, so it is disabled and its deliveries end"
                )),
                warn(&format!(
                    "endpoint {E2} has had a delivery fail for three days, so it is disabled and its deliveries end"
                )),
            ]
        );
    }

    #[tokio::test]
    async fn a_search_that_fails_is_a_warning_each_time() {
        let pool = closed().await;
        let mut watch = Watch::default();
        for _ in 0..2 {
            let report = Report {
                searched: Some(deliveries::search(&pool, &Search::Full).await.map(|_| ())),
                ..Report::default()
            };
            assert_eq!(
                watch.lines(&report),
                [warn(
                    "the relay's search failed, and it searches again at the next tick: the deliveries not finished could not be searched: attempted to acquire a connection on a closed pool"
                )]
            );
        }
    }

    #[test]
    fn an_attempt_not_sent_is_a_warning_once_until_an_attempt_is_made() {
        let not_sent = || Ending::NotSent(NotSent::Endpoint(sqlx::Error::PoolClosed));
        let line = || {
            warn(&format!(
                "delivery {D1} was not sent, and waits like a retry: the endpoint could not be read: attempted to acquire a connection on a closed pool"
            ))
        };
        let mut watch = Watch::default();
        assert_eq!(watch.lines(&ended(not_sent())), [line()]);
        assert_eq!(watch.lines(&ended(not_sent())), []);
        // Once an attempt is made, the next one not sent is logged again.
        assert_eq!(watch.lines(&ended(Ending::Made)), []);
        assert_eq!(watch.lines(&ended(not_sent())), [line()]);
    }

    #[tokio::test]
    async fn a_record_that_fails_is_a_warning_once_until_a_record_goes_through() {
        let pool = closed().await;
        let success = Effect::Record(delivery());
        let disable = Effect::Disable(endpoint(E1), Reason::Gone);
        let mut watch = Watch::default();
        assert_eq!(
            watch.lines(&not_recorded(&pool, success).await),
            [warn(&format!(
                "the relay could not record the success of delivery {D1}, and tries again at each step: the effect could not be written: attempted to acquire a connection on a closed pool"
            ))]
        );
        assert_eq!(watch.lines(&not_recorded(&pool, disable).await), []);
        // Once a record goes through, the next failure is logged again.
        let went_through = Report {
            recorded: vec![success],
            ..Report::default()
        };
        assert_eq!(watch.lines(&went_through), []);
        assert_eq!(
            watch.lines(&not_recorded(&pool, disable).await),
            [warn(&format!(
                "the relay could not record the disablement of endpoint {E1}, and tries again at each step: the effect could not be written: attempted to acquire a connection on a closed pool"
            ))]
        );
    }

    #[tokio::test]
    async fn an_attempt_that_panicked_is_an_error_each_time() {
        let mut watch = Watch::default();
        for _ in 0..2 {
            let failed = tokio::spawn(async { bug() })
                .await
                .expect_err("the attempt panicked");
            let text = format!(
                "the attempt at delivery {D1} panicked, a bug of ours, so it waits like a retry: {failed}"
            );
            assert_eq!(
                watch.lines(&ended(Ending::Panicked(failed))),
                [error(&text)]
            );
        }
    }
}
