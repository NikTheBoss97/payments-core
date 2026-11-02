// Author: Nicola Antonello

//! What ADR-0022 promises of a business time, against PostgreSQL: it comes back
//! from the database equal, so a retry compares equal, and the database accepts
//! every time that can be built. The bounds are a measurement of PostgreSQL's
//! range, so a change in its range or in sqlx's encoding fails here.

mod common;

use std::error::Error;

use common::{runtime_pool, sqlstate};
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

#[sqlx::test]
async fn every_time_that_can_be_built_comes_back_equal(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let lowest = i128::from(DateTime::<Utc>::MIN_UTC.timestamp());
    let highest = i128::from(DateTime::<Utc>::MAX_UTC.timestamp());
    let span = highest - lowest + 1;
    // The awkward ones by name, then a sample of the whole range.
    let mut times: Vec<DateTime<Utc>> = [
        "2026-01-01T00:00:00.123456789Z",
        "1999-12-31T23:59:59.123456789Z",
        "2000-01-01T00:00:00Z",
        "1970-01-01T00:00:00Z",
        "1960-01-01T00:00:00.500000900Z",
        "2016-12-31T23:59:60Z",
        "2016-12-31T23:59:60.5Z",
        "2016-12-31T23:59:60.123456789Z",
    ]
    .into_iter()
    .map(|text| text.parse().expect("a valid time"))
    .collect();
    times.push(DateTime::<Utc>::MAX_UTC);
    times.push(
        NaiveDate::from_ymd_opt(-4713, 11, 24)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid time")
            .and_utc(),
    );
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
    // About half of chrono's range is before PostgreSQL's, so about half of the
    // sample should be built. Far fewer would mean that this tests little.
    assert!(built > 400, "only {built} of the times were in range");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_bound_is_postgresqls_own(pool: PgPool) -> TestResult {
    // The earliest time that can be built is accepted, and one microsecond before
    // it PostgreSQL refuses: the type rejects exactly what the database does.
    let app = runtime_pool(&pool).await?;
    let earliest = NaiveDate::from_ymd_opt(-4713, 11, 24)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .expect("a valid time")
        .and_utc();
    assert_eq!(round_trip(&app, earliest).await?, earliest);
    let before = DateTime::from_timestamp_micros(earliest.timestamp_micros() - 1)
        .ok_or("a time chrono holds")?;
    assert!(BusinessTime::new(before).is_err());
    let err = round_trip(&app, before).await.unwrap_err();
    assert_eq!(sqlstate(err), "22008", "datetime_field_overflow");
    app.close().await;
    Ok(())
}
