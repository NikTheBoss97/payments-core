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

use crate::ids::EndpointId;

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
    use super::*;

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
