// Author: Nicola Antonello

//! Committing, and reading database errors (ADR-0013, ADR-0014, ADR-0015).
//!
//! The ledger's functions classify a database error by its SQLSTATE and
//! constraint name, never by its message. Each classifier is a pure function
//! of those two values, tested without a database; [`code_and_constraint`]
//! reads them from a `sqlx::Error`.

use std::borrow::Cow;

use sqlx::{Postgres, Transaction};

/// What a failed `COMMIT` proves (ADR-0014).
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// A deferred check rejected the transaction (`LG002` or `LG003`), so it
    /// rolled back and wrote nothing. The data broke a rule that the types
    /// should have ruled out: a bug, never retried.
    #[error("a deferred check rejected the transaction, which rolled back")]
    Rejected(#[source] sqlx::Error),
    /// Any other failure: the transaction may or may not have committed.
    #[error("the transaction may or may not have committed")]
    Unknown(#[source] sqlx::Error),
}

/// Commits `tx`. Every `COMMIT` the application sends goes through this
/// function, so that every caller reads a failure the same way (ADR-0014).
///
/// Never wrap it in a client-side timeout or `select!`: sqlx cannot cancel
/// the server's work, and a dropped `COMMIT` leaves the outcome unknown
/// without any error to say so.
///
/// # Errors
///
/// [`CommitError::Rejected`] when a deferred check rejected the transaction,
/// and [`CommitError::Unknown`] for any other failure, whether or not it
/// carries a SQLSTATE.
pub async fn commit(tx: Transaction<'_, Postgres>) -> Result<(), CommitError> {
    tx.commit().await.map_err(|err| {
        let (code, _) = code_and_constraint(&err);
        if proves_rollback(code.as_deref()) {
            CommitError::Rejected(err)
        } else {
            CommitError::Unknown(err)
        }
    })
}

/// Whether a `COMMIT` error with this SQLSTATE proves a rollback: only the
/// codes of the ledger's deferred checks do, because PostgreSQL raises them
/// before it writes the commit record. A cancel or terminate signal held
/// during the commit can arrive after it, with any other code (ADR-0014).
fn proves_rollback(code: Option<&str>) -> bool {
    matches!(code, Some("LG002" | "LG003"))
}

/// The SQLSTATE and constraint name of a database error, each `None` when
/// PostgreSQL did not send it. An error that did not come from PostgreSQL,
/// such as a broken connection, has neither.
pub(crate) fn code_and_constraint(err: &sqlx::Error) -> (Option<Cow<'_, str>>, Option<&str>) {
    match err.as_database_error() {
        Some(db) => (db.code(), db.constraint()),
        None => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn only_the_deferred_checks_prove_a_rollback() {
        for (code, rolled_back) in [
            // The commit-time check runs before the commit record (ADR-0014).
            (Some("LG002"), true),
            (Some("LG003"), true),
            // A cancel or terminate held during the commit can arrive after
            // it, so these prove nothing.
            (Some("57014"), false),
            (Some("57P01"), false),
            // Others that do roll back are still read as unknown: one
            // identical retry settles them (ADR-0014).
            (Some("40001"), false),
            (Some("25P03"), false),
            (Some("LG001"), false),
            (Some("23505"), false),
            // No SQLSTATE: the connection broke.
            (None, false),
        ] {
            assert_eq!(proves_rollback(code), rolled_back, "{code:?}");
        }
    }

    proptest! {
        #[test]
        fn every_other_code_leaves_the_outcome_unknown(code in "[0-9A-Z]{5}") {
            let expected = code == "LG002" || code == "LG003";
            prop_assert_eq!(proves_rollback(Some(&code)), expected);
        }
    }
}
