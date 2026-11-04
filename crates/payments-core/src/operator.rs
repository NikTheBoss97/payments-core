// Author: Nicola Antonello

//! What only the operator does (ADR-0024).
//!
//! Each command runs on the system role's pool (ADR-0025) and stores its final answer
//! under one of the operator's idempotency keys, in the transaction that decided it
//! (ADR-0026). Each attempt of a request has fresh IDs, so a request looks its key up
//! first, and an attempt that loses the race for the key rolls back whole.
//!
//! No error converts from `sqlx::Error` with `#[from]`, so no `?` can skip the
//! classifier. Never wrap a command in a client-side timeout or `select!`: a dropped
//! `COMMIT` is a lost one (ADR-0014).

use sqlx::{PgPool, Postgres, Transaction};

use crate::credentials::SecretDigest;
use crate::db::{CommitError, code_and_constraint, commit};
use crate::idempotency::OperatorSeal;
use crate::ids::{ApiKeyId, MerchantId};
use crate::money::BasisPoints;
use crate::payments::{Failure, classify};

/// Why an operator's command was not applied. Only [`OperatorError::NotConfirmed`]
/// may have stored it.
#[derive(Debug, thiserror::Error)]
pub enum OperatorError {
    /// What the request names does not exist: no merchant to issue a key to, or no key
    /// to revoke. Nothing is stored, because IDs are made by the server, so this cannot
    /// become true later (ADR-0023).
    #[error("no such merchant or API key")]
    NotFound,
    /// The idempotency key already holds an answer, so this attempt rolled back whole.
    /// Look the key up for the answer (ADR-0026).
    #[error("the idempotency key already holds an answer")]
    KeyTaken,
    /// The database caught what the domain types should have prevented, or refused the
    /// statement (ADR-0025): a bug, never retried.
    #[error("the database rejected the operator's request as invalid")]
    Invariant(#[source] sqlx::Error),
    /// The command may or may not be stored. Look the key up, and run the request with
    /// fresh IDs if it is unused (ADR-0026).
    #[error("the operator's request may or may not be stored: look its key up")]
    NotConfirmed(#[source] sqlx::Error),
}

/// Which statement of a command failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The command's effect, and whatever came before it.
    Effect,
    /// The insert of its answer, under the key.
    Answer,
}

/// What a failed statement means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    NotFound,
    KeyTaken,
    Invariant,
    NotConfirmed,
}

/// A pure function of the stage and the SQLSTATE, on top of the one classifier
/// (ADR-0019). A foreign key on the effect names what the request points at, and the
/// answer has none. A duplicate on the answer can only be the key, because that is its
/// own statement. A duplicate on the effect breaks a rule, because each attempt has
/// fresh IDs, and it is never the outcome that a retry would settle.
fn read_failure(stage: Stage, code: Option<&str>) -> Reading {
    if stage == Stage::Effect && code == Some("23503") {
        return Reading::NotFound;
    }
    match (stage, classify(code)) {
        (Stage::Answer, Failure::Duplicate) => Reading::KeyTaken,
        (Stage::Effect, Failure::Duplicate) | (_, Failure::Invariant) => Reading::Invariant,
        (_, Failure::NotConfirmed) => Reading::NotConfirmed,
    }
}

impl OperatorError {
    fn at(stage: Stage, source: sqlx::Error) -> Self {
        let (code, _) = code_and_constraint(&source);
        match read_failure(stage, code.as_deref()) {
            Reading::NotFound => Self::NotFound,
            Reading::KeyTaken => Self::KeyTaken,
            Reading::Invariant => Self::Invariant(source),
            Reading::NotConfirmed => Self::NotConfirmed(source),
        }
    }
}

/// Registers a merchant at the fee rate that the operator set, under one of the
/// operator's keys.
///
/// # Errors
///
/// See [`OperatorError`].
pub async fn register_merchant_sealed(
    pool: &PgPool,
    seal: &OperatorSeal<'_>,
    id: MerchantId,
    fee_rate: BasisPoints,
) -> Result<(), OperatorError> {
    let effect = |source| OperatorError::at(Stage::Effect, source);
    let mut tx = pool.begin().await.map_err(effect)?;
    sqlx::query!(
        "INSERT INTO merchants (id, fee_bps) VALUES ($1, $2)",
        id.get(),
        i32::from(fee_rate.get())
    )
    .execute(&mut *tx)
    .await
    .map_err(effect)?;
    finish(tx, seal).await
}

