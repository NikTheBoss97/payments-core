// Author: Nicola Antonello

//! What ADR-0022 and ADR-0059 promise of a business time, against PostgreSQL: it
//! comes back from the database equal, so a retry compares equal, and the database
//! holds every time that can be built, from the year 0000 to 9999.

mod common;

use std::error::Error;

use common::runtime_pool;
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, NaiveDate, Utc};

type TestResult = Result<(), Box<dyn Error>>;

/// Sends a time to PostgreSQL as a `timestamptz` and reads it back.
async fn round_trip(app: &PgPool, time: DateTime<Utc>) -> sqlx::Result<DateTime<Utc>> {
    sqlx::query_scalar!(r#"SELECT $1::timestamptz AS "time!""#, time)
        .fetch_one(app)
        .await
}

/// SplitMix64, as in the concurrency tests, so that a failure repeats exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// `year`-`month`-`day` at `h:m:s` and `nanos`, in UTC.
fn at(year: i32, month: u32, day: u32, (h, m, s, nanos): (u32, u32, u32, u32)) -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|date| date.and_hms_nano_opt(h, m, s, nanos))
        .expect("a valid time")
        .and_utc()
}

#[sqlx::test]
async fn every_time_that_can_be_built_comes_back_equal(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    // From a century before the earliest time that can be built to the latest second.
    let lowest = i128::from(at(-100, 1, 1, (0, 0, 0, 0)).timestamp());
    let highest = i128::from(at(9999, 12, 31, (23, 59, 59, 0)).timestamp());
    let span = highest - lowest + 1;
    // The awkward ones by name, then a sample of the range.
    let mut times: Vec<DateTime<Utc>> = [
        "2026-01-01T00:00:00.123456789Z",
        "1999-12-31T23:59:59.123456789Z",
        "2000-01-01T00:00:00Z",
        "1970-01-01T00:00:00Z",
        "1960-01-01T00:00:00.500000900Z",
        "2016-12-31T23:59:60Z",
        "2016-12-31T23:59:60.5Z",
        "2016-12-31T23:59:60.123456789Z",
        "0000-01-01T00:00:00Z",
        "9999-12-31T23:59:59.999999999Z",
    ]
    .into_iter()
    .map(|text| text.parse().expect("a valid time"))
    .collect();
    let mut rng = Rng(0x5EED);
    for _ in 0..1000 {
        let seconds = lowest + i128::from(rng.next()) % span;
        let nanos = u32::try_from(rng.next() % 1_000_000_000)?;
        times.extend(DateTime::from_timestamp(i64::try_from(seconds)?, nanos));
    }
    let mut built = 0;
    for time in times {
        if let Ok(business) = BusinessTime::new(time) {
            built += 1;
            assert_eq!(
                round_trip(&app, business.get()).await?,
                business.get(),
                "{time:?}"
            );
        }
    }
    // One century in a hundred lies before the range, so about 99% of the sample
    // should be built. Far fewer would mean that this tests little.
    assert!(built > 950, "only {built} of the times were in range");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_bounds_are_rfc_3339s_years(pool: PgPool) -> TestResult {
    // The earliest and the latest time that can be built come back equal. A
    // microsecond beyond either cannot be built, though PostgreSQL holds it, and the
    // columns refuse it too (payment_records.rs).
    let app = runtime_pool(&pool).await?;
    for (bound, beyond) in [
        (
            at(0, 1, 1, (0, 0, 0, 0)),
            at(-1, 12, 31, (23, 59, 59, 999_999_000)),
        ),
        (
            at(9999, 12, 31, (23, 59, 59, 999_999_000)),
            at(10_000, 1, 1, (0, 0, 0, 0)),
        ),
    ] {
        let built = BusinessTime::new(bound)?.get();
        assert_eq!(built, bound);
        assert_eq!(round_trip(&app, built).await?, bound);
        assert!(BusinessTime::new(beyond).is_err(), "{beyond:?}");
        assert_eq!(round_trip(&app, beyond).await?, beyond);
    }
    app.close().await;
    Ok(())
}
