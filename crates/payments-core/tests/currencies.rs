// Author: Nicola Antonello

//! Database tests for the `currencies` reference table (ADR-0003, ADR-0004).
//!
//! `#[sqlx::test]` gives each test its own database with the migrations
//! applied, connected as the bootstrap superuser, which owns the tables in
//! development (ADR-0008).

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, INSUFFICIENT_PRIVILEGE, assert_guard, assert_violation, sqlstate,
};
use sqlx::PgPool;

#[sqlx::test]
async fn seeded_currencies_carry_their_iso_decimals(pool: PgPool) -> sqlx::Result<()> {
    let rows = sqlx::query!(
        "SELECT code AS \"code!\", decimals FROM currencies \
         WHERE code IN ('EUR', 'JPY', 'KWD') ORDER BY code"
    )
    .fetch_all(&pool)
    .await?;

    let found: Vec<(String, i16)> = rows.into_iter().map(|r| (r.code, r.decimals)).collect();
    assert_eq!(
        found,
        [("EUR".into(), 2), ("JPY".into(), 0), ("KWD".into(), 3)]
    );
    Ok(())
}

#[sqlx::test]
async fn malformed_codes_are_rejected(pool: PgPool) -> sqlx::Result<()> {
    for code in ["eur", "EU", "EURO", "E1R"] {
        let err = sqlx::query!(
            "INSERT INTO currencies (code, decimals) VALUES ($1, 2)",
            code
        )
        .execute(&pool)
        .await
        .unwrap_err();
        // The domain's CHECK, which every currency column shares.
        assert_violation(err, CHECK_VIOLATION, "currency_code_check");
    }
    Ok(())
}

#[sqlx::test]
async fn decimals_outside_zero_to_four_are_rejected(pool: PgPool) -> sqlx::Result<()> {
    for decimals in [-1_i16, 5] {
        let err = sqlx::query!(
            "INSERT INTO currencies (code, decimals) VALUES ('XTS', $1)",
            decimals
        )
        .execute(&pool)
        .await
        .unwrap_err();
        // The name PostgreSQL generates for the column's unnamed CHECK.
        assert_violation(err, CHECK_VIOLATION, "currencies_decimals_check");
    }
    Ok(())
}

#[sqlx::test]
async fn currencies_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    let update = sqlx::query!("UPDATE currencies SET decimals = 3 WHERE code = 'EUR'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "currencies_append_only", "currencies");

    let delete = sqlx::query!("DELETE FROM currencies WHERE code = 'EUR'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "currencies_append_only", "currencies");

    let truncate = sqlx::query!("TRUNCATE currencies")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "currencies_no_truncate",
        "currencies",
    );
    Ok(())
}

#[sqlx::test]
async fn runtime_role_can_read_but_not_write_currencies(pool: PgPool) -> sqlx::Result<()> {
    // SET ROLE changes the role of one connection, so the test keeps one.
    let mut conn = pool.acquire().await?;
    sqlx::query!("SET ROLE payments_app")
        .execute(&mut *conn)
        .await?;

    let eur = sqlx::query_scalar!("SELECT decimals FROM currencies WHERE code = 'EUR'")
        .fetch_one(&mut *conn)
        .await?;
    assert_eq!(eur, 2);

    let insert = sqlx::query!("INSERT INTO currencies (code, decimals) VALUES ('XTS', 2)")
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(insert), INSUFFICIENT_PRIVILEGE);

    let update = sqlx::query!("UPDATE currencies SET decimals = 3 WHERE code = 'EUR'")
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(update), INSUFFICIENT_PRIVILEGE);
    Ok(())
}
