// Author: Nicola Antonello

//! Spacing and bounding retries (ADR-0032, ADR-0042, ADR-0053): the delay before each
//! retry, with full jitter, the wait that an answer asks for, and the budget that bounds
//! how often a task retries, one for the sender and one for the relay. Time and randomness
//! are inputs, so a test decides both (ADR-0029), and nothing here reads a clock.
//!
//! The lints are the same as in `money`: this module counts time and tokens.
#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::time::{Duration, SystemTime};

use axum::http::{HeaderMap, header};
use sqlx::types::time::OffsetDateTime;

use crate::times::{from_unix_micros, unix_micros};

/// The base of the backoff: before the first retry, the delay is at most twice it.
pub const BASE: Duration = Duration::from_secs(1);

/// The cap of the backoff: no delay is longer, but for a `Retry-After` that asks for more.
pub const CAP: Duration = Duration::from_secs(3600);

/// The delay before the `retry`th retry, counted from 1 (ADR-0032, ADR-0042): `draw`
/// brought into `[0, min(CAP, BASE × 2^retry)]` milliseconds, Brooker's "Full Jitter",
/// and never shorter than a `Retry-After`, the PSP's or a merchant's, in seconds.
#[must_use]
pub fn delay(retry: u32, retry_after: Option<u32>, draw: u64) -> Duration {
    let cap = millis(CAP);
    // An overflow comes only from a retry far past the cap, so it means the cap.
    let bound = 2_u64
        .checked_pow(retry)
        .and_then(|factor| millis(BASE).checked_mul(factor))
        .map_or(cap, |bound| bound.min(cap));
    let jitter = draw
        .checked_rem(bound.checked_add(1).expect("the bound is at most the cap"))
        .expect("a divisor of at least one");
    let floor = retry_after.map_or(0, |seconds| {
        u64::from(seconds)
            .checked_mul(1_000)
            .expect("u32 seconds fit in u64 milliseconds")
    });
    Duration::from_millis(jitter.max(floor))
}

/// A constant duration in milliseconds.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("the base and the cap are an hour at most")
}

/// `delay` after `now`.
pub(crate) fn after(now: OffsetDateTime, delay: Duration) -> OffsetDateTime {
    let micros = i64::try_from(delay.as_micros())
        .expect("a delay of at most u32 seconds is an i64 of microseconds");
    unix_micros(now)
        .checked_add(micros)
        .and_then(from_unix_micros)
        .expect("a time at most u32 seconds after now is in range")
}

/// How long since `first`, or nothing for a time after `now`.
pub(crate) fn waited(first: OffsetDateTime, now: OffsetDateTime) -> Duration {
    unix_micros(now)
        .checked_sub(unix_micros(first))
        .and_then(|micros| u64::try_from(micros).ok())
        .map_or(Duration::ZERO, Duration::from_micros)
}

/// A `Retry-After` in seconds. The HTTP-date form reads as none.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<u32> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A merchant's `Retry-After` (ADR-0053): seconds, or an HTTP-date in any of RFC 9110's
/// three formats, counted from `start`, the attempt's start, and rounded up to a whole
/// second. A date already past asks for no wait, and a wait too long for 32 bits reads as
/// the longest, as do more seconds than 32 bits hold.
#[must_use]
pub fn retry_after_from(headers: &HeaderMap, start: OffsetDateTime) -> Option<u32> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        // Decimal digits fail to read as a u32 only by being too many.
        return Some(value.parse().unwrap_or(u32::MAX));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(wait_until(date, start))
}

/// The wait from `start` until `date`, rounded up to a whole second: none for a date at or
/// before the start, and the longest for one further than 32 bits of seconds. It takes any
/// time that a `SystemTime` holds, before 1970 too, so no date that a parser accepts can
/// make it panic.
fn wait_until(date: SystemTime, start: OffsetDateTime) -> u32 {
    let wait = micros_since_epoch(date)
        .checked_sub(i128::from(unix_micros(start)))
        .expect("two times a u64 of seconds from the epoch are an i128 of microseconds apart");
    // A date at or before the start asks for no wait.
    let wait = u128::try_from(wait).unwrap_or(0);
    u32::try_from(wait.div_ceil(1_000_000)).unwrap_or(u32::MAX)
}

