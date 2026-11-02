// Author: Nicola Antonello

//! Business times: when something happened in the world, as the caller reports
//! it (ADR-0022). The library never reads a clock for one. The type is always
//! the one that sqlx encodes, because it comes through sqlx's own re-export.

use sqlx::types::chrono::{DateTime, Utc};

/// The earliest time that a `timestamptz` holds, 4714-11-24 BC at midnight UTC,
/// in microseconds since the Unix epoch. PostgreSQL's latest time is beyond
/// chrono's, so only this end needs a check.
const EARLIEST_MICROS: i64 = -210_866_803_200_000_000;

/// A time that cannot be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is outside the range of times that can be stored")]
pub struct TimeOutOfRange(DateTime<Utc>);

/// A time exactly as PostgreSQL stores it: whole microseconds, with a leap second
/// folded into the next second. It comes back from the database equal, so a
/// retry compares equal, and holding one proves that the database accepts it
/// (ADR-0022).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusinessTime(DateTime<Utc>);

impl BusinessTime {
    /// Converts `time` to what PostgreSQL would store: anything finer than a
    /// microsecond is dropped, downwards in every era, and a leap second such
    /// as `23:59:60.5` becomes `00:00:00.5` of the next day.
    ///
    /// # Errors
    ///
    /// [`TimeOutOfRange`] for a time before 4714-11-24 BC, or a leap second in
    /// the last second that chrono can represent.
    pub fn new(time: DateTime<Utc>) -> Result<Self, TimeOutOfRange> {
        let micros = time.timestamp_micros();
        DateTime::from_timestamp_micros(micros)
            .filter(|_| micros >= EARLIEST_MICROS)
            .map(Self)
            .ok_or(TimeOutOfRange(time))
    }

    /// The time, to bind in SQL.
    pub fn get(self) -> DateTime<Utc> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sqlx::types::chrono::NaiveDate;

    fn time(text: &str) -> DateTime<Utc> {
        text.parse().expect("a valid time")
    }

    fn at(
        year: i32,
        month: u32,
        day: u32,
        (h, m, s, nanos): (u32, u32, u32, u32),
    ) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|date| date.and_hms_nano_opt(h, m, s, nanos))
            .expect("a valid time")
            .and_utc()
    }

    fn built(value: DateTime<Utc>) -> DateTime<Utc> {
        BusinessTime::new(value).expect("in range").get()
    }

    #[test]
    fn anything_finer_than_a_microsecond_is_dropped_downwards() {
        // Before 2000 too, where sqlx would round the other way: it counts from
        // 2000 and rounds towards it. Before 1970 too, where the Unix count is
        // negative.
        for (given, expected) in [
            (
                "2026-01-01T00:00:00.123456789Z",
                "2026-01-01T00:00:00.123456Z",
            ),
            ("2026-01-01T00:00:00.123456Z", "2026-01-01T00:00:00.123456Z"),
            (
                "1999-12-31T23:59:59.123456789Z",
                "1999-12-31T23:59:59.123456Z",
            ),
            (
                "1960-01-01T00:00:00.500000900Z",
                "1960-01-01T00:00:00.500000Z",
            ),
        ] {
            assert_eq!(built(time(given)), time(expected), "{given}");
        }
    }

    #[test]
    fn a_leap_second_is_folded_into_the_next_second() {
        // As PostgreSQL does: left as it is, a retry would never compare equal.
        for (given, expected) in [
            ("2016-12-31T23:59:60Z", "2017-01-01T00:00:00Z"),
            ("2016-12-31T23:59:60.5Z", "2017-01-01T00:00:00.5Z"),
            (
                "2016-12-31T23:59:60.123456789Z",
                "2017-01-01T00:00:00.123456Z",
            ),
        ] {
            assert_eq!(built(time(given)), time(expected), "{given}");
        }
    }

    #[test]
    fn only_times_that_can_be_stored_are_accepted() {
        let earliest = at(-4713, 11, 24, (0, 0, 0, 0));
        assert_eq!(earliest.timestamp_micros(), EARLIEST_MICROS);
        assert_eq!(built(earliest), earliest);
        let last = DateTime::<Utc>::MAX_UTC;
        assert_eq!(built(last).timestamp_micros(), last.timestamp_micros());
        // A microsecond early, a nanosecond early, chrono's earliest, and a leap
        // second that folds beyond chrono's latest.
        for too_early in [
            at(-4713, 11, 23, (23, 59, 59, 999_999_000)),
            at(-4713, 11, 23, (23, 59, 59, 999_999_999)),
            DateTime::<Utc>::MIN_UTC,
            at(262_142, 12, 31, (23, 59, 59, 1_500_000_000)),
        ] {
            assert_eq!(
                BusinessTime::new(too_early),
                Err(TimeOutOfRange(too_early)),
                "{too_early:?}"
            );
        }
    }

    #[test]
    fn the_error_names_the_time() {
        let early = at(-4713, 11, 23, (0, 0, 0, 0));
        assert_eq!(
            TimeOutOfRange(early).to_string(),
            "-4713-11-23 00:00:00 UTC is outside the range of times that can be stored"
        );
    }

    /// Any time chrono holds, with a leap second among the fractions: nanoseconds
    /// from 1e9 are valid only in a second that could be the 59th.
    fn times() -> impl Strategy<Value = DateTime<Utc>> {
        let seconds = DateTime::<Utc>::MIN_UTC.timestamp()..=DateTime::<Utc>::MAX_UTC.timestamp();
        (seconds, 0_u32..2_000_000_000).prop_filter_map("a valid time", |(seconds, nanos)| {
            DateTime::from_timestamp(seconds, nanos)
        })
    }

    proptest! {
        #[test]
        fn a_built_time_is_whole_microseconds_and_stays_as_it_is(value in times()) {
            if let Ok(first) = BusinessTime::new(value) {
                let nanos = first.get().timestamp_subsec_nanos();
                prop_assert_eq!(nanos % 1_000, 0);
                prop_assert!(nanos < 1_000_000_000, "a leap second survived: {:?}", first);
                prop_assert_eq!(BusinessTime::new(first.get()), Ok(first));
            }
        }

        #[test]
        fn a_time_is_rejected_exactly_below_the_earliest(value in times()) {
            prop_assume!(value.timestamp_subsec_nanos() < 1_000_000_000);
            let in_range = value.timestamp_micros() >= EARLIEST_MICROS;
            prop_assert_eq!(BusinessTime::new(value).is_ok(), in_range, "{:?}", value);
        }
    }
}
