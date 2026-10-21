// Author: Nicola Antonello

//! Posting ledger transactions (ADR-0011, ADR-0015).
//!
//! [`post_in`] writes a ledger transaction inside the caller's database
//! transaction and never begins or commits, so phase 2 can write a payment's
//! state change and its postings in one unit of work (ADR-0011). [`post`]
//! owns the database transaction for phase 1 and is idempotent by
//! transaction ID: calling it again with the same value is how an unknown
//! outcome is settled (ADR-0005, ADR-0014, ADR-0015).
//!
//! The lints are the same as in `money`: this module handles amounts.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::db::{CommitError, code_and_constraint, commit};
use crate::ledger::LedgerTransaction;
use crate::money::Currency;

/// What [`post`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// This call stored the ledger transaction.
    Applied,
    /// An earlier call had stored it, with the same entries.
    AlreadyApplied,
}

/// Why [`post_in`] failed. After any of these the database transaction is
/// aborted: drop it. Committing it anyway makes sqlx report success while
/// PostgreSQL rolls back (ADR-0014).
#[derive(Debug, thiserror::Error)]
pub enum PostInError {
    /// The ID exists: `23505` on either unique key of `ledger_transactions`,
    /// whichever index PostgreSQL checked first (ADR-0015).
    #[error("ledger transaction {id} already exists")]
    Exists {
        id: Uuid,
        #[source]
        source: sqlx::Error,
    },
    /// `23503` on `ledger_entries_account_fkey`: an entry's account does not
    /// exist, or is kept in another currency. One key reports both.
    #[error("ledger transaction {id}: an entry's account is not an account in {currency}")]
    AccountNotInCurrency {
        id: Uuid,
        currency: Currency,
        #[source]
        source: sqlx::Error,
    },
    /// Any other integrity violation or `LG` code: the database caught what
    /// the domain types should have prevented, a bug (ADR-0013).
    #[error("ledger transaction {id}: the database rejected it as invalid")]
    Invariant {
        id: Uuid,
        #[source]
        source: sqlx::Error,
    },
    /// Anything else, such as a lost connection or a lock timeout.
    #[error("ledger transaction {id} could not be written")]
    Database {
        id: Uuid,
        #[source]
        source: sqlx::Error,
    },
}

/// Why [`post`] failed (ADR-0015). Only [`PostError::NotConfirmed`] may
/// have stored the ledger transaction.
#[derive(Debug, thiserror::Error)]
pub enum PostError {
    /// `23503` on `ledger_entries_account_fkey`: an entry's account does not
    /// exist, or is kept in another currency. Nothing is stored.
    #[error("ledger transaction {id}: an entry's account is not an account in {currency}")]
    AccountNotInCurrency {
        id: Uuid,
        currency: Currency,
        #[source]
        source: sqlx::Error,
    },
    /// The database caught what the domain types should have prevented: a
    /// bug, never retried. Nothing is stored.
    #[error("ledger transaction {id}: the database rejected it as invalid")]
    Invariant {
        id: Uuid,
        #[source]
        source: sqlx::Error,
    },
    /// The ID is stored with other entries: a bug, never retried.
    #[error("ledger transaction {id} is stored with other entries")]
    Mismatch { id: Uuid },
    /// The ledger transaction may or may not be stored. Call [`post`] again
    /// later with the same value, never with new IDs (ADR-0014).
    #[error("ledger transaction {id} may or may not be stored: post it again")]
    NotConfirmed {
        id: Uuid,
        #[source]
        source: sqlx::Error,
    },
}

