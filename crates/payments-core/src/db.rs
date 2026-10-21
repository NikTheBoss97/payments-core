// Author: Nicola Antonello

//! Reading database errors (ADR-0013, ADR-0015).
//!
//! The ledger's functions classify a database error by its SQLSTATE and
//! constraint name, never by its message. Each classifier is a pure function
//! of those two values, tested without a database; this adapter reads them
//! from a `sqlx::Error`.

use std::borrow::Cow;

/// The SQLSTATE and constraint name of a database error, each `None` when
/// PostgreSQL did not send it. An error that did not come from PostgreSQL,
/// such as a broken connection, has neither.
pub(crate) fn code_and_constraint(err: &sqlx::Error) -> (Option<Cow<'_, str>>, Option<&str>) {
    match err.as_database_error() {
        Some(db) => (db.code(), db.constraint()),
        None => (None, None),
    }
}
