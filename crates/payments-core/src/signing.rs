// Author: Nicola Antonello

//! Standard Webhooks `v1`, both ways (ADR-0030, ADR-0051): keys written `whsec_` and
//! base64, and the HMAC-SHA256 over `id.timestamp.body`. This one computation checks the
//! PSP's notifications and signs the deliveries to merchants, so the specification's test
//! vector holds for both.
//!
//! Each endpoint signs under a secret of its own, derived from one master key, so that
//! nothing secret is stored (ADR-0051).

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::types::chrono::{DateTime, Utc};

use crate::ids::{EndpointId, EventId};

/// The MAC of `v1`.
pub(crate) type HmacSha256 = Hmac<Sha256>;

/// How a key is written: the specification's prefix, then base64.
const PREFIX: &str = "whsec_";

/// Why a key, or a list of keys, was refused. No message repeats what it read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("it lists no key")]
    None,
    #[error("a key does not start with whsec_")]
    Prefix,
    #[error("a key is not base64 after its prefix")]
    Base64,
    #[error("a key has {0} bytes, not 24 to 64")]
    Length(usize),
    #[error("the master key has {0} bytes, not 32")]
    MasterLength(usize),
}

/// A key's bytes, from `whsec_` and base64: the one parser of every key (ADR-0030,
/// ADR-0051). Each kind of key checks its own length.
pub(crate) fn decode(written: &str) -> Result<Vec<u8>, KeyError> {
    let encoded = written.strip_prefix(PREFIX).ok_or(KeyError::Prefix)?;
    STANDARD.decode(encoded).map_err(|_| KeyError::Base64)
}

/// The MAC of `v1` under `key` over `id.timestamp.body`: the ID and the timestamp as their
/// headers carry them, and the body's bytes as sent.
pub(crate) fn mac(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

/// The start of an endpoint's `info`, which the endpoint's 16 ID bytes end (ADR-0051).
const ENDPOINT_INFO: &[u8] = b"payments-core webhook endpoint v1";

/// The master key, from which every endpoint's secret is derived, and which signs nothing
/// itself (ADR-0051). Its 32 bytes come from the operating system, so HKDF needs no extract
/// step (RFC 5869, section 3.3). It prints none of its bytes.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    /// Reads the master key, written `whsec_` and base64 of exactly 32 bytes: HKDF-Expand
    /// takes a pseudorandom key of at least SHA-256's 32 bytes (RFC 5869, section 2.3), and
    /// ADR-0051 makes it 32.
    ///
    /// # Errors
    ///
    /// See [`KeyError`].
    pub fn parse(written: &str) -> Result<Self, KeyError> {
        let bytes = decode(written)?;
        <[u8; 32]>::try_from(bytes.as_slice())
            .map(Self)
            .map_err(|_| KeyError::MasterLength(bytes.len()))
    }

    /// The secret of this endpoint: HKDF-Expand with SHA-256 under the master key, with the
    /// `info` [`ENDPOINT_INFO`] followed by the endpoint's 16 ID bytes, and 32 bytes out
    /// (ADR-0051). Each signature derives it again, so nothing secret is stored.
    #[must_use]
    pub fn secret_of(&self, endpoint: EndpointId) -> EndpointSecret {
        EndpointSecret(expand(&self.0, &[ENDPOINT_INFO, endpoint.get().as_bytes()]))
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// An endpoint's secret (ADR-0051). It prints none of its bytes: only
/// [`EndpointSecret::written`] shows it, for the one answer that registers its endpoint
/// (ADR-0050).
#[derive(Clone)]
pub struct EndpointSecret([u8; 32]);

impl EndpointSecret {
    /// The secret as a merchant's `v1` library reads it: `whsec_` and base64.
    #[must_use]
    pub fn written(&self) -> String {
        format!("{PREFIX}{}", STANDARD.encode(self.0))
    }
}

impl fmt::Debug for EndpointSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EndpointSecret(..)")
    }
}

/// The three headers of one attempt to deliver an event (ADR-0051).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// `webhook-id`: the event's ID as the API writes IDs, the same at every attempt and at
    /// every endpoint, by which a merchant drops copies.
    pub id: String,
    /// `webhook-timestamp`: the attempt's time, in whole seconds.
    pub timestamp: String,
    /// `webhook-signature`: `v1,` and the base64 of the MAC.
    pub signature: String,
}

/// Signs one attempt to deliver an event's body, its bytes as sent, to the endpoint whose
/// secret this is. `at` is the attempt's own time, which a receiver checks against its
/// clock, so a retry is signed afresh (ADR-0051, ADR-0056).
#[must_use]
pub fn sign(secret: &EndpointSecret, event: EventId, at: DateTime<Utc>, body: &[u8]) -> Signed {
    let id = event.to_string();
    let timestamp = at.timestamp().to_string();
    let signature = signature(&secret.0, &id, &timestamp, body);
    Signed {
        id,
        timestamp,
        signature,
    }
}

