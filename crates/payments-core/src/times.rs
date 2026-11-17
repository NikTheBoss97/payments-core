// Author: Nicola Antonello

//! Business times: when something happened in the world, as the caller reports
//! it (ADR-0022). The library never reads a clock for one. The type is always
//! the one that sqlx encodes, because it comes through sqlx's own re-export.

use sqlx::types::time::OffsetDateTime;

/// The earliest time that RFC 3339 writes, 0000-01-01T00:00:00Z, in microseconds
/// since the Unix epoch (ADR-0059).
const EARLIEST_MICROS: i64 = -62_167_219_200_000_000;

/// The latest, 9999-12-31T23:59:59.999999Z.
const LATEST_MICROS: i64 = 253_402_300_799_999_999;

/// A time that cannot be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is outside the range of times that can be stored")]
pub struct TimeOutOfRange(OffsetDateTime);

/// A time exactly as PostgreSQL stores it, in the years 0000 to 9999 that RFC 3339
/// writes: whole microseconds, in UTC. It comes back from the database equal, so a
/// retry compares equal, and holding one proves that the database accepts it
/// (ADR-0022, ADR-0059).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusinessTime(OffsetDateTime);

impl BusinessTime {
    /// Converts `time` to what PostgreSQL would store: anything finer than a
    /// microsecond is dropped, downwards in every era, and its offset becomes UTC.
    ///
    /// # Errors
    ///
    /// [`TimeOutOfRange`] for a time outside the years 0000 to 9999 in UTC.
    pub fn new(time: OffsetDateTime) -> Result<Self, TimeOutOfRange> {
        Some(unix_micros(time))
            .filter(|micros| (EARLIEST_MICROS..=LATEST_MICROS).contains(micros))
            .and_then(from_unix_micros)
            .map(Self)
            .ok_or(TimeOutOfRange(time))
    }

    /// The time, to bind in SQL.
    pub fn get(self) -> OffsetDateTime {
        self.0
    }

    /// The time as the API writes it: RFC 3339 in UTC with `Z`, with the fractional digits
    /// that it needs (ADR-0045). Written here, because time's own RFC 3339 drops every
    /// trailing zero, and stored answers hold this form (ADR-0059).
    #[must_use]
    pub fn written(self) -> String {
        let time = self.0;
        // No digits for a whole second, and otherwise 3 or 6, as many as it takes.
        let micros = time.microsecond();
        let fraction = match (micros, micros % 1_000) {
            (0, _) => String::new(),
            (_, 0) => format!(".{:03}", micros / 1_000),
            _ => format!(".{micros:06}"),
        };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}Z",
            time.year(),
            u8::from(time.month()),
            time.day(),
            time.hour(),
            time.minute(),
            time.second()
        )
    }
}

/// A time's whole microseconds since the Unix epoch, rounded down in every era.
#[must_use]
pub fn unix_micros(time: OffsetDateTime) -> i64 {
    i64::try_from(time.unix_timestamp_nanos().div_euclid(1_000))
        .expect("the years that time holds are an i64 of microseconds")
}

/// The time `micros` microseconds after the Unix epoch, in UTC, if time can hold it.
#[must_use]
pub fn from_unix_micros(micros: i64) -> Option<OffsetDateTime> {
    i128::from(micros)
        .checked_mul(1_000)
        .and_then(|nanos| OffsetDateTime::from_unix_timestamp_nanos(nanos).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use sqlx::types::time::{Date, UtcOffset};
    use time::Month;
    use time::format_description::well_known::Rfc3339;

    fn time(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).expect("a valid time")
    }

    fn at(year: i32, month: u8, day: u8, (h, m, s, nanos): (u8, u8, u8, u32)) -> OffsetDateTime {
        Month::try_from(month)
            .and_then(|month| Date::from_calendar_date(year, month, day))
            .and_then(|date| date.with_hms_nano(h, m, s, nanos))
            .expect("a valid time")
            .assume_utc()
    }

    /// `time` with its offset set `seconds` behind UTC, the same wall clock: a later instant.
    fn behind(time: OffsetDateTime, seconds: i32) -> OffsetDateTime {
        time.replace_offset(UtcOffset::from_whole_seconds(-seconds).expect("an offset"))
    }

    fn built(value: OffsetDateTime) -> OffsetDateTime {
        BusinessTime::new(value).expect("in range").get()
    }

    #[test]
    fn a_time_is_written_in_utc_with_the_digits_that_it_needs() {
        let written_at = |micros: i64| {
            let time = from_unix_micros(micros).expect("a time in range");
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
    fn a_time_in_another_offset_is_written_in_utc() {
        let noon = at(2030, 1, 1, (12, 0, 0, 0));
        assert_eq!(
            BusinessTime::new(behind(noon, 3_600))
                .expect("in range")
                .written(),
            "2030-01-01T13:00:00Z"
        );
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
    fn only_the_years_that_rfc_3339_writes_are_accepted() {
        let earliest = at(0, 1, 1, (0, 0, 0, 0));
        assert_eq!(unix_micros(earliest), EARLIEST_MICROS);
        assert_eq!(built(earliest), earliest);
        let latest = at(9999, 12, 31, (23, 59, 59, 999_999_000));
        assert_eq!(unix_micros(latest), LATEST_MICROS);
        assert_eq!(built(latest), latest);
        assert_eq!(built(at(9999, 12, 31, (23, 59, 59, 999_999_999))), latest);
        // A microsecond early, a nanosecond early, time's earliest, and two that an
        // offset takes past 9999 in UTC, the first a microsecond late.
        for out_of_range in [
            at(-1, 12, 31, (23, 59, 59, 999_999_000)),
            at(-1, 12, 31, (23, 59, 59, 999_999_999)),
            at(-9999, 1, 1, (0, 0, 0, 0)),
            behind(at(9999, 12, 31, (23, 59, 59, 0)), 1),
            behind(at(9999, 12, 31, (23, 0, 0, 0)), 3_600),
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
            "-4713-11-23 0:00:00.0 +00:00:00 is outside the range of times that can be stored"
        );
    }

    /// Times from the year -9999 to 9999, the range that time holds, about half of which
    /// can be built.
    fn times() -> impl Strategy<Value = OffsetDateTime> {
        let seconds = at(-9999, 1, 1, (0, 0, 0, 0)).unix_timestamp()
            ..=at(9999, 12, 31, (23, 59, 59, 0)).unix_timestamp();
        (seconds, 0_u32..1_000_000_000).prop_filter_map("a valid time", |(seconds, nanos)| {
            OffsetDateTime::from_unix_timestamp(seconds)
                .and_then(|time| time.replace_nanosecond(nanos))
                .ok()
        })
    }

    proptest! {
        #[test]
        fn a_built_time_is_whole_microseconds_and_stays_as_it_is(value in times()) {
            if let Ok(first) = BusinessTime::new(value) {
                prop_assert_eq!(first.get().nanosecond() % 1_000, 0);
                prop_assert_eq!(BusinessTime::new(first.get()), Ok(first));
            }
        }

        #[test]
        fn a_time_is_accepted_exactly_within_the_years_0000_to_9999(value in times()) {
            let range = at(0, 1, 1, (0, 0, 0, 0))..=at(9999, 12, 31, (23, 59, 59, 999_999_999));
            prop_assert_eq!(BusinessTime::new(value).is_ok(), range.contains(&value), "{:?}", value);
        }
    }
}
