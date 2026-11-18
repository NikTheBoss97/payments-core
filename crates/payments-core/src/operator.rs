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

use axum::http::Uri;
use axum::http::uri::{Authority, Scheme};
use sqlx::{PgPool, Postgres, Transaction};

use crate::credentials::SecretDigest;
use crate::db::{CommitError, code_and_constraint, commit};
use crate::idempotency::OperatorSeal;
use crate::ids::{ApiKeyId, EndpointId, MerchantId};
use crate::money::BasisPoints;
use crate::payments::{Failure, classify};

/// Why an operator's command was not applied. Only [`OperatorError::NotConfirmed`]
/// may have stored it.
#[derive(Debug, thiserror::Error)]
pub enum OperatorError {
    /// What the request names does not exist: no merchant to issue a key to or to
    /// register an endpoint for, no key to revoke, or no endpoint to disable. Nothing is
    /// stored, because IDs are made by the server, so this cannot become true later
    /// (ADR-0023).
    #[error("no such merchant, API key or endpoint")]
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

/// An endpoint's URL as ADR-0050 and ADR-0060 take it: absolute and `http`, with a host and
/// any port as a client reads them, without user information or a fragment, and with its
/// scheme in lowercase, as the database's check wants it (ADR-0055).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointUrl(String);

/// Why the server refuses an endpoint's URL (ADR-0060).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UrlRefusal {
    /// The text is not an absolute URL, or its host is empty, or its port is not one that
    /// a client reads: a malformed body.
    #[error("the text is not an absolute URL")]
    NotAbsolute,
    /// An absolute URL that an endpoint cannot have: another scheme, user information or a
    /// fragment.
    #[error("an endpoint cannot have this URL")]
    Unusable,
}

impl EndpointUrl {
    /// Reads an endpoint's URL (ADR-0060).
    ///
    /// # Errors
    ///
    /// See [`UrlRefusal`].
    pub fn parse(text: &str) -> Result<Self, UrlRefusal> {
        let uri: Uri = text.parse().map_err(|_| UrlRefusal::NotAbsolute)?;
        let (Some(scheme), Some(authority)) = (uri.scheme(), uri.authority()) else {
            return Err(UrlRefusal::NotAbsolute);
        };
        if !names_a_host_and_its_port(authority) {
            return Err(UrlRefusal::NotAbsolute);
        }
        // The parser drops a fragment, so the text itself is read for one.
        if *scheme != Scheme::HTTP || authority.as_str().contains('@') || text.contains('#') {
            return Err(UrlRefusal::Unusable);
        }
        // The text starts with its scheme, which has four letters in any case.
        let rest = text.get(4..).expect("the text starts with its scheme");
        Ok(Self(format!("http{rest}")))
    }

