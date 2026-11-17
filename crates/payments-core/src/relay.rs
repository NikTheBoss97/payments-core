// Author: Nicola Antonello

//! The relay's rules, apart from its input and output (ADR-0052, ADR-0053): what an
//! endpoint's answer means, which deliveries to attempt at each step, when an endpoint is
//! disabled, and the mark below which a search need not look. The server's task reads the
//! database, posts the attempts and carries out the effects; this module decides. The
//! time and the random draws are inputs, so every rule is tested without a clock.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;

use sqlx::types::time::OffsetDateTime;
use uuid::Uuid;

use crate::ids::{DeliveryId, EndpointId, EventId};
use crate::retries::{self, RetryBudget};

/// At most this many attempts at once to one endpoint (ADR-0053).
pub const AT_ONCE: usize = 4;

/// What one attempt to deliver got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The endpoint answered with this status, and with a `Retry-After` of this many
    /// seconds, if it gave one in seconds.
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
}

/// What the server carries out after an answer (ADR-0053).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Records the delivery's success: a `23505` means that it is recorded already.
    Record(DeliveryId),
    /// Disables the endpoint, and its unfinished deliveries end with it.
    Disable(EndpointId, Reason),
}

/// Why the relay disables an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// It answered `410`.
    Gone,
}

/// A search begun, which can have seen every delivery that the relay learned of before
/// it began (ADR-0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Search {
    number: u64,
}