/// The `v1` entry of `webhook-signature`: `v1,` and the base64 of the MAC.
fn signature(key: &[u8], id: &str, timestamp: &str, body: &[u8]) -> String {
    let tag = mac(key, id, timestamp, body).finalize().into_bytes();
    format!("v1,{}", STANDARD.encode(tag))
}

/// HKDF-Expand with SHA-256 for 32 bytes out (RFC 5869, section 2.3), with `info` given in
/// parts: the first block alone, `T(1) = HMAC-Hash(PRK, T(0) | info | 0x01)` with `T(0)`
/// empty. It is computed with `hmac` rather than `hkdf`, whose output fills a zeroed buffer
/// that CodeQL reads as a hard-coded key.
fn expand(prk: &[u8; 32], info: &[&[u8]]) -> [u8; 32] {
    let mut block = HmacSha256::new_from_slice(prk).expect("HMAC takes a key of any length");
    for part in info {
        block.update(part);
    }
    block.update(&[1]);
    block.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;
    use crate::notifications::{Notification, Refusal, SigningKeys, verify};

    /// The specification's test vector: `test_sign` in its Rust reference library.
    const VECTOR_KEY: &str = "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD";
    const VECTOR_ID: &str = "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk";
    const VECTOR_TIMESTAMP: &str = "1649367553";
    const VECTOR_BODY: &[u8] = br#"{"email":"test@example.com","username":"test_user"}"#;
    const VECTOR_SIGNATURE: &str = "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=";

    /// An event's body, which names its payment, as the PSP's notifications do.
    const PAYMENT: &str = "0196a5b4-c3d2-7e8f-9a0b-1c2d3e4f5a6b";
    const BODY: &[u8] = br#"{"type":"payment.updated","timestamp":"2026-01-01T00:00:00.123456Z","data":{"payment_id":"0196a5b4-c3d2-7e8f-9a0b-1c2d3e4f5a6b"}}"#;

    /// 2026-01-01T00:00:00.654321Z: an attempt's time, with a fraction of a second.
    fn attempt() -> DateTime<Utc> {
        DateTime::from_timestamp_micros(1_767_225_600_654_321).expect("a time in range")
    }

    /// The headers of a signed attempt, as a receiver gets them.
    fn headers(signed: &Signed) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("webhook-id", &signed.id),
            ("webhook-timestamp", &signed.timestamp),
            ("webhook-signature", &signed.signature),
        ] {
            headers.insert(name, HeaderValue::from_str(value).expect("a header value"));
        }
        headers
    }

    /// Whether our own check of the PSP's notifications reads this attempt as signed under
    /// `secret` at `now`.
    fn verified(
        signed: &Signed,
        secret: &EndpointSecret,
        now: DateTime<Utc>,
    ) -> Result<Notification, Refusal> {
        let keys = SigningKeys::parse(&secret.written()).expect("a secret of 32 bytes");
        verify(&headers(signed), BODY, &keys, now)
    }

    #[test]
    fn the_specifications_test_vector_is_signed() {
        let key = decode(VECTOR_KEY).expect("the vector's key");
        assert!(
            signature(&key, VECTOR_ID, VECTOR_TIMESTAMP, VECTOR_BODY) == VECTOR_SIGNATURE,
            "the specification's test vector"
        );
    }

    #[test]
    fn an_attempt_carries_the_events_id_and_its_own_time() {
        let secret = master().secret_of(EndpointId::now_v7());
        let event = EventId::now_v7();
        let signed = sign(&secret, event, attempt(), BODY);
        // The event's ID as the API writes IDs, and the attempt's whole seconds (ADR-0051).
        assert_eq!(signed.id, event.to_string());
        assert_eq!(signed.timestamp, "1767225600");
        let read = verified(&signed, &secret, attempt())
            .map(|notification| (notification.event_id, notification.payment.to_string()));
        assert_eq!(read, Ok((event.to_string(), PAYMENT.to_owned())));
    }

    #[test]
    fn a_retry_is_signed_afresh_under_the_same_id() {
        let secret = master().secret_of(EndpointId::now_v7());
        let event = EventId::now_v7();
        // Half an hour later.
        let later =
            DateTime::from_timestamp_micros(1_767_227_400_654_321).expect("a time in range");
        let first = sign(&secret, event, attempt(), BODY);
        let retry = sign(&secret, event, later, BODY);
        assert_eq!(retry.id, first.id);
        assert_eq!(retry.timestamp, "1767227400");
        assert!(retry.signature != first.signature, "a new signature");
        // Each passes a receiver's window at its own time, which the first would not.
        assert!(
            verified(&retry, &secret, later).is_ok(),
            "the retry, when it is sent"
        );
        assert!(
            verified(&first, &secret, later) == Err(Refusal::Window),
            "the first attempt, half an hour later"
        );
    }

    #[test]
    fn each_endpoint_signs_under_its_own_secret() {
        let master = master();
        let (ours, theirs) = (
            master.secret_of(EndpointId::now_v7()),
            master.secret_of(EndpointId::now_v7()),
        );
        let event = EventId::now_v7();
        let signed = sign(&ours, event, attempt(), BODY);
        let elsewhere = sign(&theirs, event, attempt(), BODY);
        assert!(elsewhere.signature != signed.signature, "another signature");
        assert!(
            verified(&signed, &ours, attempt()).is_ok(),
            "under its own secret"
        );
        assert!(
            verified(&signed, &theirs, attempt()) == Err(Refusal::Signature),
            "under another endpoint's secret"
        );
    }

    /// `length` bytes from the operating system, and how a key of them is written. Tests make
    /// their keys at runtime: the only fixed ones are published test vectors.
    fn random_written(length: usize) -> (Vec<u8>, String) {
        let mut bytes = vec![0_u8; length];
        getrandom::fill(&mut bytes).expect("the operating system's randomness");
        let written = format!("{PREFIX}{}", STANDARD.encode(&bytes));
        (bytes, written)
    }

    fn master() -> MasterKey {
        MasterKey::parse(&random_written(32).1).expect("32 bytes")
    }

    fn hex(digits: &str) -> Vec<u8> {
        (0..digits.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&digits[at..at + 2], 16).expect("hex digits"))
            .collect()
    }

    #[test]
    fn the_expand_step_gives_rfc_5869s_test_case_1() {
        // Appendix A.1: the pseudorandom key that the extract step gives, and the first 32 of
        // the 42 bytes that the expand step gives. They are its first block, which is all of
        // the 32 bytes that ADR-0051 runs it for.
        let prk = <[u8; 32]>::try_from(hex(
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
        ))
        .expect("32 bytes");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let okm = expand(&prk, &[&info]);
        assert_eq!(
            okm.to_vec(),
            hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf")
        );
    }

    #[test]
    fn an_endpoints_secret_is_one_hmac_block_over_its_info() {
        // With 32 bytes out, HKDF-Expand is one block: the HMAC under the master key of the
        // info and the byte 1 (RFC 5869, section 2.3). Recomputed here from the inputs that
        // ADR-0051 fixed, written out, the test pins them: a change would change every
        // endpoint's secret.
        let (bytes, written) = random_written(32);
        let master = MasterKey::parse(&written).expect("32 bytes");
        let endpoint = EndpointId::now_v7();
        let mut block = HmacSha256::new_from_slice(&bytes).expect("HMAC takes a key of any length");
        block.update(b"payments-core webhook endpoint v1");
        block.update(endpoint.get().as_bytes());
        block.update(&[1]);
        let expected = format!("{PREFIX}{}", STANDARD.encode(block.finalize().into_bytes()));
        assert!(
            master.secret_of(endpoint).written() == expected,
            "the derivation of ADR-0051"
        );
    }

    #[test]
    fn each_endpoint_and_each_master_key_gives_its_own_secret() {
        let master = master();
        let (one, two) = (EndpointId::now_v7(), EndpointId::now_v7());
        let first = master.secret_of(one).written();
        assert!(
            master.secret_of(one).written() == first,
            "one endpoint gets the same secret again"
        );
        assert!(
            master.secret_of(two).written() != first,
            "another endpoint gets another secret"
        );
        assert!(
            self::master().secret_of(one).written() != first,
            "another master key gives another secret"
        );
    }

    #[test]
    fn a_secret_is_written_whsec_and_base64_of_32_bytes() {
        let written = master().secret_of(EndpointId::now_v7()).written();
        assert!(written.starts_with("whsec_"), "the prefix");
        assert!(
            decode(&written).is_ok_and(|bytes| bytes.len() == 32),
            "base64 of 32 bytes"
        );
    }

    #[test]
    fn a_master_key_is_written_whsec_and_base64_of_exactly_32_bytes() {
        for length in [0, 24, 31, 33, 64] {
            let (_, written) = random_written(length);
            assert!(
                MasterKey::parse(&written).err() == Some(KeyError::MasterLength(length)),
                "{length} bytes"
            );
        }
        let (_, written) = random_written(32);
        assert!(MasterKey::parse(&written).is_ok(), "32 bytes");
        assert!(
            MasterKey::parse(&written[PREFIX.len()..]).err() == Some(KeyError::Prefix),
            "no prefix"
        );
        assert!(
            MasterKey::parse("whsec_not base64!").err() == Some(KeyError::Base64),
            "not base64"
        );
    }

    #[test]
    fn no_key_prints_its_bytes() {
        let (bytes, written) = random_written(32);
        let master = MasterKey::parse(&written).expect("32 bytes");
        let derived = master.secret_of(EndpointId::now_v7());
        assert!(
            format!("{master:?}") == "MasterKey(..)",
            "the master key's Debug"
        );
        assert!(
            format!("{derived:?}") == "EndpointSecret(..)",
            "an endpoint's secret's Debug"
        );
        // Nor does the error of a key refused for its length.
        let short = format!("{PREFIX}{}", STANDARD.encode(&bytes[..31]));
        let refused = MasterKey::parse(&short).err().map(|err| err.to_string());
        assert!(
            refused.is_some_and(|message| !message.contains(&short[PREFIX.len()..])),
            "the refusal's message"
        );
    }
}
