// Author: Nicola Antonello

//! Standard Webhooks `v1` verification, as a merchant's endpoint does it (ADR-0056): an
//! HMAC-SHA256 over `msg_id.timestamp.payload`, under the secret that registering the
//! endpoint answered (ADR-0051).
//!
//! The server signs with code of its own. The two share the specification's published
//! test vector, not code (ADR-0054).

use std::fmt;

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// How a secret is written: the specification's prefix, then base64.
const PREFIX: &str = "whsec_";

/// The headers of a delivery (ADR-0051).
const ID: &str = "webhook-id";
const TIMESTAMP: &str = "webhook-timestamp";
const SIGNATURE: &str = "webhook-signature";

/// How far a timestamp may stand from the fake's clock, either way: the tolerance of the
/// specification's reference library, and the server's own (ADR-0030).
pub const TOLERANCE_SECONDS: i64 = 5 * 60;

/// A secret that the fake trusts. Its bytes are a secret, so it prints none of them.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

/// Why a written secret was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("a secret starts with {PREFIX}")]
    Prefix,
    #[error("a secret is base64 after its prefix")]
    Base64,
    #[error("a secret has 24 to 64 bytes, not {0}")]
    Length(usize),
}

impl Secret {
    /// Reads a secret written `whsec_` and base64, of 24 to 64 bytes as the specification
    /// allows: the server's have 32, and the specification's test vector has 24.
    ///
    /// # Errors
    ///
    /// See [`SecretError`].
    pub fn parse(written: &str) -> Result<Self, SecretError> {
        let encoded = written.strip_prefix(PREFIX).ok_or(SecretError::Prefix)?;
        let bytes = STANDARD.decode(encoded).map_err(|_| SecretError::Base64)?;
        if !(24..=64).contains(&bytes.len()) {
            return Err(SecretError::Length(bytes.len()));
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Why a delivery was refused before its body was read (ADR-0056).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The header is missing, repeated, or not visible ASCII.
    #[error("{0} must appear once, in visible ASCII")]
    Header(&'static str),
    /// The ID is empty or holds a `.`, so `msg_id.timestamp.payload` could read two ways.
    #[error("{ID} must not be empty or hold a dot")]
    Id,
    /// The timestamp holds something other than digits.
    #[error("{TIMESTAMP} must hold only digits")]
    Timestamp,
    /// The timestamp stands more than five minutes from the fake's clock.
    #[error("{TIMESTAMP} is more than {TOLERANCE_SECONDS} seconds from the fake's clock")]
    Window,
    /// No listed `v1` signature verifies under a trusted secret.
    #[error("no v1 signature verifies under a trusted secret")]
    Signature,
}

/// Checks a delivery's headers and the bytes received, in ADR-0030's order, and stops at
/// the first check that fails. `now` is the fake's clock, an input so that the
/// specification's test vector can be checked at its own time.
///
/// # Errors
///
/// The first check that fails: see [`Refusal`].
pub fn verify(
    headers: &HeaderMap,
    body: &[u8],
    secrets: &[Secret],
    now: DateTime<Utc>,
) -> Result<(), Refusal> {
    let id = one(headers, ID).ok_or(Refusal::Header(ID))?;
    let timestamp = one(headers, TIMESTAMP).ok_or(Refusal::Header(TIMESTAMP))?;
    let listed = one(headers, SIGNATURE).ok_or(Refusal::Header(SIGNATURE))?;
    if id.is_empty() || id.contains('.') {
        return Err(Refusal::Id);
    }
    let seconds = seconds(timestamp).ok_or(Refusal::Timestamp)?;
    let now = now.timestamp();
    if !(now - TOLERANCE_SECONDS..=now + TOLERANCE_SECONDS).contains(&seconds) {
        return Err(Refusal::Window);
    }
    if !signed(secrets, id, timestamp, body, listed) {
        return Err(Refusal::Signature);
    }
    Ok(())
}

/// The value of a header that appears once, in visible ASCII.
fn one<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

/// Whole Unix seconds, written in digits alone: no sign, no space, no fraction.
fn seconds(timestamp: &str) -> Option<i64> {
    if !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    timestamp.parse().ok()
}

/// Whether one listed `v1` signature verifies under one trusted secret, over the
/// timestamp as it came and the bytes received. Entries of another scheme, and any that
/// is not base64, are skipped. `verify_slice` compares the tags with `subtle`'s `ct_eq`,
/// in constant time, so that timing cannot tell how much of a forged tag is right.
fn signed(secrets: &[Secret], id: &str, timestamp: &str, body: &[u8], listed: &str) -> bool {
    listed
        .split(' ')
        .filter_map(|entry| entry.strip_prefix("v1,"))
        .filter_map(|tag| STANDARD.decode(tag).ok())
        .any(|tag| {
            secrets.iter().any(|secret| {
                let mut mac = Hmac::<Sha256>::new_from_slice(&secret.0)
                    .expect("HMAC takes a key of any length");
                mac.update(id.as_bytes());
                mac.update(b".");
                mac.update(timestamp.as_bytes());
                mac.update(b".");
                mac.update(body);
                mac.verify_slice(&tag).is_ok()
            })
        })
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    // The specification's published test vector, from `test_sign` in its Rust reference
    // library: the one fixed key in the repository (ADR-0030).
    const VECTOR_SECRET: &str = "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD";
    const VECTOR_ID: &str = "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk";
    const VECTOR_TIMESTAMP: &str = "1649367553";
    const VECTOR_BODY: &[u8] = br#"{"email":"test@example.com","username":"test_user"}"#;
    const VECTOR_SIGNATURE: &str = "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=";
    /// The vector's time, as the fake's clock.
    const VECTOR_NOW: i64 = 1_649_367_553;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("a time in range")
    }

    fn vector() -> Vec<Secret> {
        vec![Secret::parse(VECTOR_SECRET).expect("the vector's secret")]
    }

    fn headers(id: &str, timestamp: &str, signature: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("webhook-id", id),
            ("webhook-timestamp", timestamp),
            ("webhook-signature", signature),
        ] {
            headers.insert(name, HeaderValue::from_str(value).expect("a header value"));
        }
        headers
    }

    /// A secret's bytes from the operating system, and the secret as the server writes it.
    fn new_secret() -> (Vec<u8>, String) {
        let mut bytes = vec![0_u8; 32];
        getrandom::fill(&mut bytes).expect("the operating system's random bytes");
        let written = format!("whsec_{}", STANDARD.encode(&bytes));
        (bytes, written)
    }

    fn trusting(written: &str) -> Vec<Secret> {
        vec![Secret::parse(written).expect("a secret written as the server writes it")]
    }

    /// The test's own signer, apart from the code under test: `v1,` and the base64 of the
    /// HMAC-SHA256 of `id.timestamp.body`.
    fn sign(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
        mac.update(format!("{id}.{timestamp}.").as_bytes());
        mac.update(body);
        format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn the_specifications_test_vector_verifies() {
        let signed = headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE);
        assert_eq!(
            verify(&signed, VECTOR_BODY, &vector(), at(VECTOR_NOW)),
            Ok(())
        );
    }

    #[test]
    fn every_signed_byte_counts() {
        let mut other_body = VECTOR_BODY.to_vec();
        other_body[2] = b'E';
        let (_, other_secret) = new_secret();
        for (spoiled, body, secrets) in [
            (
                headers(
                    "msg_27UH4WbU6Z5A5EzD8u03UvzRbpK",
                    VECTOR_TIMESTAMP,
                    VECTOR_SIGNATURE,
                ),
                VECTOR_BODY,
                vector(),
            ),
            (
                headers(VECTOR_ID, "1649367554", VECTOR_SIGNATURE),
                VECTOR_BODY,
                vector(),
            ),
            (
                headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE),
                &other_body[..],
                vector(),
            ),
            (
                headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE),
                VECTOR_BODY,
                trusting(&other_secret),
            ),
            (
                headers(
                    VECTOR_ID,
                    VECTOR_TIMESTAMP,
                    "v1,tZ1I5/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=",
                ),
                VECTOR_BODY,
                vector(),
            ),
        ] {
            assert_eq!(
                verify(&spoiled, body, &secrets, at(VECTOR_NOW)),
                Err(Refusal::Signature),
                "{spoiled:?}"
            );
        }
    }

    #[test]
    fn the_mac_covers_the_timestamp_as_received() {
        // The vector's second, written with a leading zero: a MAC over the text without
        // it does not verify, and a MAC over the text as it came does.
        let (key, written) = new_secret();
        let padded = "01649367553";
        for (signed_text, expected) in [
            (VECTOR_TIMESTAMP, Err(Refusal::Signature)),
            (padded, Ok(())),
        ] {
            let signature = sign(&key, VECTOR_ID, signed_text, VECTOR_BODY);
            let delivered = headers(VECTOR_ID, padded, &signature);
            assert_eq!(
                verify(&delivered, VECTOR_BODY, &trusting(&written), at(VECTOR_NOW)),
                expected,
                "signed over {signed_text}"
            );
        }
    }

    #[test]
    fn the_window_holds_its_ends_and_nothing_beyond() {
        let signed = headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE);
        for (now, expected) in [
            (VECTOR_NOW - 300, Ok(())),
            (VECTOR_NOW + 300, Ok(())),
            (VECTOR_NOW - 301, Err(Refusal::Window)),
            (VECTOR_NOW + 301, Err(Refusal::Window)),
        ] {
            assert_eq!(
                verify(&signed, VECTOR_BODY, &vector(), at(now)),
                expected,
                "now {now}"
            );
        }
    }

    #[test]
    fn any_trusted_secret_verifies_any_listed_v1_signature() {
        let (old_key, old) = new_secret();
        let (new_key, new) = new_secret();
        let both = vec![
            Secret::parse(&old).expect("a new secret"),
            Secret::parse(&new).expect("a new secret"),
        ];
        let by_old = sign(&old_key, VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_BODY);
        let by_new = sign(&new_key, VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_BODY);
        let old_tag = by_old.strip_prefix("v1,").expect("a v1 signature");
        let new_tag = by_new.strip_prefix("v1,").expect("a v1 signature");
        // Entries that must be skipped come first: another scheme with a tag that would
        // verify, a v1 entry that is not base64, and an unknown version.
        let listed = format!("v1a,{old_tag} v1,not*base64 v2,{old_tag} {by_new}");
        // Each case is named rather than printed: a message that shows a signature reads
        // to CodeQL's `rust/cleartext-logging` as a secret written out (ADR-0027).
        for (case, signature, secrets, expected) in [
            (
                "signed under the old secret",
                by_old.clone(),
                both.clone(),
                Ok(()),
            ),
            (
                "signed under the new secret",
                by_new.clone(),
                both.clone(),
                Ok(()),
            ),
            ("listed after entries to skip", listed, both.clone(), Ok(())),
            (
                "signed under a secret not trusted",
                by_new.clone(),
                trusting(&old),
                Err(Refusal::Signature),
            ),
            (
                "a v1a entry whose tag would verify",
                format!("v1a,{new_tag}"),
                both.clone(),
                Err(Refusal::Signature),
            ),
            (
                "no secret trusted",
                by_new,
                Vec::new(),
                Err(Refusal::Signature),
            ),
        ] {
            let delivered = headers(VECTOR_ID, VECTOR_TIMESTAMP, &signature);
            assert_eq!(
                verify(&delivered, VECTOR_BODY, &secrets, at(VECTOR_NOW)),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn each_header_appears_once_in_visible_ascii() {
        for name in ["webhook-id", "webhook-timestamp", "webhook-signature"] {
            let mut missing = headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE);
            missing.remove(name);
            let mut repeated = headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE);
            let value = repeated.get(name).expect("a header set above").clone();
            repeated.append(name, value);
            let mut opaque = headers(VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_SIGNATURE);
            opaque.insert(
                name,
                HeaderValue::from_bytes(b"\xff")
                    .expect("a byte above ASCII, which a header may hold"),
            );
            for spoiled in [missing, repeated, opaque] {
                assert_eq!(
                    verify(&spoiled, VECTOR_BODY, &vector(), at(VECTOR_NOW)),
                    Err(Refusal::Header(name)),
                    "{spoiled:?}"
                );
            }
        }
    }

    #[test]
    fn an_id_is_not_empty_and_holds_no_dot() {
        let (key, written) = new_secret();
        for id in ["", ".", "msg.1", "msg_1."] {
            let signed = headers(
                id,
                VECTOR_TIMESTAMP,
                &sign(&key, id, VECTOR_TIMESTAMP, VECTOR_BODY),
            );
            assert_eq!(
                verify(&signed, VECTOR_BODY, &trusting(&written), at(VECTOR_NOW)),
                Err(Refusal::Id),
                "{id:?}"
            );
        }
    }

    #[test]
    fn a_timestamp_holds_only_digits() {
        let (key, written) = new_secret();
        for timestamp in [
            "",
            "+1649367553",
            "-1649367553",
            "1649367553.0",
            " 1649367553",
            "1649367553 ",
            "1.6e9",
            "99999999999999999999",
        ] {
            let signed = headers(
                VECTOR_ID,
                timestamp,
                &sign(&key, VECTOR_ID, timestamp, VECTOR_BODY),
            );
            assert_eq!(
                verify(&signed, VECTOR_BODY, &trusting(&written), at(VECTOR_NOW)),
                Err(Refusal::Timestamp),
                "{timestamp:?}"
            );
        }
    }

    #[test]
    fn the_checks_run_in_order() {
        // Each delivery fails every check from the one expected on, so only the order
        // decides which refusal it gets.
        let wrong = "v1,tZ1I5/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=";
        let mut unsigned = headers("msg.1", "x", wrong);
        unsigned.remove("webhook-signature");
        for (spoiled, expected) in [
            (unsigned, Refusal::Header("webhook-signature")),
            (headers("msg.1", "x", wrong), Refusal::Id),
            (headers(VECTOR_ID, "x", wrong), Refusal::Timestamp),
            (headers(VECTOR_ID, "1", wrong), Refusal::Window),
        ] {
            assert_eq!(
                verify(&spoiled, VECTOR_BODY, &vector(), at(VECTOR_NOW)),
                Err(expected)
            );
        }
    }

    #[test]
    fn a_written_secret_has_its_prefix_base64_and_24_to_64_bytes() {
        for length in [24, 32, 64] {
            let written = format!("whsec_{}", STANDARD.encode(vec![7_u8; length]));
            assert!(Secret::parse(&written).is_ok(), "{length} bytes");
        }
        for (written, error) in [
            (
                format!("whsec_{}", STANDARD.encode([7_u8; 23])),
                SecretError::Length(23),
            ),
            (
                format!("whsec_{}", STANDARD.encode([7_u8; 65])),
                SecretError::Length(65),
            ),
            (STANDARD.encode([7_u8; 32]), SecretError::Prefix),
            ("whsec_not base64!".to_owned(), SecretError::Base64),
        ] {
            assert_eq!(Secret::parse(&written).unwrap_err(), error, "{written}");
        }
    }

    #[test]
    fn a_secret_prints_none_of_its_bytes() {
        let (_, written) = new_secret();
        let secret = Secret::parse(&written).expect("a new secret");
        assert_eq!(format!("{secret:?}"), "Secret(..)");
    }
}
