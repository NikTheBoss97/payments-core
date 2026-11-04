// Author: Nicola Antonello

//! Idempotency keys (ADR-0026).
//!
//! A request's final answer commits in the same transaction as its effect, under
//! its merchant and key. So no record means that no attempt left an effect, and a
//! record means that its answer is final: a retry looks the key up first, and an
//! attempt that loses the race for the key rolls back whole. Each attempt has fresh
//! IDs, because the record decides whether anything runs.

use std::fmt;

use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

use crate::ids::MerchantId;
use crate::lifecycle::Refused;
use crate::payments::Recorded;
use crate::tenancy::{Scope, Tenant};

/// Why a key was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an idempotency key is 1 to 255 visible ASCII characters, without '\"' or '\\'")]
pub struct InvalidKey;

/// A client's idempotency key: 1 to 255 visible ASCII characters without `"` or
/// `\`, so that its Structured Field string needs no escapes. The column's `CHECK`
/// stands behind this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// # Errors
    ///
    /// [`InvalidKey`] for anything outside that format.
    pub fn new(key: &str) -> Result<Self, InvalidKey> {
        // Every character it allows is ASCII, so it is checked on bytes, and a byte
        // length of 255 is 255 characters.
        let allowed = |byte| matches!(byte, b'!' | b'#'..=b'[' | b']'..=b'~');
        if (1..=255).contains(&key.len()) && key.bytes().all(allowed) {
            Ok(Self(key.to_owned()))
        } else {
            Err(InvalidKey)
        }
    }
}

/// What a key was first used for: the SHA-256 of the request's method, path and raw
/// body. A reformatted body counts as a different request, which can raise a false
/// alarm but can never merge two requests (ADR-0026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// A newline ends the method and the path, neither of which can hold one, so no
    /// two requests can run together into the same input.
    #[must_use]
    pub fn of(method: &str, path: &str, body: &[u8]) -> Self {
        let digest = Sha256::new()
            .chain_update(method)
            .chain_update(b"\n")
            .chain_update(path)
            .chain_update(b"\n")
            .chain_update(body)
            .finalize();
        Self(digest.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A request's final answer, as it is stored and replayed: the same status and the
/// same bytes. Only an answer that cannot change is final, so a `CHECK` refuses a
/// status outside 200 to 499, and that refusal takes the effect down with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: i16,
    pub body: String,
}

/// What a command under a key stores with its effect: which key, what it was used
/// for, and how to render the answer that the table of moves gives (ADR-0026).
pub struct Seal<'a> {
    pub key: &'a IdempotencyKey,
    pub fingerprint: Fingerprint,
    pub answer: &'a (dyn Fn(Result<&Recorded, &Refused>) -> Answer + Sync),
}

impl fmt::Debug for Seal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Seal")
            .field("key", self.key)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Seal<'_> {
    /// Stores the answer, inside the transaction that decided it.
    pub(crate) async fn store_in(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        merchant: MerchantId,
        decided: Result<&Recorded, &Refused>,
    ) -> sqlx::Result<()> {
        let answer = (self.answer)(decided);
        sqlx::query!(
            "INSERT INTO idempotency_records (merchant_id, key, fingerprint, status, body) \
             VALUES ($1, $2, $3, $4, $5)",
            merchant.get(),
            self.key.0,
            self.fingerprint.as_bytes(),
            answer.status,
            answer.body
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}

/// What a key holds for this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recall {
    /// No attempt has committed under this key: run the request, with fresh IDs.
    Unused,
    /// This request's final answer: replay it, and run nothing.
    Answered(Answer),
    /// The key holds the answer to a different request.
    Reused,
}

/// Why a key could not be looked up. Looking it up again is safe.
#[derive(Debug, thiserror::Error)]
#[error("an idempotency key could not be looked up")]
pub struct RecallError(#[source] sqlx::Error);

/// Looks a key up. A row is never changed, so no lock is needed: an answer found is
/// final, and none means that no attempt has committed yet.
///
/// # Errors
///
/// [`RecallError`] when the key cannot be looked up.
pub async fn recall(
    tenant: Tenant<'_>,
    key: &IdempotencyKey,
    fingerprint: Fingerprint,
) -> Result<Recall, RecallError> {
    let mut tx = Scope::from(tenant).begin().await.map_err(RecallError)?;
    // The merchant is in the query as well as in the policy (ADR-0025).
    let row = sqlx::query!(
        "SELECT fingerprint, status, body FROM idempotency_records \
         WHERE merchant_id = $1 AND key = $2",
        tenant.merchant().get(),
        key.0
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(RecallError)?;
    Ok(match row {
        None => Recall::Unused,
        Some(row) if row.fingerprint == fingerprint.as_bytes() => Recall::Answered(Answer {
            status: row.status,
            body: row.body,
        }),
        Some(_) => Recall::Reused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_visible_ascii_without_quotes_or_backslashes() {
        let at_the_limit = "k".repeat(255);
        for key in [
            "!",
            "#",
            "[",
            "]",
            "~",
            "8e03978e-40d5-43e8-bc93-6894a57f9324",
            at_the_limit.as_str(),
        ] {
            assert!(IdempotencyKey::new(key).is_ok(), "{key:?}");
        }
        let over_the_limit = "k".repeat(256);
        for key in [
            "",
            " ",
            "\"",
            "\\",
            "\u{7f}",
            "a b",
            "a\nb",
            "é",
            over_the_limit.as_str(),
        ] {
            assert_eq!(IdempotencyKey::new(key), Err(InvalidKey), "{key:?}");
        }
    }

    fn hex(fingerprint: Fingerprint) -> String {
        fingerprint
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn a_fingerprint_is_the_sha_256_of_the_method_path_and_raw_body() {
        // Computed with Python's hashlib, and the first with PostgreSQL's sha256 too,
        // not with this code. The last shows that a reformatted body differs.
        for (method, path, body, expected) in [
            (
                "POST",
                "/payments",
                &b"{}"[..],
                "9bf8aaf47e01d8dec3f12e1d8e22cc45d3cf779335d1af690957f4a24e2e9faf",
            ),
            (
                "POST",
                "/payments/019b76da-a800-7000-8000-000000000001/void",
                &b""[..],
                "3cb7794fb8342dc56e9f763830f5f39e0482b80721867a260b10b1d771b00ef5",
            ),
            (
                "POST",
                "/payments",
                &b"{ }"[..],
                "bb3d904d6099972c777bb113ec7b0fa8ab5c28c4683ac4ed3810a03480e3cd1a",
            ),
        ] {
            assert_eq!(hex(Fingerprint::of(method, path, body)), expected);
        }
    }
}
