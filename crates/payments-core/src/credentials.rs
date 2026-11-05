// Author: Nicola Antonello

//! API keys, and finding the merchant that presented one (ADR-0024).
//!
//! A secret is 32 random bytes, sent as a Bearer token and shown once. Only its
//! SHA-256 digest is stored, and authenticating hashes what was presented and looks
//! the digest up. Nothing here is guessable, so no password hash is needed, and an
//! equality test on a digest reveals nothing useful about the secret.

use std::fmt;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use subtle::ConstantTimeEq;

use crate::ids::MerchantId;

/// An API key's secret: `pck_` and 64 lowercase hex digits. It cannot print itself,
/// so that no log line can hold one by accident: reading it takes a call to
/// [`expose`](Self::expose).
///
/// ```
/// use payments_core::credentials::ApiKeySecret;
///
/// let secret = ApiKeySecret::generate();
/// assert_eq!(format!("{secret:?}"), "ApiKeySecret(..)");
/// ```
///
/// ```compile_fail
/// use payments_core::credentials::ApiKeySecret;
///
/// let secret = ApiKeySecret::generate();
/// println!("{secret}"); // no Display
/// ```
pub struct ApiKeySecret(String);

impl ApiKeySecret {
    /// A new secret, from the operating system's random source.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).expect("a secret needs the operating system's random bytes");
        let digits: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Self(format!("pck_{digits}"))
    }

    /// The secret itself, for the one answer that shows it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// What is stored in its place.
    #[must_use]
    pub fn digest(&self) -> SecretDigest {
        SecretDigest::of(&self.0)
    }
}

impl fmt::Debug for ApiKeySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKeySecret(..)")
    }
}

/// The SHA-256 digest of a presented or a new secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretDigest([u8; 32]);

impl SecretDigest {
    /// The digest of what was presented, whatever that is: a malformed secret has a
    /// digest that no key has.
    #[must_use]
    pub fn of(secret: &str) -> Self {
        Self(Sha256::digest(secret).into())
    }

    /// A digest written as 64 lowercase hex digits, the form that `sha256sum` prints and
    /// the environment holds (ADR-0024).
    ///
    /// # Errors
    ///
    /// [`InvalidDigest`] for anything else.
    pub fn from_hex(text: &str) -> Result<Self, InvalidDigest> {
        let digits = text.as_bytes();
        if digits.len() != 64 {
            return Err(InvalidDigest);
        }
        let mut bytes = [0_u8; 32];
        for (byte, [high, low]) in bytes.iter_mut().zip(digits.as_chunks::<2>().0) {
            *byte = hex_value(*high)? * 16 + hex_value(*low)?;
        }
        Ok(Self(bytes))
    }

    /// Whether two digests are the same, in constant time, for the operator's key
    /// (ADR-0024).
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.as_bytes().ct_eq(other.as_bytes()).into()
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A written digest that is not 64 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a digest is written as 64 lowercase hex digits")]
pub struct InvalidDigest;

fn hex_value(digit: u8) -> Result<u8, InvalidDigest> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(InvalidDigest),
    }
}

