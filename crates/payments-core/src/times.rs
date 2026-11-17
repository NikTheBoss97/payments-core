// Author: Nicola Antonello

//! Business times: when something happened in the world, as the caller reports
//! it (ADR-0022). The library never reads a clock for one. The type is always
//! the one that sqlx encodes, because it comes through sqlx's own re-export.

use sqlx::types::chrono::{DateTime, Utc};

/// The earliest time that RFC 3339 writes, 0000-01-01T00:00:00Z, in microseconds
/// since the Unix epoch (ADR-0059).
const EARLIEST_MICROS: i64 = -62_167_219_200_000_000;

/// The latest, 9999-12-31T23:59:59.999999Z.
const LATEST_MICROS: i64 = 253_402_300_799_999_999;

/// A time that cannot be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is outside the range of times that can be stored")]
pub struct TimeOutOfRange(DateTime<Utc>);

/// A time exactly as PostgreSQL stores it, in the years 0000 to 9999 that RFC 3339
/// writes: whole microseconds, with a leap second folded into the next second. It
/// comes back from the database equal, so a retry compares equal, and holding one
/// proves that the database accepts it (ADR-0022, ADR-0059).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusinessTime(DateTime<Utc>);

impl BusinessTime {
    /// Converts `time` to what PostgreSQL would store: anything finer than a
    /// microsecond is dropped, downwards in every era, and a leap second such
    /// as `23:59:60.5` becomes `00:00:00.5` of the next day.
    ///
    /// # Errors
    ///
    /// [`TimeOutOfRange`] for a time outside the years 0000 to 9999, a leap second
    /// that folds past the last second of 9999 among them.
    pub fn new(time: DateTime<Utc>) -> Result<Self, TimeOutOfRange> {
        let micros = time.timestamp_micros();
        DateTime::from_timestamp_micros(micros)
            .filter(|_| (EARLIEST_MICROS..=LATEST_MICROS).contains(&micros))
            .map(Self)
            .ok_or(TimeOutOfRange(time))
    }

    /// The time, to bind in SQL.
    pub fn get(self) -> DateTime<Utc> {
        self.0
    }

    /// The time as the API writes it: RFC 3339 in UTC with `Z`, with the fractional digits
    /// that it needs (ADR-0045).
    #[must_use]
    pub fn written(self) -> String {
        // `%.f` writes no digits for a whole second, and otherwise 3, 6 or 9, as many
        // as it takes; a business time has 6 at most (ADR-0022).
        self.0.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string()
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
    fn a_time_is_written_in_utc_with_the_digits_that_it_needs() {
        let written_at = |micros: i64| {
            let time = DateTime::from_timestamp_micros(micros).expect("a time in range");
            BusinessTime::new(time)
                .expect("a time that PostgreSQL stores")
                .written()
        };
        // 2030-01-01T00:00:00Z, and 120 ms and 1 us after it.
        assert_eq!(written_at(1_893_456_000_000_000), "2030-01-01T00:00:00Z");
        assert_eq!(
            written_at(1_893_456_000_120_000),
            "2030-01-01T00:00:00.120Z"
        );
        assert_eq!(
            written_at(1_893_456_000_000_001),
            "2030-01-01T00:00:00.000001Z"
        );
        // The first and the last time that can be built, two before 1970, and a date
        // and a time of one digit each.
        assert_eq!(written_at(EARLIEST_MICROS), "0000-01-01T00:00:00Z");
        assert_eq!(written_at(LATEST_MICROS), "9999-12-31T23:59:59.999999Z");
        assert_eq!(written_at(-1), "1969-12-31T23:59:59.999999Z");
        assert_eq!(written_at(-999_000), "1969-12-31T23:59:59.001Z");
        assert_eq!(written_at(1_896_321_906_000_000), "2030-02-03T04:05:06Z");
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
    fn only_the_years_that_rfc_3339_writes_are_accepted() {
        let earliest = at(0, 1, 1, (0, 0, 0, 0));
        assert_eq!(earliest.timestamp_micros(), EARLIEST_MICROS);
        assert_eq!(built(earliest), earliest);
        let latest = at(9999, 12, 31, (23, 59, 59, 999_999_000));
        assert_eq!(latest.timestamp_micros(), LATEST_MICROS);
        assert_eq!(built(latest), latest);
        assert_eq!(built(at(9999, 12, 31, (23, 59, 59, 999_999_999))), latest);
        // A microsecond early, a nanosecond early, chrono's earliest, a microsecond
        // late, chrono's latest, and a leap second that folds beyond the latest.
        for out_of_range in [
            at(-1, 12, 31, (23, 59, 59, 999_999_000)),
            at(-1, 12, 31, (23, 59, 59, 999_999_999)),
            DateTime::<Utc>::MIN_UTC,
            at(10_000, 1, 1, (0, 0, 0, 0)),
            DateTime::<Utc>::MAX_UTC,
            at(9999, 12, 31, (23, 59, 59, 1_500_000_000)),
        ] {
            assert_eq!(
                BusinessTime::new(out_of_range),
                Err(TimeOutOfRange(out_of_range)),
                "{out_of_range:?}"
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

    /// Times from the year -9999 to 9999, about half of which can be built, with a leap
    /// second among the fractions: nanoseconds from 1e9 are valid only in a second that
    /// could be the 59th.
    fn times() -> impl Strategy<Value = DateTime<Utc>> {
        let seconds = at(-9999, 1, 1, (0, 0, 0, 0)).timestamp()
            ..=at(9999, 12, 31, (23, 59, 59, 0)).timestamp();
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
        fn a_time_is_accepted_exactly_within_the_years_0000_to_9999(value in times()) {
            prop_assume!(value.timestamp_subsec_nanos() < 1_000_000_000);
            let in_range = value >= at(0, 1, 1, (0, 0, 0, 0)) && value < at(10_000, 1, 1, (0, 0, 0, 0));
            prop_assert_eq!(BusinessTime::new(value).is_ok(), in_range, "{:?}", value);
        }
    }
}