/// Writes `transaction` inside the caller's database transaction: the header,
/// then every entry in one statement. It never begins or commits (ADR-0011).
///
/// # Errors
///
/// [`PostInError::Exists`] when the ID is stored,
/// [`PostInError::AccountNotInCurrency`] when an entry's account is not an
/// account in the transaction's currency, [`PostInError::Invariant`] for a
/// bug, and [`PostInError::Database`] for anything else. The database
/// transaction is then aborted: drop it.
pub async fn post_in(
    tx: &mut Transaction<'_, Postgres>,
    transaction: &LedgerTransaction,
) -> Result<(), PostInError> {
    let id = transaction.id().get();
    let currency = transaction.currency();
    let entry_count = transaction.entry_count();
    let fail = |stage, source: sqlx::Error| {
        let (code, constraint) = code_and_constraint(&source);
        match classify(stage, code.as_deref(), constraint) {
            Failure::Exists => PostInError::Exists { id, source },
            Failure::AccountNotInCurrency => PostInError::AccountNotInCurrency {
                id,
                currency,
                source,
            },
            Failure::Invariant => PostInError::Invariant { id, source },
            Failure::Database => PostInError::Database { id, source },
        }
    };
    // The header first: the entries' foreign key refers to it (ADR-0004).
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
        id,
        entry_count,
        currency.as_str()
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| fail(Stage::Header, source))?;
    // Every entry in one statement: UNNEST turns the three arrays into rows
    // (ADR-0006). They have one element per entry, so no row is padded.
    let line_nos: Vec<i32> = (1..=entry_count).collect();
    let accounts: Vec<Uuid> = transaction
        .entries()
        .iter()
        .map(|entry| entry.account.get())
        .collect();
    let amounts: Vec<i64> = transaction
        .entries()
        .iter()
        .map(|entry| entry.amount.get())
        .collect();
    sqlx::query!(
        "INSERT INTO ledger_entries \
         (transaction_id, line_no, entry_count, account_id, currency, amount) \
         SELECT $1::uuid, line_no, $2::integer, account_id, $3::text, amount \
           FROM UNNEST($4::integer[], $5::uuid[], $6::bigint[]) \
             AS entry (line_no, account_id, amount)",
        id,
        entry_count,
        currency.as_str(),
        &line_nos,
        &accounts,
        &amounts
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| fail(Stage::Entries, source))?;
    Ok(())
}

/// Posts `transaction` in a database transaction of its own. Calling it again
/// with the same value is safe, and is how [`PostError::NotConfirmed`] is
/// settled: if an earlier call stored it, this one finds it and compares
/// the entries (ADR-0005, ADR-0015).
///
/// Never wrap it in a client-side timeout or `select!` (ADR-0014). Lock
/// waits are bounded by the runtime role's `lock_timeout` (ADR-0005).
///
/// # Errors
///
/// [`PostError::AccountNotInCurrency`], [`PostError::Invariant`] and
/// [`PostError::Mismatch`] when nothing was stored by this call, and
/// [`PostError::NotConfirmed`] when the outcome is unknown.
pub async fn post(pool: &PgPool, transaction: &LedgerTransaction) -> Result<Posted, PostError> {
    let id = transaction.id().get();
    // A failure before COMMIT stored nothing in this call, but an earlier
    // call with the same value may have stored it: still not confirmed.
    let mut tx = pool
        .begin()
        .await
        .map_err(|source| PostError::NotConfirmed { id, source })?;
    if let Err(err) = post_in(&mut tx, transaction).await {
        // The database transaction is aborted. Dropping it queues the
        // ROLLBACK; committing it would report success (ADR-0014).
        drop(tx);
        return match err {
            PostInError::Exists { .. } => compare(pool, transaction).await,
            PostInError::AccountNotInCurrency {
                currency, source, ..
            } => Err(PostError::AccountNotInCurrency {
                id,
                currency,
                source,
            }),
            PostInError::Invariant { source, .. } => Err(PostError::Invariant { id, source }),
            PostInError::Database { source, .. } => Err(PostError::NotConfirmed { id, source }),
        };
    }
    match commit(tx).await {
        Ok(()) => Ok(Posted::Applied),
        Err(CommitError::Rejected(source)) => Err(PostError::Invariant { id, source }),
        Err(CommitError::Unknown(source)) => Err(PostError::NotConfirmed { id, source }),
    }
}

