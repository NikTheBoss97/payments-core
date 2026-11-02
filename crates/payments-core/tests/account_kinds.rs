// Author: Nicola Antonello

//! Database tests for the chart of accounts, `account_kinds` (ADR-0020,
//! ADR-0004).
//!
//! `#[sqlx::test]` gives each test its own database with the migrations
//! applied, connected as the bootstrap superuser, which owns the tables in
//! development (ADR-0008). `accounts.rs` tests the accounts' foreign key to
//! this table.

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION, assert_guard,
    assert_violation, runtime_connection, sqlstate,
};
use sqlx::PgPool;

#[sqlx::test]
async fn the_chart_holds_exactly_the_kinds_phase_2_posts_to(pool: PgPool) -> sqlx::Result<()> {
    let rows = sqlx::query!("SELECT kind, normal_side FROM account_kinds ORDER BY kind")
        .fetch_all(&pool)
        .await?;
    let found: Vec<(String, String)> = rows.into_iter().map(|r| (r.kind, r.normal_side)).collect();
    assert_eq!(
        found,
        [
            ("fee_revenue".into(), "credit".into()),
            ("merchant_payable".into(), "credit".into()),
            ("psp_receivable".into(), "debit".into()),
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn a_kind_has_one_normal_side(pool: PgPool) -> sqlx::Result<()> {
    let err = sqlx::query!(
        "INSERT INTO account_kinds (kind, normal_side) VALUES ('fee_revenue', 'debit')"
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "account_kinds_pkey");
    Ok(())
}

#[sqlx::test]
async fn a_bad_seed_fails_instead_of_leaving_an_unusable_row(pool: PgPool) -> sqlx::Result<()> {
    for kind in ["", "Cash", "1cash", "fee-revenue"] {
        let err = sqlx::query!(
            "INSERT INTO account_kinds (kind, normal_side) VALUES ($1, 'debit')",
            kind
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "account_kinds_kind_check");
    }
    for side in ["", "Debit", "dr"] {
        let err = sqlx::query!(
            "INSERT INTO account_kinds (kind, normal_side) VALUES ('cash', $1)",
            side
        )
        .execute(&pool)
        .await
        .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "account_kinds_normal_side_check");
    }
    Ok(())
}

#[sqlx::test]
async fn the_chart_is_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    let update =
        sqlx::query!("UPDATE account_kinds SET normal_side = 'debit' WHERE kind = 'fee_revenue'")
            .execute(&pool)
            .await
            .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "account_kinds_append_only",
        "account_kinds",
    );

    let delete = sqlx::query!("DELETE FROM account_kinds WHERE kind = 'fee_revenue'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "account_kinds_append_only",
        "account_kinds",
    );

    // CASCADE also reaches accounts, whose foreign key refers to the chart:
    // without it, PostgreSQL fails with 0A000 before any trigger runs. The
    // triggers of the named table fire first, which the trigger name proves.
    let truncate = sqlx::query!("TRUNCATE account_kinds CASCADE")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "account_kinds_no_truncate",
        "account_kinds",
    );
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_can_read_but_not_write_the_chart(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;

    let side =
        sqlx::query_scalar!("SELECT normal_side FROM account_kinds WHERE kind = 'fee_revenue'")
            .fetch_one(&mut app)
            .await?;
    assert_eq!(side, "credit");

    let insert =
        sqlx::query!("INSERT INTO account_kinds (kind, normal_side) VALUES ('cash', 'debit')")
            .execute(&mut app)
            .await
            .unwrap_err();
    assert_eq!(sqlstate(insert), INSUFFICIENT_PRIVILEGE);

    let update = sqlx::query!("UPDATE account_kinds SET normal_side = 'debit'")
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(update), INSUFFICIENT_PRIVILEGE);

    let delete = sqlx::query!("DELETE FROM account_kinds")
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(delete), INSUFFICIENT_PRIVILEGE);

    let truncate = sqlx::query!("TRUNCATE account_kinds CASCADE")
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(truncate), INSUFFICIENT_PRIVILEGE);
    Ok(())
}
