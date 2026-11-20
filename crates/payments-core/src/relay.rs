// Author: Nicola Antonello

//! The relay's rules, apart from its input and output (ADR-0052, ADR-0053, ADR-0062): what
//! an endpoint's answer means, which deliveries to attempt at each step, when an endpoint
//! is disabled, and what each search reads. The server's task reads the database, posts the
//! attempts and carries out the effects; this module decides. The time and the random draws
//! are inputs, so every rule is tested without a clock.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::time::Duration;

use sqlx::types::time::OffsetDateTime;
use uuid::Uuid;

use crate::ids::{DeliveryId, EndpointId, EventId};
use crate::retries::{self, RetryBudget};

/// At most this many attempts at once to one endpoint (ADR-0053).
pub const AT_ONCE: usize = 4;

/// How long a delivery may go without a success, from its first failed attempt, before
/// the relay disables its endpoint: three days (ADR-0053).
pub const FAILING_AFTER: Duration = Duration::from_secs(259_200);

/// What one attempt to deliver got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The endpoint answered with this status, and with a `Retry-After` of this many
    /// seconds, if it gave one: seconds, or a date counted from the attempt's start
    /// (ADR-0053).
    Status {
        status: u16,
        retry_after: Option<u32>,
    },
    /// No answer came: a refused connection, a reset, or the timeout, after which the
    /// outcome is unknown (ADR-0053).
    NoAnswer,
}

/// What an answer means, by ADR-0053's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A `2xx`: the success is recorded, and the delivery is finished.
    Delivered,
    /// A `410`: the endpoint is disabled, and its deliveries are finished.
    Gone,
    /// Anything else: tried again, and never before `at_least` seconds when the endpoint
    /// asked for them.
    Again { at_least: Option<u32> },
}

/// Reads an answer by ADR-0053's table. A redirect is a failure, since the client follows
/// none, and a 429, 502 or 504 without `Retry-After` backs off like any other failure.
#[must_use]
pub fn verdict(answer: Answer) -> Verdict {
    match answer {
        Answer::Status {
            status: 200..=299, ..
        } => Verdict::Delivered,
        Answer::Status { status: 410, .. } => Verdict::Gone,
        Answer::Status { retry_after, .. } => Verdict::Again {
            at_least: retry_after,
        },
        Answer::NoAnswer => Verdict::Again { at_least: None },
    }
}

/// A delivery as the relay learns of it, from the handover or a search, and as it hands it
/// back to be attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub id: DeliveryId,
    pub endpoint: EndpointId,
    pub event: EventId,
    /// The transaction that wrote it, which the relay compares with its snapshots
    /// (ADR-0064).
    pub xact: XactId,
}

/// What the server carries out after an answer (ADR-0053).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Records the delivery's success, once: recording it again writes nothing.
    Record(DeliveryId),
    /// Disables the endpoint, and its unfinished deliveries end with it.
    Disable(EndpointId, Reason),
}

/// Why the relay disables an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// It answered `410`.
    Gone,
    /// One of its deliveries has gone [`FAILING_AFTER`] without a success.
    Failing,
}

impl Reason {
    /// The label that the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gone => "gone",
            Self::Failing => "failing",
        }
    }
}

/// What a step asks of the server: the effects to carry out, each one not recorded yet,
/// then the attempts to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub effects: Vec<Effect>,
    pub attempts: Vec<Delivery>,
}

/// A transaction's ID as `pg_current_xact_id()` gives it, an `xid8`, which never wraps
/// around (ADR-0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XactId(u64);

impl XactId {
    /// The ID that an `xid8` holds.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The ID, to bind in SQL.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What the server reads of a snapshot, in a statement of its own before a search's rows
/// (ADR-0062): its `xmax`, at or above which no transaction had completed, and the
/// transactions below it then in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub xmax: XactId,
    pub in_progress: Vec<XactId>,
}

impl Snapshot {
    /// Whether transaction `xact` had completed when the snapshot was taken: below its
    /// `xmax`, and not in its list (ADR-0062, ADR-0064).
    fn completed(&self, xact: XactId) -> bool {
        xact < self.xmax && !self.in_progress.contains(&xact)
    }
}

/// Which transactions' deliveries a search reads: those not finished, with no success and
/// an endpoint not disabled (ADR-0062).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Search {
    /// Every transaction's: the first search since the start.
    Full,
    /// The transactions that had not completed at this snapshot, the previous search's:
    /// those in its list, and those at or above its `xmax`.
    Since(Snapshot),
}

/// What the relay holds in memory: the deliveries it will attempt, the attempts in flight,
/// the endpoints that it knows are disabled, the effects not recorded yet, its budget of
/// retries, whose turn is next and the previous search's snapshot. A restart starts it
/// empty (ADR-0052, ADR-0062).
#[derive(Debug)]
pub struct Relay {
    budget: RetryBudget,
    /// The effects returned and not recorded yet, oldest first, which each step hands back
    /// until the task records them.
    pending: Vec<Effect>,
    /// The deliveries to attempt, by ID.
    known: BTreeMap<Uuid, Known>,
    /// The attempts started and not answered yet, by delivery, with their endpoints.
    in_flight: HashMap<Uuid, EndpointId>,
    /// The endpoints that this process disabled, or that an attempt found disabled: a
    /// search that raced the record of a disablement can still return their deliveries.
    disabled: HashSet<Uuid>,
    /// The endpoint whose retry took the last token: the next turn starts after it.
    last_served: Option<Uuid>,
    /// The previous search's snapshot, from which the next search reads, or none before the
    /// first search.
    snapshot: Option<Snapshot>,
}

/// What the relay knows of one delivery.
#[derive(Debug, Clone, Copy)]
struct Known {
    delivery: Delivery,
    /// The retry that the next attempt is, counted from 1, or 0 for a first attempt, which
    /// takes neither a token nor a turn.
    retry: u32,
    /// When the next attempt is due.
    due: OffsetDateTime,
    /// When an attempt of it first failed in this process, for the three days.
    first_failure: Option<OffsetDateTime>,
}

impl Relay {
    /// A relay that knows nothing, with ADR-0053's budget, full at `now`.
    #[must_use]
    pub fn new(now: OffsetDateTime) -> Self {
        Self::with_budget(RetryBudget::standard(now))
    }

    /// A relay that knows nothing, with this budget.
    #[must_use]
    pub fn with_budget(budget: RetryBudget) -> Self {
        Self {
            budget,
            pending: Vec::new(),
            known: BTreeMap::new(),
            in_flight: HashMap::new(),
            disabled: HashSet::new(),
            last_served: None,
            snapshot: None,
        }
    }

