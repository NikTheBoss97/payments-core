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

/// SQLSTATE raised by the append-only trigger (ADR-0004).
pub const APPEND_ONLY: &str = "LG001";
/// SQLSTATE for a violated CHECK constraint, including a domain's CHECK.
pub const CHECK_VIOLATION: &str = "23514";
/// SQLSTATE for a missing privilege.
pub const INSUFFICIENT_PRIVILEGE: &str = "42501";

/// The SQLSTATE of a database error, or a panic if the error is not one.
pub fn sqlstate(err: sqlx::Error) -> String {
    err.as_database_error()
        .and_then(|e| e.code())
        .map(|code| code.into_owned())
        .unwrap_or_else(|| panic!("expected a database error, got {err:?}"))
}