/// Compares the stored ledger transaction with this ID to the intended one
/// (ADR-0005). It is committed: an insert that meets an uncommitted row with
/// the same key waits for that transaction to end, and reports the duplicate
/// only if it committed.
async fn compare(pool: &PgPool, transaction: &LedgerTransaction) -> Result<Posted, PostError> {
    let id = transaction.id().get();
    let rows = sqlx::query!(
        r#"SELECT h.entry_count, h.currency AS "currency!", e.line_no, e.account_id, e.amount
             FROM ledger_transactions h
             JOIN ledger_entries e ON e.transaction_id = h.id
            WHERE h.id = $1
            ORDER BY e.line_no"#,
        id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| PostError::NotConfirmed { id, source })?;
    let stored: Vec<(i32, String, i32, Uuid, i64)> = rows
        .into_iter()
        .map(|row| {
            (
                row.entry_count,
                row.currency,
                row.line_no,
                row.account_id,
                row.amount,
            )
        })
        .collect();
    let currency = transaction.currency().as_str().to_owned();
    let intended: Vec<(i32, String, i32, Uuid, i64)> = transaction
        .entries()
        .iter()
        .zip(1..=transaction.entry_count())
        .map(|(entry, line_no)| {
            (
                transaction.entry_count(),
                currency.clone(),
                line_no,
                entry.account.get(),
                entry.amount.get(),
            )
        })
        .collect();
    if stored == intended {
        Ok(Posted::AlreadyApplied)
    } else {
        Err(PostError::Mismatch { id })
    }
}

/// The statement a database error came from.
#[derive(Debug, Clone, Copy)]
enum Stage {
    Header,
    Entries,
}

/// What a failed statement means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    Exists,
    AccountNotInCurrency,
    Invariant,
    Database,
}

/// The pair (code, constraint) decides, never the message (ADR-0013).
fn classify(stage: Stage, code: Option<&str>, constraint: Option<&str>) -> Failure {
    match (stage, code, constraint) {
        // A retry breaks both unique keys, and PostgreSQL names the one whose
        // index it checks first, in OID order (ADR-0015).
        (
            Stage::Header,
            Some("23505"),
            Some("ledger_transactions_pkey" | "ledger_transactions_id_entry_count_currency_key"),
        ) => Failure::Exists,
        (Stage::Entries, Some("23503"), Some("ledger_entries_account_fkey")) => {
            Failure::AccountNotInCurrency
        }
        (_, Some(code), _) if code.starts_with("23") || code.starts_with("LG") => {
            Failure::Invariant
        }
        _ => Failure::Database,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_failed_statement_is_classified_by_stage_code_and_constraint() {
        use Failure::{AccountNotInCurrency, Database, Exists, Invariant};
        use Stage::{Entries, Header};
        for (stage, code, constraint, expected) in [
            // A duplicate ID, reported under whichever unique key PostgreSQL
            // checked first (ADR-0015).
            (
                Header,
                Some("23505"),
                Some("ledger_transactions_pkey"),
                Exists,
            ),
            (
                Header,
                Some("23505"),
                Some("ledger_transactions_id_entry_count_currency_key"),
                Exists,
            ),
            (
                Entries,
                Some("23503"),
                Some("ledger_entries_account_fkey"),
                AccountNotInCurrency,
            ),
            // Every other integrity violation is ruled out by the types.
            (
                Entries,
                Some("23505"),
                Some("ledger_entries_pkey"),
                Invariant,
            ),
            (
                Entries,
                Some("23503"),
                Some("ledger_entries_header_fkey"),
                Invariant,
            ),
            (
                Entries,
                Some("23514"),
                Some("ledger_entries_amount_check"),
                Invariant,
            ),
            (
                Header,
                Some("23514"),
                Some("ledger_transactions_id_check"),
                Invariant,
            ),
            (
                Header,
                Some("23503"),
                Some("ledger_entries_account_fkey"),
                Invariant,
            ),
            (
                Header,
                Some("LG001"),
                Some("ledger_transactions_append_only"),
                Invariant,
            ),
            // Anything else may work when posted again.
            (Header, Some("55P03"), None, Database),
            (Entries, Some("40P01"), None, Database),
            (Entries, Some("57014"), None, Database),
            (Header, Some("42501"), None, Database),
            (Header, None, None, Database),
        ] {
            assert_eq!(
                classify(stage, code, constraint),
                expected,
                "{stage:?} {code:?} {constraint:?}"
            );
        }
    }

    proptest! {
        #[test]
        fn codes_outside_class_23_and_lg_are_database_failures(
            code in "[0-9A-Z]{5}",
            header in any::<bool>(),
        ) {
            prop_assume!(!code.starts_with("23") && !code.starts_with("LG"));
            let stage = if header { Stage::Header } else { Stage::Entries };
            prop_assert_eq!(classify(stage, Some(&code), Some("any_constraint")), Failure::Database);
        }
    }
}