/// Issues an API key to a merchant, under one of the operator's keys. Only the digest
/// is stored, so the caller holds the only copy of the secret (ADR-0024).
///
/// # Errors
///
/// See [`OperatorError`].
pub async fn issue_api_key(
    pool: &PgPool,
    seal: &OperatorSeal<'_>,
    id: ApiKeyId,
    merchant: MerchantId,
    digest: &SecretDigest,
) -> Result<(), OperatorError> {
    let effect = |source| OperatorError::at(Stage::Effect, source);
    let mut tx = pool.begin().await.map_err(effect)?;
    sqlx::query!(
        "INSERT INTO api_keys (id, merchant_id, digest) VALUES ($1, $2, $3)",
        id.get(),
        merchant.get(),
        digest.as_bytes()
    )
    .execute(&mut *tx)
    .await
    .map_err(effect)?;
    finish(tx, seal).await
}

/// Revokes an API key, under one of the operator's keys. The key stops working on its
/// next request (ADR-0024). Revoking it again changes nothing and answers too: what the
/// request asks for already holds.
///
/// # Errors
///
/// See [`OperatorError`].
pub async fn revoke_api_key(
    pool: &PgPool,
    seal: &OperatorSeal<'_>,
    id: ApiKeyId,
) -> Result<(), OperatorError> {
    let effect = |source| OperatorError::at(Stage::Effect, source);
    let mut tx = pool.begin().await.map_err(effect)?;
    sqlx::query!(
        "INSERT INTO api_key_revocations (api_key_id) VALUES ($1) \
         ON CONFLICT (api_key_id) DO NOTHING",
        id.get()
    )
    .execute(&mut *tx)
    .await
    .map_err(effect)?;
    finish(tx, seal).await
}

/// Stores the answer and commits, so that the effect and its answer land together or
/// not at all. A failed statement drops the transaction, which queues the ROLLBACK;
/// committing it would report success (ADR-0014).
async fn finish(
    mut tx: Transaction<'_, Postgres>,
    seal: &OperatorSeal<'_>,
) -> Result<(), OperatorError> {
    seal.store_in(&mut tx)
        .await
        .map_err(|source| OperatorError::at(Stage::Answer, source))?;
    commit(tx).await.map_err(|err| match err {
        CommitError::Rejected(source) => OperatorError::Invariant(source),
        CommitError::Unknown(source) => OperatorError::NotConfirmed(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_statement_is_read_by_its_stage_and_its_code() {
        use Reading::{Invariant, KeyTaken, NotConfirmed, NotFound};
        for (stage, code, expected) in [
            // A foreign key on the effect names what the request points at. The answer
            // has none, so there it breaks a rule.
            (Stage::Effect, Some("23503"), NotFound),
            (Stage::Answer, Some("23503"), Invariant),
            // A duplicate depends on the statement that met it.
            (Stage::Answer, Some("23505"), KeyTaken),
            (Stage::Effect, Some("23505"), Invariant),
            // A rule broken, or a statement refused, whichever statement it was.
            (Stage::Answer, Some("23514"), Invariant),
            (Stage::Effect, Some("23514"), Invariant),
            (Stage::Answer, Some("42501"), Invariant),
            (Stage::Effect, Some("42501"), Invariant),
            (Stage::Effect, Some("LG001"), Invariant),
            // A lock timeout, a terminate, a lost connection.
            (Stage::Answer, Some("55P03"), NotConfirmed),
            (Stage::Effect, Some("55P03"), NotConfirmed),
            (Stage::Answer, Some("57P01"), NotConfirmed),
            (Stage::Effect, Some("57P01"), NotConfirmed),
            (Stage::Answer, None, NotConfirmed),
            (Stage::Effect, None, NotConfirmed),
        ] {
            assert_eq!(read_failure(stage, code), expected, "{stage:?} {code:?}");
        }
    }
}