/// What the relay holds in memory: the deliveries it will attempt, the attempts in flight,
/// the endpoints it disabled, its budget of retries and whose turn is next. A restart
/// starts it empty (ADR-0052).
#[derive(Debug)]
pub struct Relay {
    budget: RetryBudget,
    /// The deliveries to attempt, by ID.
    known: BTreeMap<Uuid, Known>,
    /// The attempts started and not answered yet, by delivery, with their endpoints.
    in_flight: HashMap<Uuid, EndpointId>,
    /// The endpoints that this process disabled: a search that raced the record of a
    /// disablement can still return their deliveries.
    disabled: HashSet<Uuid>,
    /// The endpoint whose retry took the last token: the next turn starts after it.
    last_served: Option<Uuid>,
    /// How many searches have begun.
    searches: u64,
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
    /// How many searches had begun when the relay learned of it: those can miss it.
    learned: u64,
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
            known: BTreeMap::new(),
            in_flight: HashMap::new(),
            disabled: HashSet::new(),
            last_served: None,
            searches: 0,
        }
    }

    /// Takes deliveries that the catch-up has just committed: each is a first attempt, due
    /// at `now`, which takes no token (ADR-0052). One that the relay knows already, or of
    /// an endpoint that it disabled, is left as it is.
    pub fn hand_over(
        &mut self,
        deliveries: impl IntoIterator<Item = Delivery>,
        now: OffsetDateTime,
    ) {
        for delivery in deliveries {
            self.learn(delivery, 0, now);
        }
    }

    /// Begins a search: the server reads the deliveries that are not finished after this.
    pub fn begin_search(&mut self) -> Search {
        let search = Search {
            number: self.searches,
        };
        self.searches = self
            .searches
            .checked_add(1)
            .expect("fewer than 2^64 searches");
        search
    }

    /// What `search` found: the deliveries that are not finished. One that the relay does
    /// not know may have been attempted before a restart, so it is a retry, due after a
    /// first retry's delay drawn with `random`, and it costs a token (ADR-0052). One that
    /// the relay learned of before the search began, and that the search did not return,
    /// is finished, and forgotten.
    pub fn searched(
        &mut self,
        search: Search,
        found: impl IntoIterator<Item = Delivery>,
        now: OffsetDateTime,
        random: &mut impl FnMut() -> u64,
    ) {
        let mut seen = HashSet::new();
        for delivery in found {
            seen.insert(delivery.id.get());
            if !self.known.contains_key(&delivery.id.get()) {
                let due = retries::after(now, retries::delay(1, None, random()));
                self.learn(delivery, 1, due);
            }
        }
        self.known
            .retain(|id, known| seen.contains(id) || known.learned > search.number);
    }

    /// The attempts to start at `now` (ADR-0053): every first attempt that is due, then the
    /// retries that are due, one per endpoint per round, each for a token, in the order of
    /// the endpoints' IDs from after the one served last, until the tokens or the retries
    /// run out. An endpoint with [`AT_ONCE`] attempts in flight gets none.
    pub fn step(&mut self, now: OffsetDateTime) -> Vec<Delivery> {
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
    /// effect for the server to carry out, if any. A `2xx` is recorded even when the
    /// delivery is forgotten, and an answer to no attempt in flight changes nothing.
    pub fn answered(
        &mut self,
        delivery: DeliveryId,
        answer: Answer,
        now: OffsetDateTime,
        draw: u64,
    ) -> Option<Effect> {
        let endpoint = self.in_flight.remove(&delivery.get())?;
        match verdict(answer) {
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
                }
                None
            }
        }
    }

    /// Adds a delivery that the relay does not know, unless its endpoint is disabled.
    fn learn(&mut self, delivery: Delivery, retry: u32, due: OffsetDateTime) {
        if self.disabled.contains(&delivery.endpoint.get()) {
            return;
        }
        let learned = self.searches;
        self.known.entry(delivery.id.get()).or_insert(Known {
            delivery,
            retry,
            due,
            learned,
        });
    }

    /// Disables `endpoint` once, and forgets its deliveries: the effect, the first time.
    fn disable(&mut self, endpoint: EndpointId, reason: Reason) -> Option<Effect> {
        if !self.disabled.insert(endpoint.get()) {
            return None;
        }
        self.known
            .retain(|_, known| known.delivery.endpoint != endpoint);
        Some(Effect::Disable(endpoint, reason))
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

    /// The `n`th delivery, to endpoint `to`.
    fn delivery(to: u128, n: u128) -> Delivery {
        Delivery {
            id: DeliveryId::try_from(uuid(n)).expect("a UUIDv7"),
            endpoint: endpoint(to),
            event: EventId::try_from(uuid(0x2_0000 | n)).expect("a UUIDv7"),
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

    /// Hands `deliveries` over at `now`, and checks that a step starts them at once.
    fn started(relay: &mut Relay, deliveries: &[Delivery], now: OffsetDateTime) {
        relay.hand_over(deliveries.iter().copied(), now);
        assert_eq!(relay.step(now), deliveries);
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
        assert_eq!(relay.step(at(0)), [first]);
        // Once: it is in flight.
        assert_eq!(relay.step(at(0)), []);
    }

    #[test]
    fn a_delivery_that_a_search_finds_is_a_retry() {
        // The draw 1,500 puts the first retry's delay, drawn within [0, 2 s], at 1.5 s.
        let found = delivery(1, 1);
        let mut relay = holding(1);
        let search = relay.begin_search();
        relay.searched(search, [found], at(0), &mut || 1_500);
        assert_eq!(relay.step(at(1_499)), []);
        assert_eq!(relay.step(at(1_500)), [found]);
        // Without a token, it waits.
        let mut relay = broke();
        let search = relay.begin_search();
        relay.searched(search, [found], at(0), &mut || 1_500);
        assert_eq!(relay.step(at(60_000)), []);
    }

    #[test]
    fn the_nth_retry_waits_the_nth_delay() {
        let mut relay = plenty();
        let late = delivery(1, 1);
        started(&mut relay, &[late], at(0));
        // The first retry's delay is drawn within [0, 2 s], the second's within [0, 4 s].
        assert_eq!(relay.answered(late.id, FAILED, at(10), 1_500), None);
        assert_eq!(relay.step(at(1_509)), []);
        assert_eq!(relay.step(at(1_510)), [late]);
        assert_eq!(
            relay.answered(late.id, Answer::NoAnswer, at(2_000), 3_000),
            None
        );
        assert_eq!(relay.step(at(4_999)), []);
        assert_eq!(relay.step(at(5_000)), [late]);
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
        assert_eq!(relay.step(at(119_999)), []);
        assert_eq!(relay.step(at(120_000)), [slowed]);
    }

    #[test]
    fn an_empty_bucket_holds_the_retries_for_a_later_step() {
        let mut relay = holding(1);
        let (first, second) = (delivery(1, 1), delivery(2, 2));
        failing(&mut relay, &[first, second]);
        assert_eq!(relay.step(at(0)), [first]);
        assert_eq!(relay.step(at(999)), []);
        assert_eq!(relay.step(at(1_000)), [second]);
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
        assert_eq!(relay.step(at(0)), [dead[0]]);
        assert_eq!(relay.step(at(1_000)), [other]);
        assert_eq!(relay.step(at(2_000)), [dead[1]]);
        // A round gives each endpoint one retry, and rounds go on while tokens last.
        let mut relay = holding(2);
        failing(&mut relay, &all);
        assert_eq!(relay.step(at(0)), [dead[0], other]);
        let mut relay = holding(4);
        failing(&mut relay, &all);
        assert_eq!(relay.step(at(0)), [dead[0], other, dead[1], dead[2]]);
    }

    #[test]
    fn a_fifth_attempt_at_one_endpoint_waits_while_the_others_go() {
        let mut relay = plenty();
        let busy = [1, 2, 3, 4, 5].map(|n| delivery(1, n));
        let other = delivery(2, 6);
        relay.hand_over(busy.into_iter().chain([other]), at(0));
        assert_eq!(
            relay.step(at(0)),
            [busy[0], busy[1], busy[2], busy[3], other]
        );
        assert_eq!(relay.step(at(0)), []);
        assert_eq!(
            relay.answered(busy[0].id, OK, at(0), 0),
            Some(Effect::Record(busy[0].id))
        );
        assert_eq!(relay.step(at(0)), [busy[4]]);
    }

    #[test]
    fn a_410_disables_the_endpoint_and_finishes_its_deliveries() {
        let mut relay = plenty();
        let gone = [1, 2, 3, 4, 5].map(|n| delivery(1, n));
        relay.hand_over(gone, at(0));
        assert_eq!(relay.step(at(0)), gone[..4]);
        assert_eq!(
            relay.answered(gone[0].id, GONE, at(0), 0),
            Some(Effect::Disable(endpoint(1), Reason::Gone))
        );
        // The fifth is finished, and so is any other that arrives for the endpoint.
        assert_eq!(relay.step(at(0)), []);
        relay.hand_over([delivery(1, 6)], at(0));
        let search = relay.begin_search();
        relay.searched(search, [gone[4], delivery(1, 7)], at(0), &mut || 0);
        assert_eq!(relay.step(at(60_000)), []);
        // Of the answers still in flight, a 2xx is recorded, and nothing else does anything.
        assert_eq!(
            relay.answered(gone[1].id, OK, at(0), 0),
            Some(Effect::Record(gone[1].id))
        );
        assert_eq!(relay.answered(gone[2].id, FAILED, at(0), 0), None);
        assert_eq!(relay.answered(gone[3].id, GONE, at(0), 0), None);
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
        assert_eq!(relay.step(at(60_000)), []);
        // An answer to no attempt in flight changes nothing.
        assert_eq!(relay.answered(done.id, OK, at(0), 0), None);
    }

    #[test]
    fn a_delivery_that_a_search_no_longer_returns_is_forgotten() {
        let mut relay = plenty();
        let (finished, waiting) = (delivery(1, 1), delivery(1, 2));
        failing(&mut relay, &[finished, waiting]);
        let search = relay.begin_search();
        relay.searched(search, [waiting], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)), [waiting]);
    }

    #[test]
    fn a_delivery_handed_over_during_a_search_is_kept() {
        let mut relay = plenty();
        let late = delivery(1, 1);
        let search = relay.begin_search();
        relay.hand_over([late], at(0));
        relay.searched(search, [], at(0), &mut || 0);
        assert_eq!(relay.step(at(0)), [late]);
    }

    /// One thing that happens around a relay, in a random run.
    #[derive(Debug, Clone)]
    enum Op {
        /// The catch-up commits a delivery to endpoint `to`, and hands it over, unless the
        /// handover is dropped, as a full channel drops it (ADR-0052).
        Commit { to: u128, dropped: bool },
        /// A step, this many milliseconds after the last thing.
        Step { after: i64 },
        /// The attempt at this place among those in flight gets this answer.
        Answered {
            which: usize,
            answer: Answer,
            draw: u64,
        },
        /// A search, which finds every delivery that is not finished.
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
        prop_oneof![
            (1_u128..=3, any::<bool>()).prop_map(|(to, dropped)| Op::Commit { to, dropped }),
            (0_i64..3_000).prop_map(|after| Op::Step { after }),
            (any::<usize>(), answer, any::<u64>()).prop_map(|(which, answer, draw)| {
                Op::Answered {
                    which,
                    answer,
                    draw,
                }
            }),
            Just(Op::Search),
        ]
    }

    /// Runs `ops` against a relay and a model of the database, and checks after each that
    /// no endpoint has more than [`AT_ONCE`] attempts in flight, that nothing is attempted
    /// for an endpoint after it is disabled, and that the relay holds every delivery that
    /// is not finished and that it has heard of. A relay without tokens must never retry:
    /// it attempts each delivery once at most, and never one that only a search found.
    fn run(ops: &[Op], tokens: u32) -> Result<(), TestCaseError> {
        let mut relay = holding(tokens);
        let mut now = 0_i64;
        let mut next = 0_u128;
        let mut unfinished: Vec<Delivery> = Vec::new();
        let mut in_flight: Vec<Delivery> = Vec::new();
        let mut disabled: HashSet<EndpointId> = HashSet::new();
        let mut attempted: HashSet<DeliveryId> = HashSet::new();
        let mut dropped: HashSet<DeliveryId> = HashSet::new();
        let mut heard: HashSet<DeliveryId> = HashSet::new();
        for op in ops {
            match *op {
                Op::Commit { to, dropped: lost } => {
                    if !disabled.contains(&endpoint(to)) {
                        next = next.checked_add(1).expect("few deliveries");
                        let new = delivery(to, next);
                        unfinished.push(new);
                        if lost {
                            dropped.insert(new.id);
                        } else {
                            heard.insert(new.id);
                            relay.hand_over([new], at(now));
                        }
                    }
                }
                Op::Step { after } => {
                    now = now.checked_add(after).expect("a short run");
                    for attempt in relay.step(at(now)) {
                        prop_assert!(!disabled.contains(&attempt.endpoint), "{attempt:?}");
                        if tokens == 0 {
                            prop_assert!(!attempted.contains(&attempt.id), "a retry: {attempt:?}");
                            prop_assert!(!dropped.contains(&attempt.id), "a retry: {attempt:?}");
                        }
                        attempted.insert(attempt.id);
                        in_flight.push(attempt);
                    }
                }
                Op::Answered {
                    which,
                    answer,
                    draw,
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
                            unfinished.retain(|delivery| delivery.id != attempt.id);
                        }
                        Verdict::Gone if disabled.insert(attempt.endpoint) => {
                            prop_assert_eq!(
                                effect,
                                Some(Effect::Disable(attempt.endpoint, Reason::Gone))
                            );
                            unfinished.retain(|delivery| delivery.endpoint != attempt.endpoint);
                        }
                        Verdict::Gone | Verdict::Again { .. } => prop_assert_eq!(effect, None),
                    }
                }
                Op::Search => {
                    let search = relay.begin_search();
                    heard.extend(unfinished.iter().map(|delivery| delivery.id));
                    relay.searched(search, unfinished.iter().copied(), at(now), &mut || 0);
                }
            }
            for endpoint in in_flight.iter().map(|attempt| attempt.endpoint) {
                let at_once = in_flight
                    .iter()
                    .filter(|attempt| attempt.endpoint == endpoint)
                    .count();
                prop_assert!(at_once <= AT_ONCE, "{at_once} at once to {endpoint:?}");
            }
            for delivery in unfinished
                .iter()
                .filter(|delivery| heard.contains(&delivery.id))
            {
                let id = delivery.id.get();
                prop_assert!(
                    relay.known.contains_key(&id) || relay.in_flight.contains_key(&id),
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
