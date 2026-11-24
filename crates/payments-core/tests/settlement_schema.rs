// Author: Nicola Antonello

//! Database tests for the tables that booking a settlement batch writes (ADR-0066 to
//! ADR-0068, ADR-0072): the terms agreed with a PSP. The account kinds that
//! reconciliation posts to are tested with the rest of the chart, in `account_kinds.rs`.
//!
//! Rows are written with raw SQL, as the system role, `payments_app`, where it may
//! write them, and as the tables' owner where only migrations may. Each rejected write
//! breaks a single rule. The test asserts the SQLSTATE and, where PostgreSQL reports
//! one, the constraint or trigger that rejects the write; privilege errors carry none.

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_transaction, sqlstate,
};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Terms of `psp` in `currency` from `starts_at`, an RFC 3339 time, written as the
/// tables' owner, which is how a migration writes them.
async fn insert_terms(
    pool: &PgPool,
    psp: &str,
    currency: &str,
    starts_at: &str,
    (fee_bps, fee_fixed, settle_within): (i32, i64, i64),
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO psp_terms (psp, currency, starts_at, fee_bps, fee_fixed, settle_within) \
         VALUES ($1, $2, $3::text::timestamptz, $4, $5, $6)",
        psp,
        currency,
        starts_at,
        fee_bps,
        fee_fixed,
        settle_within
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The mock's terms: 140 basis points and 25 minor units on a capture, settling
/// within two days (ADR-0072).
const MOCK_TERMS: (i32, i64, i64) = (140, 25, 172_800);

#[sqlx::test]
async fn the_mocks_terms_hold_in_every_currency_from_year_zero(pool: PgPool) -> TestResult {
    let rows = sqlx::query!(
        r#"SELECT currency::text AS "currency!", fee_bps, fee_fixed, settle_within,
                  starts_at = '0001-01-01 00:00:00+00 BC'::timestamptz AS "from_year_zero!"
             FROM psp_terms
            WHERE psp = 'mock'
            ORDER BY currency"#
    )
    .fetch_all(&pool)
    .await?;
    let terms: Vec<_> = rows
        .into_iter()
        .map(|r| {
            (
                r.currency,
                (r.fee_bps, r.fee_fixed, r.settle_within),
                r.from_year_zero,
            )
        })
        .collect();
    let expected: Vec<_> = ["EUR", "GBP", "ISK", "JPY", "KWD", "USD"]
        .into_iter()
        .map(|currency| (currency.to_owned(), MOCK_TERMS, true))
        .collect();
    assert_eq!(terms, expected);
    Ok(())
}

#[sqlx::test]
async fn only_migrations_write_the_terms(pool: PgPool) -> TestResult {
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!(
        "INSERT INTO psp_terms (psp, currency, starts_at, fee_bps, fee_fixed, settle_within) \
         VALUES ('mock', 'EUR', '2026-01-01 00:00:00+00', 100, 10, 86400)"
    )
    .execute(&mut *tx)
    .await
    .expect_err("the system role only reads the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

#[sqlx::test]
async fn the_terms_never_change(pool: PgPool) -> TestResult {
    // As the owner, the triggers refuse.
    let err = sqlx::query!("UPDATE psp_terms SET fee_bps = 0")
        .execute(&pool)
        .await
        .expect_err("terms are append-only");
    assert_guard(err, APPEND_ONLY, "psp_terms_append_only", "psp_terms");
    let err = sqlx::query!("DELETE FROM psp_terms")
        .execute(&pool)
        .await
        .expect_err("terms are append-only");
    assert_guard(err, APPEND_ONLY, "psp_terms_append_only", "psp_terms");
    let err = sqlx::query!("TRUNCATE psp_terms")
        .execute(&pool)
        .await
        .expect_err("terms are append-only");
    assert_guard(err, APPEND_ONLY, "psp_terms_no_truncate", "psp_terms");

    // As the system role, the privileges refuse first.
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("UPDATE psp_terms SET fee_bps = 0")
        .execute(&mut *tx)
        .await
        .expect_err("the system role cannot change the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("DELETE FROM psp_terms")
        .execute(&mut *tx)
        .await
        .expect_err("the system role cannot change the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let mut tx = runtime_transaction(&pool).await?;
    let err = sqlx::query!("TRUNCATE psp_terms")
        .execute(&mut *tx)
        .await
        .expect_err("the system role cannot change the terms");
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    Ok(())
}

#[sqlx::test]
async fn the_terms_keep_to_their_bounds(pool: PgPool) -> TestResult {
    let later = "2026-01-01T00:00:00Z";
    for (terms, constraint) in [
        ((-1, 25, 172_800), "psp_terms_fee_bps_check"),
        ((10_001, 25, 172_800), "psp_terms_fee_bps_check"),
        ((140, -1, 172_800), "psp_terms_fee_fixed_check"),
        (
            (140, 1_000_000_000_000_001, 172_800),
            "psp_terms_fee_fixed_check",
        ),
        ((140, 25, 0), "psp_terms_settle_within_check"),
        ((140, 25, 31_622_401), "psp_terms_settle_within_check"),
    ] {
        let err = insert_terms(&pool, "mock", "EUR", later, terms)
            .await
            .expect_err("terms out of bounds");
        assert_violation(err, CHECK_VIOLATION, constraint);
    }
    let err = insert_terms(&pool, "other", "EUR", later, MOCK_TERMS)
        .await
        .expect_err("a PSP that the service does not know");
    assert_violation(err, CHECK_VIOLATION, "psp_terms_psp_check");
    let err = insert_terms(&pool, "mock", "EUR", "10000-01-01T00:00:00Z", MOCK_TERMS)
        .await
        .expect_err("a time past RFC 3339's years");
    assert_violation(err, CHECK_VIOLATION, "psp_terms_starts_at_check");
    let err = insert_terms(&pool, "mock", "XXX", later, MOCK_TERMS)
        .await
        .expect_err("a currency that the service does not know");
    assert_violation(err, FOREIGN_KEY_VIOLATION, "psp_terms_currency_fkey");
    let err = insert_terms(
        &pool,
        "mock",
        "EUR",
        "0001-01-01 00:00:00+00 BC",
        MOCK_TERMS,
    )
    .await
    .expect_err("terms from the same start");
    assert_violation(err, UNIQUE_VIOLATION, "psp_terms_pkey");

    // Within every bound, later terms join the earlier ones.
    insert_terms(&pool, "mock", "EUR", later, (0, 0, 1)).await?;
    insert_terms(
        &pool,
        "mock",
        "EUR",
        "2027-01-01T00:00:00Z",
        (10_000, 1, 31_622_400),
    )
    .await?;
    Ok(())
}