/// A time's microseconds from the epoch, negative before it.
fn micros_since_epoch(time: SystemTime) -> i128 {
    let micros = |span: Duration| {
        i128::try_from(span.as_micros()).expect("a u64 of seconds is an i128 of microseconds")
    };
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => micros(after),
        Err(before) => micros(before.duration())
            .checked_neg()
            .expect("a u64 of seconds in microseconds negates in an i128"),
    }
}

/// The budget for one task's retries, the sender's (ADR-0032, ADR-0042) or the relay's
/// (ADR-0053): a token bucket that holds up to `capacity` tokens and gains one every
/// `every`, counted from the times that the steps give it. A retry spends a token, and
/// one that finds none waits for a later step. A first send or attempt, and the catch-up
/// after a notification, are not retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryBudget {
    capacity: u32,
    /// The time that earns one token, in microseconds, above zero.
    every: u64,
    tokens: u32,
    /// The time up to which the tokens are counted.
    counted_to: OffsetDateTime,
}

impl RetryBudget {
    /// The budget of ADR-0042: 60 tokens, and one more every second.
    #[must_use]
    pub fn standard(now: OffsetDateTime) -> Self {
        Self::new(60, Duration::from_secs(1), now)
    }

    /// A full bucket at `now`.
    ///
    /// # Panics
    ///
    /// When `every` is zero, or more than `u64::MAX` microseconds: a bucket must earn its
    /// tokens at some interval.
    #[must_use]
    pub fn new(capacity: u32, every: Duration, now: OffsetDateTime) -> Self {
        let every = u64::try_from(every.as_micros())
            .ok()
            .filter(|&micros| micros > 0)
            .expect("a budget earns a token at a positive interval");
        Self {
            capacity,
            every,
            tokens: capacity,
            counted_to: now,
        }
    }

    /// Spends a token at `now`, if the bucket holds one.
    pub fn take(&mut self, now: OffsetDateTime) -> bool {
        self.refill(now);
        match self.tokens.checked_sub(1) {
            Some(left) => {
                self.tokens = left;
                true
            }
            None => false,
        }
    }