    /// The URL, as it is stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether an authority names a host, and after it nothing, or a colon and a port in decimal
/// digits that fits in 16 bits, an empty one meaning port 80 (RFC 9110, section 4.2.1;
/// RFC 3986, sections 3.2.2 and 3.2.3). The parser takes an empty host, even an empty IP
/// literal, `[]`, and any text after the host, whose port a client cannot read and replaces
/// with 80 (ADR-0060).
fn names_a_host_and_its_port(authority: &Authority) -> bool {
    let host = authority.host();
    let fits = |digits: &str| {
        digits.is_empty()
            || (digits.bytes().all(|byte| byte.is_ascii_digit()) && digits.parse::<u16>().is_ok())
    };
    let after_host = authority
        .as_str()
        .rsplit('@')
        .next()
        .and_then(|host_and_port| host_and_port.strip_prefix(host));
    // An empty name, or empty brackets, names no host: an IP literal holds an address.
    !matches!(host, "" | "[]")
        && after_host
            .is_some_and(|rest| rest.is_empty() || rest.strip_prefix(':').is_some_and(fits))
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

/// Registers a merchant's webhook endpoint at this URL, under one of the operator's keys
/// (ADR-0050). Its secret is derived from the master key, and never stored (ADR-0051).
///
/// # Errors
///
/// See [`OperatorError`].
pub async fn register_webhook_endpoint(
    pool: &PgPool,
    seal: &OperatorSeal<'_>,
    id: EndpointId,
    merchant: MerchantId,
    url: &EndpointUrl,
) -> Result<(), OperatorError> {
    let effect = |source| OperatorError::at(Stage::Effect, source);
    let mut tx = pool.begin().await.map_err(effect)?;
    sqlx::query!(
        "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
        id.get(),
        merchant.get(),
        url.as_str()
    )
    .execute(&mut *tx)
    .await
    .map_err(effect)?;
    finish(tx, seal).await
}

/// The reason that a disablement by the operator stores. The relay's reasons are its own
/// (ADR-0053), and SQL binds each rather than spelling it (ADR-0009).
const BY_THE_OPERATOR: &str = "operator";

/// Disables a webhook endpoint, under one of the operator's keys (ADR-0050). Disabling it
/// again changes nothing, whatever disabled it first, and answers too: what the request
/// asks for already holds.
///
/// # Errors
///
/// See [`OperatorError`].
pub async fn disable_webhook_endpoint(
    pool: &PgPool,
    seal: &OperatorSeal<'_>,
    id: EndpointId,
) -> Result<(), OperatorError> {
    let effect = |source| OperatorError::at(Stage::Effect, source);
    let mut tx = pool.begin().await.map_err(effect)?;
    sqlx::query!(
        "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason) VALUES ($1, $2) \
         ON CONFLICT (endpoint_id) DO NOTHING",
        id.get(),
        BY_THE_OPERATOR
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
    fn an_endpoints_url_is_absolute_http_without_user_information_or_a_fragment() {
        // Taken, with the scheme in lowercase and the rest as written.
        for (given, stored) in [
            ("http://merchant.example", "http://merchant.example"),
            (
                "http://merchant.example:8000/hooks?from=payments",
                "http://merchant.example:8000/hooks?from=payments",
            ),
            (
                "HTTP://Merchant.Example/Hooks",
                "http://Merchant.Example/Hooks",
            ),
            ("hTtP://127.0.0.1:9/", "http://127.0.0.1:9/"),
            ("http://[::1]:8080/hooks", "http://[::1]:8080/hooks"),
            ("http://[::1]/hooks", "http://[::1]/hooks"),
            // The largest port, and an empty one, which means port 80.
            (
                "http://merchant.example:65535/",
                "http://merchant.example:65535/",
            ),
            (
                "http://merchant.example:/hooks",
                "http://merchant.example:/hooks",
            ),
        ] {
            assert_eq!(
                EndpointUrl::parse(given).map(|url| url.as_str().to_owned()),
                Ok(stored.to_owned()),
                "{given}"
            );
        }
        // Not an absolute URL: a malformed body.
        for given in [
            "",
            "not a url",
            "merchant.example",
            "/hooks",
            "http:///hooks",
            "http://merchant .example/",
            "http://merchant.example/a b",
            "http://merchant.example:port/",
            "http://merchant.example/\u{7}",
            // A host that is empty, and a port that a client would not go to.
            "http://:80/",
            "http://[]/",
            "http://[]:80/",
            "http://merchant.example:65536/",
            "http://merchant.example:+80/",
            "http://[::1]x/",
            "http://[::1]:x/",
            // Not a URL, even though it breaks a rule as well.
            "http://user@merchant.example:port/",
            "https://merchant.example:port/",
        ] {
            assert_eq!(
                EndpointUrl::parse(given),
                Err(UrlRefusal::NotAbsolute),
                "{given:?}"
            );
        }
        // An absolute URL that the rules refuse.
        for given in [
            "https://merchant.example/",
            "HTTPS://merchant.example/",
            "ftp://merchant.example/",
            "http://user@merchant.example/",
            "http://user:secret@merchant.example/",
            "http://merchant.example/hooks#part",
            "http://merchant.example#part",
            "http://merchant.example/?query#",
        ] {
            assert_eq!(
                EndpointUrl::parse(given),
                Err(UrlRefusal::Unusable),
                "{given}"
            );
        }
    }

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
