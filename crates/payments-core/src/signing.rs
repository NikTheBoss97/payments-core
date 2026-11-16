// Author: Nicola Antonello

//! Standard Webhooks `v1`, both ways (ADR-0030, ADR-0051): keys written `whsec_` and
//! base64, and the HMAC-SHA256 over `id.timestamp.body`. This one computation checks the
//! PSP's notifications and signs the deliveries to merchants, so the specification's test
//! vector holds for both.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

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
