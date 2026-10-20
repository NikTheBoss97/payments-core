// Author: Nicola Antonello

//! Helpers shared by the database tests.
//!
//! Cargo compiles each file directly under `tests/` as its own crate, while a
//! module in a subdirectory is not a test target. Each test file that needs
//! these helpers includes this one with `mod common;`, and each uses only
//! some of them.

#![allow(
    dead_code,
    reason = "each test crate uses a different subset of these helpers"
)]

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

/// SQLSTATE raised by the append-only trigger (ADR-0004).
pub const APPEND_ONLY: &str = "LG001";
/// SQLSTATE for a violated CHECK constraint, including a domain's CHECK.
pub const CHECK_VIOLATION: &str = "23514";
/// SQLSTATE for a violated foreign key.
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
/// SQLSTATE for a missing privilege.
pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
/// SQLSTATE for a NULL in a NOT NULL column. Its error names the column,
/// not a constraint.
pub const NOT_NULL_VIOLATION: &str = "23502";
/// SQLSTATE for a duplicate key in a primary key or a unique constraint.
pub const UNIQUE_VIOLATION: &str = "23505";

/// A connection that acts as the runtime role, `payments_app`, which meets
/// the same guards as the application (ADR-0004). `#[sqlx::test]` connects
/// as a superuser, which skips privilege checks. The connection is detached
/// from the pool, so the role cannot leak into a connection the pool hands
/// out later in the same test.
pub async fn runtime_connection(pool: &PgPool) -> sqlx::Result<PgConnection> {
    let mut conn = pool.acquire().await?.detach();
    sqlx::query!("SET ROLE payments_app")
        .execute(&mut conn)
        .await?;
    Ok(conn)
}

/// A database transaction that acts as the runtime role until it ends.
/// `SET LOCAL` lasts only until COMMIT or ROLLBACK, so the connection goes
/// back to the pool with its own role.
pub async fn runtime_transaction(pool: &PgPool) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL ROLE payments_app")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// The SQLSTATE of a database error, or a panic if the error is not one.
pub fn sqlstate(err: sqlx::Error) -> String {
    err.as_database_error()
        .and_then(|e| e.code())
        .map(|code| code.into_owned())
        .unwrap_or_else(|| panic!("expected a database error, got {err:?}"))
}

/// Asserts that `err` carries this SQLSTATE and names this constraint or
/// trigger. Together they prove which guard fired: a code alone is shared
/// by every guard of its kind.
#[track_caller]
pub fn assert_violation(err: sqlx::Error, code: &str, constraint: &str) {
    let Some(db) = err.as_database_error() else {
        panic!("expected a database error, got {err:?}");
    };
    assert_eq!(
        (db.code().as_deref(), db.constraint()),
        (Some(code), Some(constraint)),
        "{db}"
    );
}

/// Asserts that `err` comes from one of the project's own guards: this
/// SQLSTATE, raised by this trigger on this table of the public schema, the
/// fields ADR-0013 requires. Returns the error's DETAIL for the tests that
/// check the numbers it reports.
#[track_caller]
pub fn assert_guard(err: sqlx::Error, code: &str, trigger: &str, table: &str) -> String {
    let Some(db) = err.as_database_error() else {
        panic!("expected a database error, got {err:?}");
    };
    let Some(pg) = db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() else {
        panic!("expected a PostgreSQL error, got {db}");
    };
    assert_eq!(
        (pg.code(), pg.constraint(), pg.table(), pg.schema()),
        (code, Some(trigger), Some(table), Some("public")),
        "{pg}"
    );
    pg.detail().unwrap_or_default().to_owned()
}