/// Why a presented secret could not be looked up. Looking it up again is safe, and it
/// says nothing about whether the secret is good (ADR-0024).
#[derive(Debug, thiserror::Error)]
#[error("an API key could not be looked up")]
pub struct AuthenticateError(#[source] sqlx::Error);

/// The merchant that this secret was issued to, or `None` when no key has it or its
/// key is revoked. Pass the system role's pool: no merchant is known yet (ADR-0025).
///
/// # Errors
///
/// [`AuthenticateError`] when the key cannot be looked up.
pub async fn authenticate(
    pool: &PgPool,
    presented: &str,
) -> Result<Option<MerchantId>, AuthenticateError> {
    let digest = SecretDigest::of(presented);
    let merchant = sqlx::query_scalar!(
        "SELECT k.merchant_id FROM api_keys k \
         WHERE k.digest = $1 \
           AND NOT EXISTS (SELECT 1 FROM api_key_revocations r WHERE r.api_key_id = k.id)",
        digest.as_bytes()
    )
    .fetch_optional(pool)
    .await
    .map_err(AuthenticateError)?;
    Ok(merchant.map(|id| {
        MerchantId::try_from(id).expect("api_keys_merchant_fkey points at a merchant's UUIDv7 ID")
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn hex(digest: SecretDigest) -> String {
        digest
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    // These tests print nothing that came from a secret, even a throwaway one: a failure
    // message is a log line too.

    #[test]
    fn a_new_secret_is_the_prefix_and_64_lowercase_hex_digits() {
        let generated = ApiKeySecret::generate();
        let digits = generated
            .expose()
            .strip_prefix("pck_")
            .expect("the prefix a secret scanner looks for");
        assert_eq!(digits.len(), 64);
        assert!(
            digits
                .bytes()
                .all(|digit| matches!(digit, b'0'..=b'9' | b'a'..=b'f'))
        );
    }

    #[test]
    fn no_two_secrets_are_the_same() {
        let secrets: HashSet<String> = (0..1000)
            .map(|_| ApiKeySecret::generate().expose().to_owned())
            .collect();
        assert_eq!(secrets.len(), 1000);
    }

    #[test]
    fn a_digest_is_the_sha_256_of_the_secret() {
        // Computed with Python's hashlib and with PostgreSQL's sha256, not with this
        // code.
        let zeros = format!("pck_{}", "0".repeat(64));
        let pairs = format!("pck_{}", "ab".repeat(32));
        for (presented, expected) in [
            (
                zeros.as_str(),
                "3ef3e417230060c87cead66b3ec5474b73177abd1127177474de6252466df16c",
            ),
            (
                pairs.as_str(),
                "06a1dd6be846256db25b45a9533901bf1957f041f753f7948b3f9616e7e31335",
            ),
            (
                "not a key",
                "edfc089719c0a061bec08afad3dead4480add63a9677c13cdb5bba1779160d0f",
            ),
        ] {
            assert_eq!(hex(SecretDigest::of(presented)), expected);
        }
    }

    #[test]
    fn a_new_secret_digests_like_the_same_text_presented() {
        let generated = ApiKeySecret::generate();
        assert_eq!(generated.digest(), SecretDigest::of(generated.expose()));
        assert_ne!(generated.digest(), SecretDigest::of(""));
    }

    /// What Python's hashlib and PostgreSQL's sha256 gave for `pck_` and 64 zeros.
    const WRITTEN: &str = "3ef3e417230060c87cead66b3ec5474b73177abd1127177474de6252466df16c";

    #[test]
    fn a_written_digest_reads_as_the_digest_it_names() {
        let zeros = format!("pck_{}", "0".repeat(64));
        assert_eq!(
            SecretDigest::from_hex(WRITTEN),
            Ok(SecretDigest::of(&zeros))
        );
    }

    #[test]
    fn every_digit_of_a_written_digest_has_its_value() {
        let every_digit = "0123456789abcdef".repeat(4);
        let read = SecretDigest::from_hex(&every_digit).expect("64 lowercase hex digits");
        assert_eq!(read.as_bytes()[..8], [1, 35, 69, 103, 137, 171, 205, 239]);
        assert_eq!(hex(read), every_digit);
    }

    #[test]
    fn anything_else_is_not_a_written_digest() {
        let long = format!("{WRITTEN}0");
        let upper = WRITTEN.to_uppercase();
        // Rust's own parser for a number takes a leading sign.
        let signed = format!("+{}", &WRITTEN[1..]);
        let spaced = format!(" {}", &WRITTEN[1..]);
        let not_hex = format!("g{}", &WRITTEN[1..]);
        // 64 bytes, but not 64 digits.
        let not_ascii = "é".repeat(32);
        let prefixed = format!("0x{}", &WRITTEN[2..]);
        // What an operator might paste by mistake: the key itself.
        let the_key_itself = format!("pck_{WRITTEN}");
        for text in [
            "",
            &WRITTEN[..63],
            long.as_str(),
            upper.as_str(),
            signed.as_str(),
            spaced.as_str(),
            not_hex.as_str(),
            not_ascii.as_str(),
            prefixed.as_str(),
            the_key_itself.as_str(),
        ] {
            assert_eq!(SecretDigest::from_hex(text), Err(InvalidDigest), "{text:?}");
        }
    }

    #[test]
    fn a_digest_matches_only_itself() {
        let one = SecretDigest::of("one");
        assert!(one.matches(&SecretDigest::of("one")));
        assert!(!one.matches(&SecretDigest::of("two")));
        // One bit is enough, wherever it is.
        for (byte, bit) in [(0, 1), (31, 0x80)] {
            let mut other = one;
            other.0[byte] ^= bit;
            assert!(!one.matches(&other), "byte {byte}");
        }
    }
}