    /// Takes deliveries that the catch-up has just committed: each is a first attempt, due
    /// at `now`, which takes no token (ADR-0052). One whose transaction had completed at the
    /// relay's snapshot is left: the first search whose snapshot had it completed read it
    /// and learned it, unless it was finished then (ADR-0064). So is one that the relay
    /// knows already, whose success it holds, or of an endpoint that it disabled.
    pub fn hand_over(
        &mut self,
        deliveries: impl IntoIterator<Item = Delivery>,
        now: OffsetDateTime,
    ) {
        for delivery in deliveries {
            let searched = self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.completed(delivery.xact));
            if !searched {
                self.learn(delivery, 0, now);
            }
        }
    }

    /// What the next search reads: every delivery not finished the first time, then those
    /// of the transactions that had not completed at the previous search's snapshot. The
    /// server reads the new snapshot in a statement of its own, then the rows in a later
    /// one (ADR-0062).
    #[must_use]
    pub fn begin_search(&self) -> Search {
        self.snapshot.clone().map_or(Search::Full, Search::Since)
    }

    /// What `search` found after the server read `snapshot`: deliveries not finished. The
    /// relay learns only those of transactions that had completed at `snapshot`; a row
    /// that committed after it waits for its handover, or for the next search, which reads
    /// it again (ADR-0064). One that the relay does not know is a retry when the search was
    /// the first, due after a first retry's delay drawn with `random`, for a token, since it
    /// may have been attempted before the restart. After that it is a first attempt, due at
    /// `now`, since no process can have attempted it. The relay keeps `snapshot` for the
    /// next search, and forgets nothing that a search does not return (ADR-0062).
    pub fn searched(
        &mut self,
        search: Search,
        snapshot: Snapshot,
        found: impl IntoIterator<Item = Delivery>,
        now: OffsetDateTime,
        random: &mut impl FnMut() -> u64,
    ) {
        let first = search == Search::Full;
        // `learn` leaves a delivery that the relay knows as it is, and one whose success it
        // holds: only its record failed.
        for delivery in found {
            if !snapshot.completed(delivery.xact) {
                continue;
            }
            if first {
                let due = retries::after(now, retries::delay(1, None, random()));
                self.learn(delivery, 1, due);
            } else {
                self.learn(delivery, 0, now);
            }
        }
        self.snapshot = Some(snapshot);
    }

    /// One step at `now` (ADR-0053): the effects not recorded yet, among them a disablement
    /// of each endpoint with a delivery that has gone [`FAILING_AFTER`] without a success,
    /// then the attempts to start: every first attempt that is due, then the retries that
    /// are due, one per endpoint per round, each for a token, in the order of the
    /// endpoints' IDs from after the one served last, until the tokens or the retries run
    /// out. An endpoint with [`AT_ONCE`] attempts in flight gets none.
    pub fn step(&mut self, now: OffsetDateTime) -> Step {
        let failing: Vec<EndpointId> = self
            .known
            .values()
            .filter(|known| {
                known
                    .first_failure
                    .is_some_and(|first| retries::waited(first, now) >= FAILING_AFTER)
            })
            .map(|known| known.delivery.endpoint)
            .collect();
        for endpoint in failing {
            if let Some(effect) = self.disable(endpoint, Reason::Failing) {
                self.pending.push(effect);
            }
        }
        let attempts = self.attempts(now);
        Step {
            effects: self.pending.clone(),
            attempts,
        }
    }

    /// The attempts to start at `now`.
    fn attempts(&mut self, now: OffsetDateTime) -> Vec<Delivery> {
        let mut started = Vec::new();
        let firsts: Vec<Delivery> = self
            .known
            .values()
            .filter(|known| known.retry == 0 && self.waiting(known, now))
            .map(|known| known.delivery)
            .collect();
        for delivery in firsts {
            if self.has_room(delivery.endpoint) {
                self.start(delivery, &mut started);
            }
        }
        loop {
            // Each endpoint's next retry: the earliest due, the oldest delivery on a tie.
            let mut round: BTreeMap<Uuid, (OffsetDateTime, Delivery)> = BTreeMap::new();
            for known in self.known.values() {
                if known.retry > 0
                    && self.waiting(known, now)
                    && self.has_room(known.delivery.endpoint)
                {
                    round
                        .entry(known.delivery.endpoint.get())
                        .and_modify(|next| {
                            if known.due < next.0 {
                                *next = (known.due, known.delivery);
                            }
                        })
                        .or_insert((known.due, known.delivery));
                }
            }
            if round.is_empty() {
                return started;
            }
            let order: Vec<Delivery> = match self.last_served {
                Some(last) => round
                    .range((Bound::Excluded(last), Bound::Unbounded))
                    .chain(round.range(..=last))
                    .map(|(_, next)| next.1)
                    .collect(),
                None => round.values().map(|next| next.1).collect(),
            };
            for delivery in order {
                if !self.budget.take(now) {
                    return started;
                }
                self.last_served = Some(delivery.endpoint.get());
                self.start(delivery, &mut started);
            }
        }
    }

    /// What the attempt of `delivery` got, at `now`, with `draw` for the next delay: the
    /// effect for the server to carry out, if any, which each step hands back until the
    /// server reports it recorded. A `2xx` is recorded even when the delivery is forgotten,
    /// and an answer to no attempt in flight changes nothing.
    pub fn answered(
        &mut self,
        delivery: DeliveryId,
        answer: Answer,
        now: OffsetDateTime,
        draw: u64,
    ) -> Option<Effect> {
        let endpoint = self.in_flight.remove(&delivery.get())?;
        let effect = match verdict(answer) {
            Verdict::Delivered => {
                self.known.remove(&delivery.get());
                Some(Effect::Record(delivery))
            }
            Verdict::Gone => self.disable(endpoint, Reason::Gone),
            Verdict::Again { at_least } => {
                if let Some(known) = self.known.get_mut(&delivery.get()) {
                    known.retry = known
                        .retry
                        .checked_add(1)
                        .expect("fewer than 2^32 retries of a delivery");
                    known.due = retries::after(now, retries::delay(known.retry, at_least, draw));
                    known.first_failure.get_or_insert(now);
                }
                None
            }
        };
        self.pending.extend(effect);
        effect
    }

    /// The task recorded `effect` (ADR-0053): the relay stops handing it back. Until then,
    /// every step hands back each effect that the relay returned, from `answered` or from a
    /// step. Nothing else records it before a restart: a search after the first reads only
    /// the transactions not completed at the previous one (ADR-0062), and the relay leaves
    /// a delivery whose success it holds or whose endpoint it disabled.
    pub fn recorded(&mut self, effect: Effect) {
        self.pending.retain(|owed| *owed != effect);
    }

    /// The attempt of `delivery` found its endpoint disabled, by the operator or before a
    /// restart: the attempt ends, and so do the endpoint's deliveries, with nothing to
    /// record (ADR-0062).
    pub fn found_disabled(&mut self, delivery: DeliveryId) {
        if let Some(endpoint) = self.in_flight.remove(&delivery.get()) {
            self.forget_endpoint(endpoint);
        }
    }

    /// The attempt of `delivery` was not made: the service's database did not give the
    /// endpoint's URL, its disablement or the event's body (ADR-0053), or the attempt
    /// panicked, a bug of ours that the task logs as an error. The attempt ends, and the
    /// delivery waits like a retry, for its delay drawn with `draw` and a token, but its
    /// three days do not start: only the endpoint's answers count against it.
    pub fn not_sent(&mut self, delivery: DeliveryId, now: OffsetDateTime, draw: u64) {
        if self.in_flight.remove(&delivery.get()).is_none() {
            return;
        }
        if let Some(known) = self.known.get_mut(&delivery.get()) {
            known.retry = known
                .retry
                .checked_add(1)
                .expect("fewer than 2^32 retries of a delivery");
            known.due = retries::after(now, retries::delay(known.retry, None, draw));
        }
    }

    /// Adds a delivery that the relay does not know, unless its endpoint is disabled or the
    /// relay holds its success.
    fn learn(&mut self, delivery: Delivery, retry: u32, due: OffsetDateTime) {
        if self.disabled.contains(&delivery.endpoint.get())
            || self.pending.contains(&Effect::Record(delivery.id))
        {
            return;
        }
        self.known.entry(delivery.id.get()).or_insert(Known {
            delivery,
            retry,
            due,
            first_failure: None,
        });
    }

    /// Disables `endpoint` once, and forgets its deliveries: the effect, the first time.
    fn disable(&mut self, endpoint: EndpointId, reason: Reason) -> Option<Effect> {
        self.forget_endpoint(endpoint)
            .then_some(Effect::Disable(endpoint, reason))
    }

    /// Counts `endpoint` as disabled, and forgets its deliveries, once: whether this was
    /// the first time.
    fn forget_endpoint(&mut self, endpoint: EndpointId) -> bool {
        if !self.disabled.insert(endpoint.get()) {
            return false;
        }
        self.known
            .retain(|_, known| known.delivery.endpoint != endpoint);
        true
    }

    /// Whether `known` is due at `now` and not in flight.
    fn waiting(&self, known: &Known, now: OffsetDateTime) -> bool {
        known.due <= now && !self.in_flight.contains_key(&known.delivery.id.get())
    }

    /// Whether `endpoint` has fewer than [`AT_ONCE`] attempts in flight.
    fn has_room(&self, endpoint: EndpointId) -> bool {
        self.in_flight
            .values()
            .filter(|other| **other == endpoint)
            .count()
            < AT_ONCE
    }

    /// Starts an attempt of `delivery`.
    fn start(&mut self, delivery: Delivery, started: &mut Vec<Delivery>) {
        self.in_flight.insert(delivery.id.get(), delivery.endpoint);
        started.push(delivery);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::*;
    use crate::times::from_unix_micros;

    fn status(status: u16) -> Answer {
        Answer::Status {
            status,
            retry_after: None,
        }
    }

    fn asking(status: u16, seconds: u32) -> Answer {
        Answer::Status {
            status,
            retry_after: Some(seconds),
        }
    }

    #[test]
    fn a_2xx_is_delivered_whatever_else_it_says() {
        for code in [200, 201, 202, 204, 299] {
            assert_eq!(verdict(status(code)), Verdict::Delivered, "{code}");
        }
        assert_eq!(verdict(asking(200, 60)), Verdict::Delivered);
    }

    #[test]
    fn a_410_is_gone_whatever_else_it_says() {
        assert_eq!(verdict(status(410)), Verdict::Gone);
        assert_eq!(verdict(asking(410, 60)), Verdict::Gone);
    }

    #[test]
    fn anything_else_is_tried_again() {
        // Redirects, which the client does not follow, every other client or server
        // error, and the codes that ask to slow down, sent without a Retry-After.
        for code in [
            100, 199, 300, 301, 302, 304, 307, 308, 400, 401, 403, 404, 409, 411, 429, 500, 502,
            503, 504, 599,
        ] {
            assert_eq!(
                verdict(status(code)),
                Verdict::Again { at_least: None },
                "{code}"
            );
        }
        assert_eq!(verdict(Answer::NoAnswer), Verdict::Again { at_least: None });
    }

    #[test]
    fn a_retry_after_is_the_least_wait() {
        for code in [301, 429, 500, 502, 503, 504] {
            assert_eq!(
                verdict(asking(code, 120)),
                Verdict::Again {
                    at_least: Some(120)
                },
                "{code}"
            );
        }
    }

    /// 2026-01-01T00:00:00Z, in microseconds.
    const START: i64 = 1_767_225_600_000_000;

    /// `millis` milliseconds after [`START`].
    fn at(millis: i64) -> OffsetDateTime {
        millis
            .checked_mul(1_000)
            .and_then(|micros| START.checked_add(micros))
            .and_then(from_unix_micros)
            .expect("a time near the start")
    }

    /// The tests' `n`th UUIDv7, in the order of `n`.
    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(0x0196_f6a0_0000_7000_8000_0000_0000_0000 | n)
    }

    fn endpoint(n: u128) -> EndpointId {
        EndpointId::try_from(uuid(0x1_0000 | n)).expect("a UUIDv7")
    }

    /// The `n`th delivery, to endpoint `to`, written by a transaction that every snapshot in
    /// these tests shows as completed.
    fn delivery(to: u128, n: u128) -> Delivery {
        written(to, n, 0)
    }

    /// The `n`th delivery, to endpoint `to`, written by transaction `xact`.
    fn written(to: u128, n: u128, xact: u64) -> Delivery {
        Delivery {
            id: DeliveryId::try_from(uuid(n)).expect("a UUIDv7"),
            endpoint: endpoint(to),
            event: EventId::try_from(uuid(0x2_0000 | n)).expect("a UUIDv7"),
            xact: XactId(xact),
        }
    }

    /// A relay with this many tokens, earning one a second from [`START`].
    fn holding(tokens: u32) -> Relay {
        Relay::with_budget(RetryBudget::new(tokens, Duration::from_secs(1), at(0)))
    }

    /// A relay whose tokens never run out in a test.
    fn plenty() -> Relay {
        holding(1_000)
    }

    /// A relay that never has a token.
    fn broke() -> Relay {
        Relay::with_budget(RetryBudget::new(0, Duration::from_secs(3_600), at(0)))
    }

    const OK: Answer = Answer::Status {
        status: 200,
        retry_after: None,
    };
    const GONE: Answer = Answer::Status {
        status: 410,
        retry_after: None,
    };
    const FAILED: Answer = Answer::Status {
        status: 500,
        retry_after: None,
    };

    /// A snapshot with this `xmax` and these transactions in progress.
    fn snapshot(xmax: u64, in_progress: &[u64]) -> Snapshot {
        Snapshot {
            xmax: XactId(xmax),
            in_progress: in_progress.iter().copied().map(XactId).collect(),
        }
    }

    /// Hands `deliveries` over at `now`, and checks that a step starts them at once.
    fn started(relay: &mut Relay, deliveries: &[Delivery], now: OffsetDateTime) {
        relay.hand_over(deliveries.iter().copied(), now);
        assert_eq!(relay.step(now).attempts, deliveries);
    }

    /// Starts `deliveries` at [`START`] and fails each with the draw 0, so that each is a
    /// first retry due at once.
    fn failing(relay: &mut Relay, deliveries: &[Delivery]) {
        started(relay, deliveries, at(0));
        for delivery in deliveries {
            assert_eq!(relay.answered(delivery.id, FAILED, at(0), 0), None);
        }
    }

    #[test]
    fn a_delivery_handed_over_is_attempted_at_once_without_a_token() {
        let mut relay = broke();
        let first = delivery(1, 1);
        relay.hand_over([first], at(0));
        assert_eq!(relay.step(at(0)).attempts, [first]);
        // Once: it is in flight.
        assert_eq!(relay.step(at(0)).attempts, []);
    }

    #[test]
    fn a_delivery_that_the_first_search_finds_is_a_retry() {
        // It may have been attempted before the restart. The draw 1,500 puts the first
        // retry's delay, drawn within [0, 2 s], at 1.5 s.
        let found = delivery(1, 1);
        let mut relay = holding(1);
        let search = relay.begin_search();
        assert_eq!(search, Search::Full);
        relay.searched(search, snapshot(1, &[]), [found], at(0), &mut || 1_500);
        assert_eq!(relay.step(at(1_499)).attempts, []);
        assert_eq!(relay.step(at(1_500)).attempts, [found]);
        // Without a token, it waits.
        let mut relay = broke();
        let search = relay.begin_search();
        relay.searched(search, snapshot(1, &[]), [found], at(0), &mut || 1_500);
        assert_eq!(relay.step(at(60_000)).attempts, []);
    }

    #[test]
    fn a_delivery_that_a_later_search_finds_is_attempted_at_once_without_a_token() {
        // Its transaction completed after the first search began, so no process can have
        // attempted it: its handover was dropped.
        let mut relay = broke();
        let first = relay.begin_search();
        relay.searched(first, snapshot(10, &[7]), [], at(0), &mut || 0);
        let found = delivery(1, 1);
        let later = relay.begin_search();
        assert_eq!(later, Search::Since(snapshot(10, &[7])));
        relay.searched(later, snapshot(12, &[]), [found], at(5), &mut || 1_500);
        assert_eq!(relay.step(at(5)).attempts, [found]);
    }

    #[test]
    fn the_nth_retry_waits_the_nth_delay() {
        let mut relay = plenty();
        let late = delivery(1, 1);
        started(&mut relay, &[late], at(0));
        // The first retry's delay is drawn within [0, 2 s], the second's within [0, 4 s].
        assert_eq!(relay.answered(late.id, FAILED, at(10), 1_500), None);
        assert_eq!(relay.step(at(1_509)).attempts, []);
        assert_eq!(relay.step(at(1_510)).attempts, [late]);
        assert_eq!(
            relay.answered(late.id, Answer::NoAnswer, at(2_000), 3_000),
            None
        );
        assert_eq!(relay.step(at(4_999)).attempts, []);
        assert_eq!(relay.step(at(5_000)).attempts, [late]);
    }

    #[test]
    fn a_retry_after_is_the_least_wait_before_the_next_attempt() {
        let mut relay = plenty();
        let slowed = delivery(1, 1);
        started(&mut relay, &[slowed], at(0));
        let asked = Answer::Status {
            status: 429,
            retry_after: Some(120),
        };
        assert_eq!(relay.answered(slowed.id, asked, at(0), 0), None);
        assert_eq!(relay.step(at(119_999)).attempts, []);
        assert_eq!(relay.step(at(120_000)).attempts, [slowed]);
    }

    #[test]
    fn an_empty_bucket_holds_the_retries_for_a_later_step() {
        let mut relay = holding(1);
        let (first, second) = (delivery(1, 1), delivery(2, 2));
        failing(&mut relay, &[first, second]);
        assert_eq!(relay.step(at(0)).attempts, [first]);
        assert_eq!(relay.step(at(999)).attempts, []);
        assert_eq!(relay.step(at(1_000)).attempts, [second]);
    }

    #[test]
    fn the_tokens_go_to_the_endpoints_in_turn() {
        // A dead endpoint's backlog, and one retry of another endpoint. With a token a
        // second, each step starts after the endpoint served last.
        let dead = [delivery(1, 1), delivery(1, 2), delivery(1, 3)];
        let other = delivery(2, 4);
        let all = [dead[0], dead[1], dead[2], other];
        let mut relay = holding(1);
        failing(&mut relay, &all);
        assert_eq!(relay.step(at(0)).attempts, [dead[0]]);
        assert_eq!(relay.step(at(1_000)).attempts, [other]);
        assert_eq!(relay.step(at(2_000)).attempts, [dead[1]]);
        // A round gives each endpoint one retry, and rounds go on while tokens last.
        let mut relay = holding(2);
        failing(&mut relay, &all);
        assert_eq!(relay.step(at(0)).attempts, [dead[0], other]);
        let mut relay = holding(4);
        failing(&mut relay, &all);
        assert_eq!(
            relay.step(at(0)).attempts,
            [dead[0], other, dead[1], dead[2]]
        );
    }

    #[test]
    fn a_fifth_attempt_at_one_endpoint_waits_while_the_others_go() {
        let mut relay = plenty();
        let busy = [1, 2, 3, 4, 5].map(|n| delivery(1, n));
        let other = delivery(2, 6);
        relay.hand_over(busy.into_iter().chain([other]), at(0));
        assert_eq!(
            relay.step(at(0)).attempts,
            [busy[0], busy[1], busy[2], busy[3], other]
        );
        assert_eq!(relay.step(at(0)).attempts, []);
        assert_eq!(
            relay.answered(busy[0].id, OK, at(0), 0),
            Some(Effect::Record(busy[0].id))
        );
        assert_eq!(relay.step(at(0)).attempts, [busy[4]]);
    }

    #[test]
    fn a_410_disables_the_endpoint_and_finishes_its_deliveries() {
        let mut relay = plenty();
        let gone = [1, 2, 3, 4, 5].map(|n| delivery(1, n));
        relay.hand_over(gone, at(0));
        assert_eq!(relay.step(at(0)).attempts, gone[..4]);
        assert_eq!(
            relay.answered(gone[0].id, GONE, at(0), 0),
            Some(Effect::Disable(endpoint(1), Reason::Gone))
        );
        // The fifth is finished, and so is any other that arrives for the endpoint.
        assert_eq!(relay.step(at(0)).attempts, []);
        relay.hand_over([delivery(1, 6)], at(0));
        let search = relay.begin_search();
        relay.searched(
            search,
            snapshot(1, &[]),
            [gone[4], delivery(1, 7)],
            at(0),
            &mut || 0,
        );
        assert_eq!(relay.step(at(60_000)).attempts, []);
        // Of the answers still in flight, a 2xx is recorded, and nothing else does anything.
        assert_eq!(
            relay.answered(gone[1].id, OK, at(0), 0),
            Some(Effect::Record(gone[1].id))
        );
        assert_eq!(relay.answered(gone[2].id, FAILED, at(0), 0), None);
        assert_eq!(relay.answered(gone[3].id, GONE, at(0), 0), None);
    }

    #[test]
    fn an_attempt_not_sent_frees_its_place_and_waits_like_a_retry() {
        // The service's database did not give what the attempt needed: its place goes to
        // another delivery, and it waits a retry's delay, for a token.
        let busy = [1, 2, 3, 4, 5].map(|n| delivery(1, n));
        let mut relay = holding(1);
        relay.hand_over(busy, at(0));
        assert_eq!(relay.step(at(0)).attempts, busy[..4]);
        // The draw 1,500 puts the first retry's delay, drawn within [0, 2 s], at 1.5 s.
        relay.not_sent(busy[0].id, at(0), 1_500);
        // Its attempt is no longer in flight, so a late answer to it changes nothing.
        assert_eq!(relay.answered(busy[0].id, OK, at(0), 0), None);
        assert_eq!(relay.step(at(0)).attempts, [busy[4]]);
        for done in &busy[1..] {
            assert_eq!(
                relay.answered(done.id, OK, at(0), 0),
                Some(Effect::Record(done.id))
            );
        }
        assert_eq!(relay.step(at(1_499)).attempts, []);
        assert_eq!(relay.step(at(1_500)).attempts, [busy[0]]);
        // Without a token, it waits.
        let mut relay = broke();
        let single = delivery(2, 6);
        relay.hand_over([single], at(0));
        assert_eq!(relay.step(at(0)).attempts, [single]);
        relay.not_sent(single.id, at(0), 0);
        assert_eq!(relay.step(at(60_000)).attempts, []);
        // An attempt not in flight changes nothing.
        relay.not_sent(single.id, at(0), 0);
        assert_eq!(relay.step(at(60_000)).attempts, []);
    }

    #[test]
    fn an_attempt_not_sent_never_starts_the_three_days() {
        // Not sent at every attempt for four days: only the endpoint's answers count
        // against it, so it is never disabled.
        let mut relay = plenty();
        let stuck = delivery(1, 1);
        relay.hand_over([stuck], at(0));
        assert_eq!(relay.step(at(0)).attempts, [stuck]);
        relay.not_sent(stuck.id, at(0), 0);
        let mut now = 0_i64;
        while now < THREE_DAYS.checked_add(86_400_000).expect("small") {
            now = now.checked_add(3_600_000).expect("a short run");
            let step = relay.step(at(now));
            assert_eq!(step.effects, []);
            assert_eq!(step.attempts, [stuck]);
            relay.not_sent(stuck.id, at(now), 0);
        }
    }

    #[test]
    fn a_delivery_that_failed_keeps_its_three_days_when_not_sent() {
        // Refused at the start, then never sent again: its three days still end it.
        let mut relay = plenty();
        let refused = delivery(1, 1);
        failing(&mut relay, &[refused]);
        let mut now = 0_i64;
        loop {
            now = now.checked_add(3_600_000).expect("a short run");
            let step = relay.step(at(now));
            if now >= THREE_DAYS {
                assert_eq!(
                    step.effects,
                    [Effect::Disable(endpoint(1), Reason::Failing)]
                );
                break;
            }
            assert_eq!(step.effects, []);
            for attempt in step.attempts {
                relay.not_sent(attempt.id, at(now), 0);
            }
        }
    }

    #[test]
    fn a_2xx_finishes_the_delivery() {
        let mut relay = plenty();
        let done = delivery(1, 1);
        started(&mut relay, &[done], at(0));
        assert_eq!(
            relay.answered(done.id, OK, at(0), 0),
            Some(Effect::Record(done.id))
        );
        assert_eq!(relay.step(at(60_000)).attempts, []);
        // An answer to no attempt in flight changes nothing.
        assert_eq!(relay.answered(done.id, OK, at(0), 0), None);
    }

    #[test]
    fn a_delivery_that_a_search_does_not_return_is_kept() {
        // A search returns only what changed since the previous one, so what the relay
        // knows stays until it is finished.
        let mut relay = plenty();
        let waiting = [delivery(1, 1), delivery(2, 2)];
        failing(&mut relay, &waiting);
        let first = relay.begin_search();
        relay.searched(first, snapshot(5, &[]), [], at(0), &mut || 0);
        let later = relay.begin_search();
        relay.searched(later, snapshot(6, &[]), [], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)).attempts, waiting);
    }

    #[test]
    fn a_search_learns_only_the_transactions_completed_at_its_snapshot() {
        // A search's rows are read after its snapshot, so they can show a transaction that
        // committed in between, whose handover is on its way (ADR-0064).
        let mut relay = plenty();
        let first = relay.begin_search();
        relay.searched(first, snapshot(10, &[]), [], at(0), &mut || 0);
        // At the later snapshot 12 has completed, 11 is in progress, and 13 is at xmax.
        let (done, in_progress, at_xmax) =
            (written(1, 1, 12), written(1, 2, 11), written(1, 3, 13));
        let later = relay.begin_search();
        relay.searched(
            later,
            snapshot(13, &[11]),
            [done, in_progress, at_xmax],
            at(0),
            &mut || 0,
        );
        assert_eq!(relay.step(at(0)).attempts, [done]);
        // The next search reads the other two again, and they have completed by then.
        let next = relay.begin_search();
        relay.searched(
            next,
            snapshot(14, &[]),
            [in_progress, at_xmax],
            at(0),
            &mut || 0,
        );
        assert_eq!(relay.step(at(0)).attempts, [in_progress, at_xmax]);
    }

    #[test]
    fn a_handover_is_left_when_its_transaction_completed_at_the_relays_snapshot() {
        // The first search whose snapshot had the transaction completed read its rows and
        // learned what was not finished (ADR-0064).
        let mut relay = plenty();
        let search = relay.begin_search();
        relay.searched(search, snapshot(10, &[7]), [], at(0), &mut || 0);
        let (completed, in_progress, at_xmax) =
            (written(1, 1, 8), written(1, 2, 7), written(1, 3, 10));
        relay.hand_over([completed, in_progress, at_xmax], at(0));
        assert_eq!(relay.step(at(0)).attempts, [in_progress, at_xmax]);
    }

    #[test]
    fn a_delivery_that_a_search_sees_before_its_snapshot_does_is_sent_once() {
        // The search's rows show transaction 12, which committed after its snapshot: the
        // handover that follows is what the relay learns it from, once.
        let mut relay = plenty();
        let first = relay.begin_search();
        relay.searched(first, snapshot(10, &[]), [], at(0), &mut || 0);
        let early = written(1, 1, 12);
        let later = relay.begin_search();
        relay.searched(later, snapshot(12, &[]), [early], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)).attempts, []);
        relay.hand_over([early], at(1));
        assert_eq!(relay.step(at(1)).attempts, [early]);
        let record = Effect::Record(early.id);
        assert_eq!(relay.answered(early.id, OK, at(2), 0), Some(record));
        relay.recorded(record);
        assert_eq!(relay.step(at(3)).attempts, []);
    }

    #[test]
    fn a_handover_that_comes_after_its_delivery_succeeded_is_left() {
        // The catch-up stalled between its COMMIT and its handover while a search learned
        // the delivery and the relay delivered it.
        let mut relay = plenty();
        let first = relay.begin_search();
        relay.searched(first, snapshot(10, &[]), [], at(0), &mut || 0);
        let stalled = written(1, 1, 11);
        let later = relay.begin_search();
        relay.searched(later, snapshot(12, &[]), [stalled], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)).attempts, [stalled]);
        let record = Effect::Record(stalled.id);
        assert_eq!(relay.answered(stalled.id, OK, at(1), 0), Some(record));
        relay.recorded(record);
        relay.hand_over([stalled], at(2));
        assert_eq!(relay.step(at(2)).attempts, []);
    }

    #[test]
    fn the_first_search_leaves_what_committed_during_it_to_the_next_as_a_first_attempt() {
        // Transaction 10 committed after the first snapshot, so after this process
        // started: no earlier process attempted its delivery.
        let mut relay = broke();
        let during = written(1, 1, 10);
        let first = relay.begin_search();
        relay.searched(first, snapshot(10, &[]), [during], at(0), &mut || 0);
        let later = relay.begin_search();
        relay.searched(later, snapshot(11, &[]), [during], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)).attempts, [during]);
    }

    #[test]
    fn a_delivery_handed_over_during_a_search_is_kept() {
        let mut relay = plenty();
        let late = delivery(1, 1);
        let search = relay.begin_search();
        relay.hand_over([late], at(0));
        relay.searched(search, snapshot(1, &[]), [], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)).attempts, [late]);
    }

    /// `micros` microseconds after `time`, or before it when negative.
    fn moved(time: OffsetDateTime, micros: i64) -> OffsetDateTime {
        crate::times::unix_micros(time)
            .checked_add(micros)
            .and_then(from_unix_micros)
            .expect("a time near the start")
    }

    /// Three days, in milliseconds.
    const THREE_DAYS: i64 = 259_200_000;

    #[test]
    fn one_delivery_refused_for_three_days_disables_its_endpoint() {
        // The endpoint takes every other delivery, and always refuses this one.
        let mut relay = plenty();
        let refused = delivery(1, 1);
        let taken = [delivery(1, 2), delivery(1, 3)];
        failing(&mut relay, &[refused]);
        for (n, &delivery) in taken.iter().enumerate() {
            let when = at(i64::try_from(n)
                .expect("two")
                .checked_add(1)
                .expect("small"));
            relay.hand_over([delivery], when);
        }
        let step = relay.step(at(1_000));
        assert_eq!(step.effects, []);
        for attempt in step.attempts {
            if attempt == refused {
                assert_eq!(relay.answered(attempt.id, FAILED, at(1_000), 0), None);
            } else {
                assert_eq!(
                    relay.answered(attempt.id, OK, at(1_000), 0),
                    Some(Effect::Record(attempt.id))
                );
                relay.recorded(Effect::Record(attempt.id));
            }
        }
        // Counted from its first failed attempt, at the start: not a microsecond before.
        let due = at(THREE_DAYS);
        let before = relay.step(moved(due, -1));
        assert_eq!(before.effects, []);
        assert_eq!(before.attempts, [refused]);
        let at_three_days = relay.step(due);
        assert_eq!(
            at_three_days.effects,
            [Effect::Disable(endpoint(1), Reason::Failing)]
        );
        assert_eq!(at_three_days.attempts, []);
        relay.recorded(Effect::Disable(endpoint(1), Reason::Failing));
        // Once: its deliveries leave with it, and none comes back.
        relay.hand_over([delivery(1, 4)], due);
        assert_eq!(
            relay.step(due),
            Step {
                effects: vec![],
                attempts: vec![]
            }
        );
        // The attempt still in flight: a 2xx is still recorded.
        assert_eq!(
            relay.answered(refused.id, OK, due, 0),
            Some(Effect::Record(refused.id))
        );
    }

    #[test]
    fn an_endpoint_that_refuses_everything_is_disabled_after_three_days() {
        let mut relay = plenty();
        let refused = [delivery(1, 1), delivery(1, 2)];
        let elsewhere = delivery(2, 3);
        failing(&mut relay, &[refused[0], refused[1], elsewhere]);
        // An hour later the other endpoint's retry succeeds, and the first endpoint's fail.
        let step = relay.step(at(3_600_000));
        for attempt in step.attempts {
            let answer = if attempt == elsewhere { OK } else { FAILED };
            relay.answered(attempt.id, answer, at(3_600_000), 0);
        }
        relay.recorded(Effect::Record(elsewhere.id));
        let due = relay.step(at(THREE_DAYS));
        assert_eq!(due.effects, [Effect::Disable(endpoint(1), Reason::Failing)]);
        assert_eq!(due.attempts, []);
    }

    #[test]
    fn a_relay_counts_from_its_own_first_failure() {
        // After a restart the clock starts again: a delivery first refused 71 hours into
        // the old relay's count, and found by the new relay's search, has three more days.
        let refused = delivery(1, 1);
        let mut relay = plenty();
        let search = relay.begin_search();
        let restart = at(255_600_000);
        relay.searched(search, snapshot(1, &[]), [refused], restart, &mut || 0);
        assert_eq!(relay.step(restart).attempts, [refused]);
        assert_eq!(relay.answered(refused.id, FAILED, restart, 0), None);
        let old_due = relay.step(at(THREE_DAYS));
        assert_eq!(old_due.effects, []);
        let new_due = moved(restart, 259_200_000_000);
        assert_eq!(
            relay.step(new_due).effects,
            [Effect::Disable(endpoint(1), Reason::Failing)]
        );
    }

    #[test]
    fn each_search_reads_from_the_previous_searchs_snapshot() {
        let mut relay = plenty();
        assert_eq!(relay.begin_search(), Search::Full);
        // A search that never came back changes nothing.
        assert_eq!(relay.begin_search(), Search::Full);
        relay.searched(Search::Full, snapshot(40, &[31, 37]), [], at(0), &mut || 0);
        assert_eq!(relay.begin_search(), Search::Since(snapshot(40, &[31, 37])));
        relay.searched(
            Search::Since(snapshot(40, &[31, 37])),
            snapshot(45, &[37]),
            [],
            at(0),
            &mut || 0,
        );
        assert_eq!(relay.begin_search(), Search::Since(snapshot(45, &[37])));
    }

    #[test]
    fn an_attempt_that_finds_its_endpoint_disabled_ends_its_deliveries() {
        // The operator disabled the endpoint: nothing is recorded, and nothing more is
        // attempted for it.
        let mut relay = plenty();
        let ended = [1, 2, 3, 4, 5].map(|n| delivery(1, n));
        let other = delivery(2, 6);
        relay.hand_over(ended.into_iter().chain([other]), at(0));
        assert_eq!(
            relay.step(at(0)).attempts,
            [ended[0], ended[1], ended[2], ended[3], other]
        );
        relay.found_disabled(ended[0].id);
        // Its attempt is no longer in flight, so a late answer to it changes nothing.
        assert_eq!(relay.answered(ended[0].id, OK, at(0), 0), None);
        // The fifth is not attempted, nor anything that arrives for the endpoint.
        relay.hand_over([delivery(1, 7)], at(0));
        let search = relay.begin_search();
        relay.searched(search, snapshot(1, &[]), [delivery(1, 8)], at(0), &mut || 0);
        assert_eq!(relay.step(at(60_000)).attempts, []);
        // The other endpoint goes on, and of the answers still in flight, a 2xx is
        // recorded and nothing else does anything.
        assert_eq!(
            relay.answered(other.id, OK, at(0), 0),
            Some(Effect::Record(other.id))
        );
        relay.recorded(Effect::Record(other.id));
        assert_eq!(
            relay.answered(ended[1].id, OK, at(0), 0),
            Some(Effect::Record(ended[1].id))
        );
        relay.recorded(Effect::Record(ended[1].id));
        relay.found_disabled(ended[2].id);
        assert_eq!(relay.answered(ended[3].id, FAILED, at(0), 0), None);
        assert_eq!(relay.answered(ended[3].id, GONE, at(0), 0), None);
        assert_eq!(
            relay.step(at(THREE_DAYS)),
            Step {
                effects: vec![],
                attempts: vec![]
            }
        );
    }

    /// One thing that happens in a model of PostgreSQL's transactions, as far as a search
    /// needs them (ADR-0062).
    #[derive(Debug, Clone)]
    enum Tx {
        /// A transaction writes for the first time, and takes the next ID.
        Begin,
        /// The open transaction at this place writes an event and one delivery.
        Write { which: usize },
        /// The open transaction at this place commits, or rolls back.
        End { which: usize, commit: bool },
        /// The unfinished delivery at this place is finished: a transaction of its own
        /// records its success, or its endpoint's disablement, and commits.
        Finish { which: usize },
        /// The relay begins a search and reads the snapshot, in a statement of its own.
        ReadSnapshot,
        /// The search reads the deliveries not finished of the transactions that it names,
        /// in a later statement.
        Search,
    }

    fn tx() -> impl Strategy<Value = Tx> {
        prop_oneof![
            Just(Tx::Begin),
            any::<usize>().prop_map(|which| Tx::Write { which }),
            (any::<usize>(), any::<bool>()).prop_map(|(which, commit)| Tx::End { which, commit }),
            any::<usize>().prop_map(|which| Tx::Finish { which }),
            Just(Tx::ReadSnapshot),
            Just(Tx::Search),
        ]
    }

    /// Where a delivery's row stands in the model.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Row {
        /// Written by a transaction that is still open.
        Uncommitted,
        Unfinished,
        Finished,
    }

    /// The snapshot that PostgreSQL takes now: `xmax` one past the highest completed
    /// transaction, and in progress the open ones below it. An open transaction at or above
    /// `xmax` is in no list.
    fn snapshot_of(open: &[u64], latest_completed: u64) -> Snapshot {
        let xmax = latest_completed.checked_add(1).expect("few transactions");
        Snapshot {
            xmax: XactId(xmax),
            in_progress: open
                .iter()
                .copied()
                .filter(|&xact| xact < xmax)
                .map(XactId)
                .collect(),
        }
    }

    /// Whether `search` reads the rows of the transaction `xact`, as the server's query
    /// does (ADR-0062).
    fn reads(search: &Search, xact: u64) -> bool {
        match search {
            Search::Full => true,
            Search::Since(previous) => {
                xact >= previous.xmax.0 || previous.in_progress.contains(&XactId(xact))
            }
        }
    }

    proptest! {
        #[test]
        fn after_each_search_the_relay_holds_every_delivery_not_finished_that_its_snapshot_shows(
            txs in prop::collection::vec(tx(), 1..120)
        ) {
            let mut relay = plenty();
            let mut next = 1_u64;
            let mut latest_completed = 0_u64;
            let mut open: Vec<u64> = Vec::new();
            let mut rows: Vec<(u64, Delivery, Row)> = Vec::new();
            let mut count = 0_u128;
            let mut pending: Option<(Search, Snapshot)> = None;
            for tx in txs {
                match tx {
                    Tx::Begin => {
                        open.push(next);
                        next = next.checked_add(1).expect("few transactions");
                    }
                    Tx::Write { which } => {
                        if let Some(place) = which.checked_rem(open.len()) {
                            count = count.checked_add(1).expect("few deliveries");
                            let xact = open[place];
                            rows.push((xact, written(1, count, xact), Row::Uncommitted));
                        }
                    }
                    Tx::End { which, commit } => {
                        if let Some(place) = which.checked_rem(open.len()) {
                            let ended = open.remove(place);
                            latest_completed = latest_completed.max(ended);
                            rows.retain(|row| commit || row.0 != ended);
                            for row in rows.iter_mut().filter(|row| row.0 == ended) {
                                row.2 = Row::Unfinished;
                            }
                        }
                    }
                    Tx::Finish { which } => {
                        let unfinished: Vec<usize> = (0..rows.len())
                            .filter(|place| rows[*place].2 == Row::Unfinished)
                            .collect();
                        if let Some(place) = which.checked_rem(unfinished.len()) {
                            rows[unfinished[place]].2 = Row::Finished;
                            latest_completed = latest_completed.max(next);
                            next = next.checked_add(1).expect("few transactions");
                        }
                    }
                    Tx::ReadSnapshot => {
                        if pending.is_none() {
                            let search = relay.begin_search();
                            pending = Some((search, snapshot_of(&open, latest_completed)));
                        }
                    }
                    Tx::Search => {
                        if let Some((search, snapshot)) = pending.take() {
                            let found: Vec<Delivery> = rows
                                .iter()
                                .filter(|row| row.2 == Row::Unfinished && reads(&search, row.0))
                                .map(|row| row.1)
                                .collect();
                            let taken = snapshot.clone();
                            relay.searched(search, snapshot, found, at(0), &mut || 0);
                            // Every delivery not finished whose transaction had completed at
                            // the snapshot, the relay now holds, whichever transactions the
                            // search read. The others wait for their handovers or the next
                            // search (ADR-0064).
                            for (xact, delivery, row) in &rows {
                                if *row == Row::Unfinished && taken.completed(XactId(*xact)) {
                                    prop_assert!(
                                        relay.known.contains_key(&delivery.id.get()),
                                        "{:?} of {} missed", delivery, xact
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_success_comes_back_at_each_step_until_it_is_recorded() {
        let mut relay = plenty();
        let done = delivery(1, 1);
        relay.hand_over([done], at(0));
        assert_eq!(relay.step(at(0)).attempts, [done]);
        let record = Effect::Record(done.id);
        assert_eq!(relay.answered(done.id, OK, at(1), 0), Some(record));
        // Its record failed: each step hands it back, and attempts nothing again.
        for later in [2, 3] {
            assert_eq!(
                relay.step(at(later)),
                Step {
                    effects: vec![record],
                    attempts: vec![]
                }
            );
        }
        relay.recorded(record);
        assert_eq!(
            relay.step(at(4)),
            Step {
                effects: vec![],
                attempts: vec![]
            }
        );
    }

    #[test]
    fn recording_one_effect_leaves_the_others_owed() {
        let mut relay = plenty();
        let (first, second) = (delivery(1, 1), delivery(2, 2));
        relay.hand_over([first, second], at(0));
        assert_eq!(relay.step(at(0)).attempts, [first, second]);
        let (one, two) = (Effect::Record(first.id), Effect::Record(second.id));
        assert_eq!(relay.answered(first.id, OK, at(1), 0), Some(one));
        assert_eq!(relay.answered(second.id, OK, at(1), 0), Some(two));
        // The first record failed, and the second went through.
        relay.recorded(two);
        assert_eq!(relay.step(at(2)).effects, [one]);
    }

    #[test]
    fn a_success_not_recorded_yet_is_not_sent_again() {
        let mut relay = plenty();
        let search = relay.begin_search();
        relay.searched(search, snapshot(1, &[]), [], at(0), &mut || 0);
        // Written by transaction 1, which completed after the first snapshot.
        let done = written(1, 1, 1);
        relay.hand_over([done], at(0));
        assert_eq!(relay.step(at(0)).attempts, [done]);
        let record = Effect::Record(done.id);
        assert_eq!(relay.answered(done.id, OK, at(1), 0), Some(record));
        // Its record failed, so it is not finished, and the next search, which reads its
        // transaction and shows it completed, finds it: the relay holds its success, and
        // leaves it.
        let search = relay.begin_search();
        relay.searched(search, snapshot(2, &[]), [done], at(2), &mut || 0);
        assert_eq!(
            relay.step(at(2)),
            Step {
                effects: vec![record],
                attempts: vec![]
            }
        );
    }

    #[test]
    fn a_disablement_comes_back_at_each_step_until_it_is_recorded() {
        let mut relay = plenty();
        let (gone, other) = (delivery(1, 1), delivery(1, 2));
        relay.hand_over([gone], at(0));
        assert_eq!(relay.step(at(0)).attempts, [gone]);
        let disable = Effect::Disable(endpoint(1), Reason::Gone);
        assert_eq!(relay.answered(gone.id, GONE, at(1), 0), Some(disable));
        // A delivery of the endpoint handed over since is left, as the endpoint is.
        relay.hand_over([other], at(2));
        for later in [2, 3] {
            assert_eq!(
                relay.step(at(later)),
                Step {
                    effects: vec![disable],
                    attempts: vec![]
                }
            );
        }
        relay.recorded(disable);
        assert!(relay.step(at(4)).effects.is_empty());
    }

    #[test]
    fn a_failing_endpoints_disablement_comes_back_until_it_is_recorded() {
        let mut relay = plenty();
        let refused = delivery(1, 1);
        relay.hand_over([refused], at(0));
        assert_eq!(relay.step(at(0)).attempts, [refused]);
        assert_eq!(relay.answered(refused.id, FAILED, at(0), 0), None);
        let three_days = i64::try_from(FAILING_AFTER.as_millis()).expect("three days in ms");
        let disable = Effect::Disable(endpoint(1), Reason::Failing);
        for later in [0, 1] {
            let now = three_days.checked_add(later).expect("near the start");
            assert_eq!(relay.step(at(now)).effects, [disable]);
        }
        relay.recorded(disable);
        let after = three_days.checked_add(2).expect("near the start");
        assert!(relay.step(at(after)).effects.is_empty());
    }

    /// When a committed delivery's handover reaches the relay.
    #[derive(Debug, Clone, Copy)]
    enum Handed {
        Now,
        /// Later, when the task reads the handovers queued: a catch-up that stalls between
        /// its `COMMIT` and its handover, or a task busy with other events.
        Late,
        /// Never, as a full channel drops it (ADR-0052).
        Dropped,
    }

    /// One thing that happens around a relay, in a random run.
    #[derive(Debug, Clone)]
    enum Op {
        /// The catch-up commits a delivery to endpoint `to`, in a transaction of its own,
        /// and hands it over.
        Commit { to: u128, handover: Handed },
        /// The task reads every handover queued so far.
        Deliver,
        /// A step, this many milliseconds after the last thing, whose effects the task
        /// records, or fails to.
        Step { after: i64, recorded: bool },
        /// The attempt at this place among those in flight gets this answer, whose effect
        /// the task records, or fails to.
        Answered {
            which: usize,
            answer: Answer,
            draw: u64,
            recorded: bool,
        },
        /// The attempt at this place among those in flight finds its endpoint disabled by
        /// the operator.
        FoundDisabled { which: usize },
        /// The attempt at this place among those in flight is not sent: the service's
        /// database did not give what it needed.
        NotSent { which: usize, draw: u64 },
        /// A search begins and reads the snapshot, in a statement of its own (ADR-0062).
        ReadSnapshot,
        /// The search that began reads its rows, in a later statement: a commit in between
        /// shows in the rows and not in the snapshot (ADR-0064).
        Search,
    }

    fn op() -> impl Strategy<Value = Op> {
        let answer = prop_oneof![
            Just(OK),
            Just(GONE),
            Just(FAILED),
            Just(Answer::NoAnswer),
            (1_u32..5).prop_map(|seconds| Answer::Status {
                status: 429,
                retry_after: Some(seconds)
            }),
        ];
        // Most records go through; the others wait for a later step.
        let recorded = || prop::bool::weighted(0.8);
        let handed = prop_oneof![
            2 => Just(Handed::Now),
            1 => Just(Handed::Late),
            1 => Just(Handed::Dropped),
        ];
        prop_oneof![
            (1_u128..=3, handed).prop_map(|(to, handover)| Op::Commit { to, handover }),
            Just(Op::Deliver),
            (0_i64..3_000, recorded()).prop_map(|(after, recorded)| Op::Step { after, recorded }),
            // A day or more, so that the three days pass in a run.
            (86_400_000_i64..90_000_000, recorded())
                .prop_map(|(after, recorded)| Op::Step { after, recorded }),
            (any::<usize>(), answer, any::<u64>(), recorded()).prop_map(
                |(which, answer, draw, recorded)| Op::Answered {
                    which,
                    answer,
                    draw,
                    recorded,
                }
            ),
            any::<usize>().prop_map(|which| Op::FoundDisabled { which }),
            (any::<usize>(), any::<u64>()).prop_map(|(which, draw)| Op::NotSent { which, draw }),
            Just(Op::ReadSnapshot),
            Just(Op::Search),
        ]
    }

    /// The database, as far as a run needs it: the deliveries not finished, each with its
    /// transaction, and the endpoints disabled.
    #[derive(Default)]
    struct Db {
        unfinished: Vec<(Delivery, u64)>,
        disabled: HashSet<EndpointId>,
    }

    impl Db {
        /// The endpoint is disabled, and its deliveries are finished with it.
        fn disable(&mut self, endpoint: EndpointId) {
            self.disabled.insert(endpoint);
            self.unfinished
                .retain(|(delivery, _)| delivery.endpoint != endpoint);
        }
    }

    /// The task's record of `effect`: when it goes through, the database holds the success
    /// or the disablement, the relay hears so, and the effect is owed no longer; otherwise
    /// it stays owed, and the database is as it was.
    fn settle(
        relay: &mut Relay,
        pending: &mut Vec<Effect>,
        db: &mut Db,
        effect: Effect,
        recorded: bool,
    ) {
        if !recorded {
            if !pending.contains(&effect) {
                pending.push(effect);
            }
            return;
        }
        match effect {
            Effect::Record(id) => db.unfinished.retain(|(delivery, _)| delivery.id != id),
            Effect::Disable(endpoint, _) => db.disable(endpoint),
        }
        relay.recorded(effect);
        pending.retain(|owed| *owed != effect);
    }

    /// Runs `ops` against a relay and a model of the database, and checks after each that
    /// no endpoint has more than [`AT_ONCE`] attempts in flight, that nothing is attempted
    /// for an endpoint after it is disabled, and that the relay holds every delivery that
    /// is not finished and that it has heard of, or owes what finishes it. An endpoint is
    /// disabled as failing only once one of its attempts got an answer that failed, and
    /// every effect that the relay returned comes back at each step until the task records
    /// it. A relay without tokens must never retry: it attempts each delivery once at most,
    /// whether its handover comes at once, late or never, and never one that only the first
    /// search found, which may have been attempted before a restart.
    fn run(ops: &[Op], tokens: u32) -> Result<(), TestCaseError> {
        let mut relay = holding(tokens);
        let mut pending: Vec<Effect> = Vec::new();
        let mut now = 0_i64;
        let mut next = 0_u128;
        // Each commit is a transaction of its own, so no snapshot has one in progress.
        let mut xact = 0_u64;
        let mut db = Db::default();
        // The handovers that the task has not read yet.
        let mut queued: Vec<Delivery> = Vec::new();
        // The search that has read its snapshot and not its rows yet.
        let mut taken: Option<(Search, Snapshot)> = None;
        let mut in_flight: Vec<Delivery> = Vec::new();
        // The endpoints that the relay knows are disabled: those it disabled, which the
        // database learns of once the task records it, and those an attempt found disabled.
        let mut disabled: HashSet<EndpointId> = HashSet::new();
        let mut attempted: HashSet<DeliveryId> = HashSet::new();
        let mut found_first: HashSet<DeliveryId> = HashSet::new();
        let mut heard: HashSet<DeliveryId> = HashSet::new();
        // The endpoints with an attempt that got an answer that failed.
        let mut failed: HashSet<EndpointId> = HashSet::new();
        for op in ops {
            match *op {
                Op::Commit { to, handover } => {
                    // The catch-up writes to each endpoint that the database holds enabled.
                    if !db.disabled.contains(&endpoint(to)) {
                        next = next.checked_add(1).expect("few deliveries");
                        xact = xact.checked_add(1).expect("few transactions");
                        let new = written(to, next, xact);
                        db.unfinished.push((new, xact));
                        match handover {
                            Handed::Now => {
                                heard.insert(new.id);
                                relay.hand_over([new], at(now));
                            }
                            Handed::Late => queued.push(new),
                            Handed::Dropped => {}
                        }
                    }
                }
                Op::Deliver => {
                    let late = std::mem::take(&mut queued);
                    heard.extend(late.iter().map(|delivery| delivery.id));
                    relay.hand_over(late, at(now));
                }
                Op::Step { after, recorded } => {
                    now = now.checked_add(after).expect("a short run");
                    let step = relay.step(at(now));
                    // Each effect not recorded yet comes back, and the others are new.
                    for effect in &pending {
                        prop_assert!(step.effects.contains(effect), "not handed back: {effect:?}");
                    }
                    let new: Vec<Effect> = step
                        .effects
                        .iter()
                        .filter(|effect| !pending.contains(effect))
                        .copied()
                        .collect();
                    prop_assert_eq!(
                        step.effects.len(),
                        pending.len().checked_add(new.len()).expect("few effects")
                    );
                    for effect in new {
                        let Effect::Disable(endpoint, Reason::Failing) = effect else {
                            return Err(TestCaseError::fail(format!("{effect:?}")));
                        };
                        prop_assert!(disabled.insert(endpoint), "disabled twice: {endpoint:?}");
                        prop_assert!(failed.contains(&endpoint), "never failed: {endpoint:?}");
                    }
                    for effect in step.effects {
                        settle(&mut relay, &mut pending, &mut db, effect, recorded);
                    }
                    for attempt in step.attempts {
                        prop_assert!(!disabled.contains(&attempt.endpoint), "{attempt:?}");
                        if tokens == 0 {
                            prop_assert!(!attempted.contains(&attempt.id), "a retry: {attempt:?}");
                            prop_assert!(
                                !found_first.contains(&attempt.id),
                                "a retry: {attempt:?}"
                            );
                        }
                        attempted.insert(attempt.id);
                        in_flight.push(attempt);
                    }
                }
                Op::Answered {
                    which,
                    answer,
                    draw,
                    recorded,
                } => {
                    if in_flight.is_empty() {
                        continue;
                    }
                    let place = which.checked_rem(in_flight.len()).expect("some in flight");
                    let attempt = in_flight.remove(place);
                    let effect = relay.answered(attempt.id, answer, at(now), draw);
                    match verdict(answer) {
                        Verdict::Delivered => {
                            prop_assert_eq!(effect, Some(Effect::Record(attempt.id)));
                        }
                        Verdict::Gone if disabled.insert(attempt.endpoint) => {
                            prop_assert_eq!(
                                effect,
                                Some(Effect::Disable(attempt.endpoint, Reason::Gone))
                            );
                        }
                        Verdict::Gone => prop_assert_eq!(effect, None),
                        Verdict::Again { .. } => {
                            prop_assert_eq!(effect, None);
                            failed.insert(attempt.endpoint);
                        }
                    }
                    if let Some(effect) = effect {
                        settle(&mut relay, &mut pending, &mut db, effect, recorded);
                    }
                }
                Op::NotSent { which, draw } => {
                    if in_flight.is_empty() {
                        continue;
                    }
                    let place = which.checked_rem(in_flight.len()).expect("some in flight");
                    let attempt = in_flight.remove(place);
                    relay.not_sent(attempt.id, at(now), draw);
                }
                Op::FoundDisabled { which } => {
                    if in_flight.is_empty() {
                        continue;
                    }
                    let place = which.checked_rem(in_flight.len()).expect("some in flight");
                    let attempt = in_flight.remove(place);
                    relay.found_disabled(attempt.id);
                    disabled.insert(attempt.endpoint);
                    db.disable(attempt.endpoint);
                }
                Op::ReadSnapshot => {
                    if taken.is_none() {
                        taken = Some((relay.begin_search(), snapshot_of(&[], xact)));
                    }
                }
                Op::Search => {
                    let Some((search, snapshot)) = taken.take() else {
                        continue;
                    };
                    let found: Vec<Delivery> = db
                        .unfinished
                        .iter()
                        .filter(|(_, writer)| reads(&search, *writer))
                        .map(|(delivery, _)| *delivery)
                        .collect();
                    // What the search tells the relay: the deliveries of transactions
                    // completed at its snapshot. The others wait for their handovers or the
                    // next search (ADR-0064).
                    let told: Vec<DeliveryId> = found
                        .iter()
                        .filter(|delivery| snapshot.completed(delivery.xact))
                        .map(|delivery| delivery.id)
                        .collect();
                    if search == Search::Full {
                        found_first.extend(told.iter().filter(|id| !heard.contains(id)));
                    }
                    heard.extend(told);
                    relay.searched(search, snapshot, found, at(now), &mut || 0);
                }
            }
            for endpoint in in_flight.iter().map(|attempt| attempt.endpoint) {
                let at_once = in_flight
                    .iter()
                    .filter(|attempt| attempt.endpoint == endpoint)
                    .count();
                prop_assert!(at_once <= AT_ONCE, "{at_once} at once to {endpoint:?}");
            }
            for (delivery, _) in db
                .unfinished
                .iter()
                .filter(|(delivery, _)| heard.contains(&delivery.id))
            {
                let id = delivery.id.get();
                // What finishes it once recorded: its success, or its endpoint's disablement.
                let owed = pending.iter().any(|effect| match *effect {
                    Effect::Record(done) => done == delivery.id,
                    Effect::Disable(endpoint, _) => endpoint == delivery.endpoint,
                });
                prop_assert!(
                    relay.known.contains_key(&id) || relay.in_flight.contains_key(&id) || owed,
                    "lost: {delivery:?}"
                );
            }
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn a_run_keeps_the_relays_rules(ops in prop::collection::vec(op(), 1..80)) {
            run(&ops, 1_000)?;
        }

        #[test]
        fn a_relay_without_tokens_never_retries(ops in prop::collection::vec(op(), 1..80)) {
            run(&ops, 0)?;
        }
    }
}