    /// Adds the tokens earned since the count, and keeps the part of one earned so far.
    /// A full bucket earns nothing, and a time before the count adds nothing, since the
    /// steps may come with their times out of order.
    fn refill(&mut self, now: OffsetDateTime) {
        let since = unix_micros(now)
            .checked_sub(unix_micros(self.counted_to))
            .expect("two times in time's range are an i64 of microseconds apart");
        let Ok(elapsed) = u64::try_from(since) else {
            return;
        };
        let room = self
            .capacity
            .checked_sub(self.tokens)
            .expect("the tokens never pass the capacity");
        let earned = elapsed
            .checked_div(self.every)
            .expect("every is above zero");
        if earned >= u64::from(room) {
            self.tokens = self.capacity;
            self.counted_to = now;
            return;
        }
        let earned_tokens = u32::try_from(earned).expect("fewer than the room left, a u32");
        self.tokens = self
            .tokens
            .checked_add(earned_tokens)
            .expect("at most the capacity");
        let counted = i64::try_from(
            earned
                .checked_mul(self.every)
                .expect("at most the elapsed time"),
        )
        .expect("at most the elapsed time, an i64");
        let counted_to = unix_micros(self.counted_to)
            .checked_add(counted)
            .expect("at most now");
        self.counted_to = from_unix_micros(counted_to).expect("at most now");
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use proptest::prelude::*;

    use super::*;

    /// 2026-01-01T00:00:00Z, in microseconds.
    const START: i64 = 1_767_225_600_000_000;

    /// `millis` milliseconds after [`START`].
    fn at(millis: i64) -> OffsetDateTime {
        let micros = millis
            .checked_mul(1_000)
            .and_then(|micros| START.checked_add(micros))
            .expect("a time near the start");
        from_unix_micros(micros).expect("in range")
    }

    #[test]
    fn the_delay_doubles_its_bound_up_to_the_cap() {
        for (retry, bound) in [
            (0, 1_000),
            (1, 2_000),
            (2, 4_000),
            (11, 2_048_000),
            (12, 3_600_000),
            (40, 3_600_000),
            (64, 3_600_000),
            (u32::MAX, 3_600_000),
        ] {
            assert_eq!(delay(retry, None, 0), Duration::ZERO, "retry {retry}");
            assert_eq!(
                delay(retry, None, bound),
                Duration::from_millis(bound),
                "retry {retry}"
            );
            // The draw is taken modulo the bound plus one, so one past it wraps to zero.
            let past = bound.checked_add(1).expect("a small bound");
            assert_eq!(delay(retry, None, past), Duration::ZERO, "retry {retry}");
        }
    }

    #[test]
    fn retry_after_is_a_floor() {
        assert_eq!(delay(1, Some(120), 0), Duration::from_secs(120));
        assert_eq!(delay(1, Some(1), 2_000), Duration::from_secs(2));
        assert_eq!(
            delay(12, Some(7_200), 3_600_000),
            Duration::from_secs(7_200)
        );
        assert_eq!(delay(1, Some(0), 1_500), Duration::from_millis(1_500));
    }

    #[test]
    fn a_retry_after_is_read_in_seconds_only() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        for (value, read) in [
            ("120", Some(120)),
            (" 7 ", Some(7)),
            ("Wed, 21 Oct 2015 07:28:00 GMT", None),
            ("-1", None),
        ] {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static(value));
            assert_eq!(retry_after(&headers), read, "{value}");
        }
    }

    /// The `Retry-After` of a merchant's answer to an attempt that started at `start`.
    fn read_from(value: &'static str, start: OffsetDateTime) -> Option<u32> {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static(value));
        retry_after_from(&headers, start)
    }

    #[test]
    fn a_merchants_retry_after_in_seconds_saturates_at_the_longest_wait() {
        for (value, read) in [
            ("120", Some(120)),
            (" 7 ", Some(7)),
            ("0", Some(0)),
            ("0080", Some(80)),
            ("4294967295", Some(u32::MAX)),
            ("4294967296", Some(u32::MAX)),
            ("99999999999999999999999999", Some(u32::MAX)),
        ] {
            assert_eq!(read_from(value, at(250)), read, "{value}");
        }
    }

    #[test]
    fn a_merchants_retry_after_reads_each_date_format_from_the_attempts_start() {
        // The attempt started at 00:00:00.250 on 1 January 2026, a Thursday, and each date
        // is two minutes into that day: 119.75 seconds later, rounded up.
        for value in [
            "Thu, 01 Jan 2026 00:02:00 GMT",
            "Thursday, 01-Jan-26 00:02:00 GMT",
            "Thu Jan  1 00:02:00 2026",
        ] {
            assert_eq!(read_from(value, at(250)), Some(120), "{value}");
        }
        // Three quarters of a second still wait a whole one.
        assert_eq!(read_from("Thu, 01 Jan 2026 00:00:01 GMT", at(250)), Some(1));
    }

    #[test]
    fn a_date_already_past_asks_for_no_wait_and_a_far_one_for_the_longest() {
        for (value, read) in [
            ("Thu, 01 Jan 2026 00:00:00 GMT", Some(0)),
            ("Wed, 31 Dec 2025 23:59:00 GMT", Some(0)),
            ("Thu, 01 Jan 1970 00:00:00 GMT", Some(0)),
            ("Fri, 31 Dec 9999 23:59:59 GMT", Some(u32::MAX)),
        ] {
            assert_eq!(read_from(value, at(250)), read, "{value}");
        }
    }

    #[test]
    fn anything_else_in_a_merchants_retry_after_reads_as_none() {
        assert_eq!(retry_after_from(&HeaderMap::new(), at(250)), None);
        for value in [
            "",
            "soon",
            "-1",
            // Rust reads a sign before digits, and RFC 9110's delay-seconds has none.
            "+7",
            "1.5",
            "120 seconds",
            // A date on the wrong weekday, and one not in GMT.
            "Fri, 01 Jan 2026 00:02:00 GMT",
            "Thu, 01 Jan 2026 00:02:00 UTC",
        ] {
            assert_eq!(read_from(value, at(250)), None, "{value}");
        }
    }

    /// `seconds` after the epoch, as a `SystemTime`.
    fn epoch_plus(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds))
            .expect("a time that a SystemTime holds")
    }

    #[test]
    fn a_wait_until_any_time_rounds_up_and_never_panics() {
        // The start is a quarter of a second into 2026, as above.
        let start = at(250);
        let day_before_1970 = SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(86_400))
            .expect("a time that a SystemTime holds");
        let just_after_start = epoch_plus(1_767_225_600)
            .checked_add(Duration::from_micros(250_001))
            .expect("a time that a SystemTime holds");
        for (date, wait) in [
            // The start's own second, already past by a quarter of it.
            (epoch_plus(1_767_225_600), 0),
            // A microsecond after the start still waits a whole second.
            (just_after_start, 1),
            // Two minutes into 2026: 119.75 seconds, rounded up.
            (epoch_plus(1_767_225_720), 120),
            // Before 1970, which no HTTP-date names today: long past.
            (day_before_1970, 0),
            // Ten thousand years on, past the year 9999 and past 32 bits of seconds.
            (epoch_plus(315_360_000_000), u32::MAX),
        ] {
            assert_eq!(wait_until(date, start), wait, "{date:?}");
        }
        // With a start before 1970 too, a date's side of the epoch matters: two minutes
        // after a start a day before it.
        let start_before_1970 = from_unix_micros(-86_400_000_000).expect("in range");
        let two_minutes_on = SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(86_280))
            .expect("a time that a SystemTime holds");
        assert_eq!(wait_until(two_minutes_on, start_before_1970), 120);
    }

    #[test]
    fn a_full_budget_spends_its_tokens_then_waits() {
        let mut budget = RetryBudget::new(3, Duration::from_secs(1), at(0));
        let spent: Vec<bool> = (0..5).map(|_| budget.take(at(0))).collect();
        assert_eq!(spent, [true, true, true, false, false]);
        // A second earns a token, and part of one earns none yet.
        assert!(!budget.take(at(999)));
        assert!(budget.take(at(1_000)));
        assert!(!budget.take(at(1_000)));
        // The part of a token earned so far is kept.
        assert!(!budget.take(at(1_500)));
        assert!(budget.take(at(2_000)));
    }

    #[test]
    fn a_budget_never_holds_more_than_its_capacity() {
        let mut budget = RetryBudget::new(2, Duration::from_secs(1), at(0));
        // An hour later it holds two tokens, not 3,600.
        let spent: Vec<bool> = (0..3).map(|_| budget.take(at(3_600_000))).collect();
        assert_eq!(spent, [true, true, false]);
        // A full bucket earns nothing: an hour of idling, then one spent, leaves one.
        let spent: Vec<bool> = (0..3).map(|_| budget.take(at(7_200_000))).collect();
        assert_eq!(spent, [true, true, false]);
    }

    #[test]
    fn a_time_before_the_count_earns_nothing() {
        let mut budget = RetryBudget::new(1, Duration::from_secs(1), at(10_000));
        assert!(budget.take(at(10_000)));
        assert!(!budget.take(at(0)));
        assert!(!budget.take(at(10_500)));
        assert!(budget.take(at(11_000)));
    }

    #[test]
    fn the_standard_budget_holds_60_and_earns_one_a_second() {
        let mut budget = RetryBudget::standard(at(0));
        let spent = (0..61).filter(|_| budget.take(at(0))).count();
        assert_eq!(spent, 60);
        assert!(!budget.take(at(999)));
        assert!(budget.take(at(1_000)));
    }

    proptest! {
        #[test]
        fn a_delay_stays_between_its_floor_and_its_bound(
            retry in 0_u32..100,
            retry_after in proptest::option::of(0_u32..100_000),
            draw in any::<u64>(),
        ) {
            // The bound restated with Duration's own saturating arithmetic.
            let bound = BASE.saturating_mul(2_u32.saturating_pow(retry)).min(CAP);
            let floor = Duration::from_secs(u64::from(retry_after.unwrap_or(0)));
            let delay = delay(retry, retry_after, draw);
            prop_assert!(delay >= floor, "{delay:?} below {floor:?}");
            prop_assert!(delay <= bound.max(floor), "{delay:?} above {bound:?}");
        }

        #[test]
        fn no_stretch_of_time_holds_more_retries_than_the_budget_earns(
            capacity in 1_u32..20,
            every in 1_u64..5_000,
            steps in prop::collection::vec((0_i64..3_000, 0_u32..5), 1..60),
        ) {
            let mut budget = RetryBudget::new(capacity, Duration::from_millis(every), at(0));
            let (mut now, mut spent) = (0_i64, 0_u64);
            for (gap, tries) in steps {
                now = now.checked_add(gap).expect("a short run");
                for _ in 0..tries {
                    if budget.take(at(now)) {
                        spent = spent.checked_add(1).expect("a short run");
                    }
                }
                let elapsed = u64::try_from(now).expect("time moves forward");
                let earned = elapsed.checked_div(every).expect("every is above zero");
                let allowed = u64::from(capacity).checked_add(earned).expect("a short run");
                prop_assert!(spent <= allowed, "{spent} spent, {allowed} allowed at {now} ms");
            }
        }
    }
}
