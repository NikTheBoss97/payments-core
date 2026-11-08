// Author: Nicola Antonello

//! Standard Webhooks `v1` signatures (ADR-0030): an HMAC-SHA256 over
//! `msg_id.timestamp.payload`, sent as `v1,` and the MAC in base64.
//!
//! The server verifies them with code of its own. The two share the specification's
//! published test vector, not code (ADR-0029).

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// How a key is written: the specification's prefix, then base64.
const PREFIX: &str = "whsec_";

/// A key that signs notifications. Its bytes are a secret, so it prints none of them.
#[derive(Clone)]
pub struct SigningKey(Vec<u8>);

/// Why a written key was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("a signing key starts with {PREFIX}")]
    Prefix,
    #[error("a signing key is base64 after its prefix")]
    Base64,
    #[error("a signing key has 24 to 64 bytes, not {0}")]
    Length(usize),
}

impl SigningKey {
    /// 32 random bytes from the operating system, the length of SHA-256's output:
    /// RFC 2104 calls a key shorter than that "strongly discouraged" (ADR-0030).
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = vec![0_u8; 32];
        getrandom::fill(&mut bytes)
            .expect("a signing key needs the operating system's random bytes");
        Self(bytes)
    }

    /// Reads a key written `whsec_` and base64, of 24 to 64 bytes as the specification
    /// allows. The keys made here have 32, and the specification's test vector has 24.
    ///
    /// # Errors
    ///
    /// See [`KeyError`].
    pub fn parse(written: &str) -> Result<Self, KeyError> {
        let encoded = written.strip_prefix(PREFIX).ok_or(KeyError::Prefix)?;
        let bytes = STANDARD.decode(encoded).map_err(|_| KeyError::Base64)?;
        if !(24..=64).contains(&bytes.len()) {
            return Err(KeyError::Length(bytes.len()));
        }
        Ok(Self(bytes))
    }

    /// The key as the receiving server reads it from its environment.
    #[must_use]
    pub fn written(&self) -> String {
        format!("{PREFIX}{}", STANDARD.encode(&self.0))
    }

    /// The signature of one delivery: `v1,` and the base64 of the HMAC-SHA256 of
    /// `msg_id.timestamp.payload`, over the payload's bytes as they are sent.
    #[must_use]
    pub fn sign(&self, msg_id: &str, timestamp: i64, payload: &[u8]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC takes a key of any length");
        mac.update(msg_id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(payload);
        format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(..)")
    }
}

/// The `webhook-signature` header: a signature under each key, separated by spaces, so
/// that during a rotation a receiver that holds either key accepts it (ADR-0030).
#[must_use]
pub fn signature_header(
    keys: &[SigningKey],
    msg_id: &str,
    timestamp: i64,
    payload: &[u8],
) -> String {
    keys.iter()
        .map(|key| key.sign(msg_id, timestamp, payload))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The known-answer test of the specification's Rust reference library, `test_sign`
    /// in `libraries/rust/src/lib.rs`, which pins this signer to the specification.
    #[test]
    fn signs_the_specifications_test_vector() {
        let key = SigningKey::parse("whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD").expect("a valid key");
        let signature = key.sign(
            "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk",
            1_649_367_553,
            br#"{"email":"test@example.com","username":"test_user"}"#,
        );
        assert_eq!(signature, "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=");
    }

    #[test]
    fn the_id_the_timestamp_every_byte_and_the_key_are_signed() {
        let key = SigningKey::generate();
        let payload = br#"{"a":1}"#;
        let signature = key.sign("msg_1", 1_000, payload);
        for other in [
            key.sign("msg_2", 1_000, payload),
            key.sign("msg_1", 1_001, payload),
            key.sign("msg_1", 1_000, br#"{"a":2}"#),
            key.sign("msg_1", 1_000, br#"{"a":1} "#),
            SigningKey::generate().sign("msg_1", 1_000, payload),
        ] {
            assert_ne!(other, signature);
        }
        assert_eq!(key.sign("msg_1", 1_000, payload), signature);
    }

    #[test]
    fn a_written_key_has_its_prefix_base64_and_24_to_64_bytes() {
        for length in [24, 32, 64] {
            let written = format!("{PREFIX}{}", STANDARD.encode(vec![7_u8; length]));
            let key = SigningKey::parse(&written).expect("a key of an allowed length");
            assert_eq!(key.written(), written);
        }
        for (written, error) in [
            (
                format!("{PREFIX}{}", STANDARD.encode([7_u8; 23])),
                KeyError::Length(23),
            ),
            (
                format!("{PREFIX}{}", STANDARD.encode([7_u8; 65])),
                KeyError::Length(65),
            ),
            (STANDARD.encode([7_u8; 32]), KeyError::Prefix),
            (format!("{PREFIX}not base64!"), KeyError::Base64),
        ] {
            assert_eq!(SigningKey::parse(&written).unwrap_err(), error, "{written}");
        }
    }

    #[test]
    fn a_new_key_has_32_bytes_and_reads_back() {
        let key = SigningKey::generate();
        assert_eq!(key.0.len(), 32);
        let read = SigningKey::parse(&key.written()).expect("a key written here");
        assert_eq!(read.written(), key.written());
        assert_ne!(SigningKey::generate().written(), key.written());
    }

    #[test]
    fn a_rotation_signs_under_each_key_in_order() {
        let (old, new) = (SigningKey::generate(), SigningKey::generate());
        let header = signature_header(&[old.clone(), new.clone()], "msg_1", 1, b"{}");
        let expected = format!(
            "{} {}",
            old.sign("msg_1", 1, b"{}"),
            new.sign("msg_1", 1, b"{}")
        );
        assert_eq!(header, expected);
    }

    #[test]
    fn a_key_prints_none_of_its_bytes() {
        assert_eq!(format!("{:?}", SigningKey::generate()), "SigningKey(..)");
    }
}
