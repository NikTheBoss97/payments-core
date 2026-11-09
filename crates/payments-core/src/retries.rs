// Author: Nicola Antonello

//! Spacing and bounding retries (ADR-0032, ADR-0042): the delay before each retry, with
//! full jitter, and the one budget that bounds how often the process retries. Time and
//! randomness are inputs, so a test decides both (ADR-0029), and nothing here reads a
//! clock.
//!
//! The lints are the same as in `money`: this module counts time and tokens.
#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::time::Duration;

use sqlx::types::chrono::{DateTime, Utc};

/// The base of the backoff: before the first retry, the delay is at most twice it.
pub const BASE: Duration = Duration::from_secs(1);

/// The cap of the backoff: no delay is longer, but for the PSP's `Retry-After`.
pub const CAP: Duration = Duration::from_secs(3600);

/// The delay before the `retry`th retry, counted from 1 (ADR-0032, ADR-0042): `draw`
/// brought into `[0, min(CAP, BASE × 2^retry)]` milliseconds, Brooker's "Full Jitter",
/// and never shorter than the PSP's `Retry-After`, in seconds.
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

/// The one budget for the process's retries (ADR-0032, ADR-0042): a token bucket that
/// holds up to `capacity` tokens and gains one every `every`, counted from the times
/// that the steps give it. A retry spends a token, and one that finds none waits for a
/// later step. A first send, and the catch-up after a notification, are not retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryBudget {
    capacity: u32,
    /// The time that earns one token, in microseconds, above zero.
    every: u64,
    tokens: u32,
    /// The time up to which the tokens are counted.
    counted_to: DateTime<Utc>,
}

impl RetryBudget {
    /// The budget of ADR-0042: 60 tokens, and one more every second.
    #[must_use]
    pub fn standard(now: DateTime<Utc>) -> Self {
        Self::new(60, Duration::from_secs(1), now)
    }

    /// A full bucket at `now`.
    ///
    /// # Panics
    ///
    /// When `every` is zero, or more than `u64::MAX` microseconds: a bucket must earn its
    /// tokens at some interval.
    #[must_use]
    pub fn new(capacity: u32, every: Duration, now: DateTime<Utc>) -> Self {
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
    pub fn take(&mut self, now: DateTime<Utc>) -> bool {
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
    fn refill(&mut self, now: DateTime<Utc>) {
        let since = now
            .timestamp_micros()
            .checked_sub(self.counted_to.timestamp_micros())
            .expect("two times in chrono's range are an i64 of microseconds apart");
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
        let counted_to = self
            .counted_to
            .timestamp_micros()
            .checked_add(counted)
            .expect("at most now");
        self.counted_to = DateTime::from_timestamp_micros(counted_to).expect("at most now");
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// 2026-01-01T00:00:00Z, in microseconds.
    const START: i64 = 1_767_225_600_000_000;

    /// `millis` milliseconds after [`START`].
    fn at(millis: i64) -> DateTime<Utc> {
        let micros = millis
            .checked_mul(1_000)
            .and_then(|micros| START.checked_add(micros))
            .expect("a time near the start");
        DateTime::from_timestamp_micros(micros).expect("in range")
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
