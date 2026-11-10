// Author: Nicola Antonello

//! The PSP's notifications (ADR-0030, ADR-0031, ADR-0032, ADR-0043): verifying one
//! before its body is parsed, and recording it once.
//!
//! A notification follows Standard Webhooks `v1`: an HMAC-SHA256, under a key that the
//! PSP and the service share, over `msg_id.timestamp.payload`. [`verify`] checks, in
//! order, that the ID and the timestamp read one way only, that the timestamp is within
//! 5 minutes of the service's clock, and that one listed signature matches one key, and
//! only then parses the body. A notification decides nothing: it names a payment whose
//! history the service then reads (ADR-0031). [`record`] keeps it once, by its PSP and
//! its event ID, so that a copy is always known as one.
//!
//! The fake signs with code of its own. The two share the specification's test vector,
//! not code (ADR-0029).

use std::fmt;

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::digest::Output;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::accounts::Psp;
use crate::db::{CommitError, code_and_constraint, commit};
use crate::ids::PaymentId;

type HmacSha256 = Hmac<Sha256>;

/// How a key is written: the specification's prefix, then base64.
const PREFIX: &str = "whsec_";

/// How far a notification's timestamp may be from the service's clock, either way, in
/// seconds: the tolerance of the specification's reference library (ADR-0030). The
/// window holds its ends (ADR-0043).
pub const TOLERANCE_SECONDS: i64 = 5 * 60;

/// The keys that may sign a notification: the current one, and during a rotation the
/// next (ADR-0030). They are secrets, so they print none of their bytes.
#[derive(Clone)]
pub struct SigningKeys(Vec<Vec<u8>>);

/// Why a list of keys was refused. No message repeats what it read.
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

impl SigningKeys {
    /// Reads keys separated by spaces, each written `whsec_` and base64, of 24 to 64
    /// bytes: the specification's range, so that its test vector, whose key has 24, goes
    /// through this parser too. Every key that this project makes has 32 (ADR-0043).
    ///
    /// # Errors
    ///
    /// See [`KeyError`].
    pub fn parse(listed: &str) -> Result<Self, KeyError> {
        let keys = listed
            .split_whitespace()
            .map(key)
            .collect::<Result<Vec<_>, _>>()?;
        if keys.is_empty() {
            return Err(KeyError::None);
        }
        Ok(Self(keys))
    }
}

/// One key's bytes, from `whsec_` and base64.
fn key(written: &str) -> Result<Vec<u8>, KeyError> {
    let encoded = written.strip_prefix(PREFIX).ok_or(KeyError::Prefix)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| KeyError::Base64)?;
    if !(24..=64).contains(&bytes.len()) {
        return Err(KeyError::Length(bytes.len()));
    }
    Ok(bytes)
}

impl fmt::Debug for SigningKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKeys(..)")
    }
}

/// What a verified notification says: its event ID, which tells its copies apart
/// (ADR-0031), and the payment that it names, which the service may not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub event_id: String,
    pub payment: Uuid,
}

/// Why a notification was refused, by the first check that failed. The route answers
/// each with the same 400 (ADR-0043); the reason tells a test which check refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("a header of the notification is missing, repeated or not visible ASCII")]
    Headers,
    #[error("the notification's ID is not 1 to 255 of [0-9A-Za-z_-]")]
    Id,
    #[error("the notification's timestamp is not ASCII digits that fit in 64 bits")]
    Timestamp,
    #[error("the notification's timestamp is more than 5 minutes from the clock")]
    Window,
    #[error("no signature of the notification matches a key")]
    Signature,
    #[error("the notification's body does not name a payment")]
    Body,
}

/// Verifies a notification and reads it (ADR-0030, ADR-0043). The checks run in this
/// order, and the first that fails refuses it:
///
/// 1. `webhook-id`, `webhook-timestamp` and `webhook-signature` each appear once;
/// 2. the ID is 1 to 255 of `[0-9A-Za-z_-]` and the timestamp is ASCII digits, so that
///    `msg_id.timestamp.payload` reads one way only;
/// 3. the timestamp is at most [`TOLERANCE_SECONDS`] from `now`, either way;
/// 4. one `v1` signature of the list matches the MAC of one key over the ID and the
///    timestamp as received and the body's bytes, compared in constant time;
/// 5. only then is the body parsed, and it must name `data.payment_id`.
///
/// `now` is the service's clock, read by the caller, so that a test can set it.
///
/// # Errors
///
/// The [`Refusal`] of the first check that failed.
pub fn verify(
    headers: &HeaderMap,
    body: &[u8],
    keys: &SigningKeys,
    now: DateTime<Utc>,
) -> Result<Notification, Refusal> {
    let id = one(headers, "webhook-id")?;
    let timestamp = one(headers, "webhook-timestamp")?;
    let signatures = one(headers, "webhook-signature")?;
    if !is_event_id(id) {
        return Err(Refusal::Id);
    }
    let seconds = seconds(timestamp).ok_or(Refusal::Timestamp)?;
    if !within_window(seconds, now) {
        return Err(Refusal::Window);
    }
    if !signed(keys, id, timestamp, body, signatures) {
        return Err(Refusal::Signature);
    }
    let read: Body = serde_json::from_slice(body).map_err(|_| Refusal::Body)?;
    Ok(Notification {
        event_id: id.to_owned(),
        payment: read.data.payment_id,
    })
}

/// A header's one value, in visible ASCII.
fn one<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, Refusal> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(Refusal::Headers)?;
    if values.next().is_some() {
        return Err(Refusal::Headers);
    }
    value.to_str().map_err(|_| Refusal::Headers)
}

/// Whether an ID has the one shape that `psp_notifications_event_id_check` allows: 1 to
/// 255 of `[0-9A-Za-z_-]`, so it holds no `.` (ADR-0030).
fn is_event_id(id: &str) -> bool {
    (1..=255).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// A timestamp's seconds, from ASCII digits alone, so with no sign, that fit in 64 bits.
fn seconds(timestamp: &str) -> Option<i64> {
    if timestamp.is_empty() || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    timestamp.parse().ok()
}

/// Whether `seconds` is at most [`TOLERANCE_SECONDS`] from `now`, either way, in whole
/// seconds, with both ends in, as in the reference library.
fn within_window(seconds: i64, now: DateTime<Utc>) -> bool {
    let now = now.timestamp();
    (now.saturating_sub(TOLERANCE_SECONDS)..=now.saturating_add(TOLERANCE_SECONDS))
        .contains(&seconds)
}

/// Whether one entry of the signature list matches the MAC of one key over the ID and the
/// timestamp as received and the body's bytes. `Mac::verify` compares the two through
/// `CtOutput`, in constant time, so that how long a comparison takes says nothing of how
/// much of a guessed tag is right (ADR-0030). An entry that is not `v1,` and the base64 of
/// 32 bytes is skipped, as the reference library skips any entry that does not match.
fn signed(keys: &SigningKeys, id: &str, timestamp: &str, body: &[u8], listed: &str) -> bool {
    let macs: Vec<HmacSha256> = keys
        .0
        .iter()
        .map(|key| {
            let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
            mac.update(id.as_bytes());
            mac.update(b".");
            mac.update(timestamp.as_bytes());
            mac.update(b".");
            mac.update(body);
            mac
        })
        .collect();
    listed
        .split(' ')
        .filter_map(tag)
        .any(|tag| macs.iter().any(|mac| mac.clone().verify(&tag).is_ok()))
}

/// The MAC that one entry of the list carries: `v1,` and the base64 of 32 bytes.
fn tag(entry: &str) -> Option<Output<HmacSha256>> {
    let encoded = entry.strip_prefix("v1,")?;
    let bytes = STANDARD.decode(encoded).ok()?;
    Output::<HmacSha256>::try_from(bytes.as_slice()).ok()
}

/// What the service reads of a verified body: the payment that it names. Other fields
/// are ignored (ADR-0043).
#[derive(Deserialize)]
struct Body {
    data: Subject,
}

#[derive(Deserialize)]
struct Subject {
    payment_id: Uuid,
}

/// What recording a notification found about the payment that it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// A payment that the service holds: a catch-up follows the answer (ADR-0032).
    Known(PaymentId),
    /// A payment that the service does not hold. The notification is kept and flagged,
    /// and nothing follows, since no retry can change that (ADR-0032).
    Unknown(Uuid),
}

/// Why a notification could not be recorded. The route answers 503, and the PSP sends
/// it again, which a copy recorded meanwhile answers (ADR-0032).
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// A statement before the `COMMIT` failed, so nothing was recorded.
    #[error("the notification could not be recorded")]
    Database(#[source] sqlx::Error),
    /// The `COMMIT` failed, so the notification may or may not be recorded (ADR-0014).
    #[error("the notification may or may not have been recorded")]
    NotConfirmed(#[source] CommitError),
}

/// Records a verified notification once, by its PSP and its event ID (ADR-0031,
/// ADR-0032, ADR-0043), in a short transaction of its own on the system role's `pool`:
/// one insert, which also reads whether the payment exists, committed through the one
/// function of ADR-0014. A copy that committed first is no error. Its key refuses the
/// insert with a `23505`, the transaction is dropped, and the copy is found: the payment
/// that it names is the one recorded.
///
/// # Errors
///
/// See [`RecordError`].
pub async fn record(
    pool: &PgPool,
    psp: Psp,
    notification: &Notification,
) -> Result<Recorded, RecordError> {
    let mut tx = pool.begin().await.map_err(RecordError::Database)?;
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO psp_notifications (psp, event_id, payment_id)
           VALUES ($1, $2, $3)
           RETURNING EXISTS (SELECT 1 FROM payments WHERE id = $3) AS "known!""#,
        psp.as_str(),
        notification.event_id,
        notification.payment
    )
    .fetch_one(&mut *tx)
    .await;
    match inserted {
        Ok(known) => {
            commit(tx).await.map_err(RecordError::NotConfirmed)?;
            Ok(recorded(notification.payment, known))
        }
        Err(failed) => {
            // The failed insert aborted the transaction: drop it (ADR-0014).
            drop(tx);
            let (code, constraint) = code_and_constraint(&failed);
            if !is_copy(code.as_deref(), constraint) {
                return Err(RecordError::Database(failed));
            }
            let copy = sqlx::query!(
                r#"SELECT n.payment_id,
                          EXISTS (SELECT 1 FROM payments p WHERE p.id = n.payment_id) AS "known!"
                     FROM psp_notifications n
                    WHERE n.psp = $1 AND n.event_id = $2"#,
                psp.as_str(),
                notification.event_id
            )
            .fetch_one(pool)
            .await
            .map_err(RecordError::Database)?;
            Ok(recorded(copy.payment_id, copy.known))
        }
    }
}

/// Whether a failed insert met a copy of the notification that committed first: a
/// `23505` on the primary key `(psp, event_id)`.
fn is_copy(code: Option<&str>, constraint: Option<&str>) -> bool {
    code == Some("23505") && constraint == Some("psp_notifications_pkey")
}

fn recorded(payment: Uuid, known: bool) -> Recorded {
    if known {
        Recorded::Known(
            PaymentId::try_from(payment).expect("payments_id_check allows only UUIDv7 IDs"),
        )
    } else {
        Recorded::Unknown(payment)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use mock_psp::signing::SigningKey;

    use super::*;

    /// The specification's test vector: `test_sign` in its Rust reference library.
    const VECTOR_KEY: &str = "whsec_C2FVsBQIhrscChlQIMV+b5sSYspob7oD";
    const VECTOR_ID: &str = "msg_27UH4WbU6Z5A5EzD8u03UvzRbpk";
    const VECTOR_TIMESTAMP: i64 = 1_649_367_553;
    const VECTOR_BODY: &[u8] = br#"{"email":"test@example.com","username":"test_user"}"#;
    const VECTOR_SIGNATURE: &str = "v1,tZ1I4/hDygAJgO5TYxiSd6Sd0kDW6hPenDe+bTa3Kkw=";

    const ID: &str = "msg_0196a5b4c3d27e8f9a0b1c2d3e4f5a6b";
    const PAYMENT: &str = "0196a5b4-c3d2-7e8f-9a0b-1c2d3e4f5a6b";
    const BODY: &[u8] = br#"{"type":"payment.updated","timestamp":"2026-01-01T00:00:00.123456Z","data":{"payment_id":"0196a5b4-c3d2-7e8f-9a0b-1c2d3e4f5a6b"}}"#;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("a time in range")
    }

    /// The service's clock in these tests.
    fn now() -> DateTime<Utc> {
        at(1_767_225_600)
    }

    fn keys_of(signers: &[&SigningKey]) -> SigningKeys {
        let listed: Vec<String> = signers.iter().map(|key| key.written()).collect();
        SigningKeys::parse(&listed.join(" ")).expect("keys that the fake made")
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

    /// A notification as the fake sends it: signed by `key` at `timestamp`.
    fn signed_by(key: &SigningKey, id: &str, timestamp: i64, body: &[u8]) -> HeaderMap {
        headers(id, &timestamp.to_string(), &key.sign(id, timestamp, body))
    }

    fn read(id: &str) -> Notification {
        Notification {
            event_id: id.to_owned(),
            payment: Uuid::parse_str(PAYMENT).expect("a UUID"),
        }
    }

    #[test]
    fn the_specifications_test_vector_passes_every_check_before_the_body() {
        let keys = SigningKeys::parse(VECTOR_KEY).expect("the vector's key, of 24 bytes");
        let headers = headers(VECTOR_ID, &VECTOR_TIMESTAMP.to_string(), VECTOR_SIGNATURE);
        // The vector's payload names no payment, so only the last check refuses it.
        assert_eq!(
            verify(&headers, VECTOR_BODY, &keys, at(VECTOR_TIMESTAMP)),
            Err(Refusal::Body)
        );
        let wrong = VECTOR_SIGNATURE.replace("Kkw=", "Kkx=");
        let headers = self::headers(VECTOR_ID, &VECTOR_TIMESTAMP.to_string(), &wrong);
        assert_eq!(
            verify(&headers, VECTOR_BODY, &keys, at(VECTOR_TIMESTAMP)),
            Err(Refusal::Signature)
        );
    }

    #[test]
    fn a_notification_that_the_fake_signed_is_read() {
        let key = SigningKey::generate();
        let headers = signed_by(&key, ID, now().timestamp(), BODY);
        assert_eq!(
            verify(&headers, BODY, &keys_of(&[&key]), now()),
            Ok(read(ID))
        );
    }

    #[test]
    fn every_signed_byte_counts() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let seconds = now().timestamp();
        let signature = key.sign(ID, seconds, BODY);
        let mut changed = BODY.to_vec();
        changed.push(b' ');
        // One byte more in the body, another ID or another time, under the same signature.
        for (id, timestamp, body) in [
            (ID, seconds, changed.as_slice()),
            ("msg_other", seconds, BODY),
            (ID, seconds - 1, BODY),
        ] {
            let headers = headers(id, &timestamp.to_string(), &signature);
            assert_eq!(
                verify(&headers, body, &keys, now()),
                Err(Refusal::Signature),
                "{id} {timestamp}"
            );
        }
        // And another key.
        let headers = signed_by(&SigningKey::generate(), ID, seconds, BODY);
        assert_eq!(
            verify(&headers, BODY, &keys, now()),
            Err(Refusal::Signature)
        );
    }

    #[test]
    fn the_window_holds_its_ends_and_nothing_beyond() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let seconds = now().timestamp();
        for (offset, verified) in [
            (-TOLERANCE_SECONDS, true),
            (TOLERANCE_SECONDS, true),
            (-TOLERANCE_SECONDS - 1, false),
            (TOLERANCE_SECONDS + 1, false),
            (0, true),
        ] {
            let headers = signed_by(&key, ID, seconds + offset, BODY);
            let expected = if verified {
                Ok(read(ID))
            } else {
                Err(Refusal::Window)
            };
            assert_eq!(verify(&headers, BODY, &keys, now()), expected, "{offset}");
        }
        // A second into the clock's second changes nothing: the window is in whole seconds.
        let later =
            DateTime::from_timestamp(now().timestamp(), 999_000_000).expect("a time in range");
        let headers = signed_by(&key, ID, seconds - TOLERANCE_SECONDS, BODY);
        assert_eq!(verify(&headers, BODY, &keys, later), Ok(read(ID)));
    }

    #[test]
    fn during_a_rotation_either_key_verifies() {
        let (old, new) = (SigningKey::generate(), SigningKey::generate());
        let seconds = now().timestamp();
        let both = format!(
            "{} {}",
            old.sign(ID, seconds, BODY),
            new.sign(ID, seconds, BODY)
        );
        let timestamp = seconds.to_string();
        // The PSP signs with both, and the service holds either or both.
        for held in [vec![&old], vec![&new], vec![&old, &new]] {
            let headers = headers(ID, &timestamp, &both);
            assert_eq!(verify(&headers, BODY, &keys_of(&held), now()), Ok(read(ID)));
        }
        // The PSP signs with one, and the service holds both.
        for signer in [&old, &new] {
            let headers = signed_by(signer, ID, seconds, BODY);
            assert_eq!(
                verify(&headers, BODY, &keys_of(&[&old, &new]), now()),
                Ok(read(ID))
            );
        }
        // Neither key is held.
        let headers = headers(ID, &timestamp, &both);
        assert_eq!(
            verify(&headers, BODY, &keys_of(&[&SigningKey::generate()]), now()),
            Err(Refusal::Signature)
        );
    }

    #[test]
    fn each_header_appears_once_in_visible_ascii() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let good = signed_by(&key, ID, now().timestamp(), BODY);
        for name in ["webhook-id", "webhook-timestamp", "webhook-signature"] {
            let mut missing = good.clone();
            missing.remove(name);
            assert_eq!(
                verify(&missing, BODY, &keys, now()),
                Err(Refusal::Headers),
                "{name} missing"
            );
            let mut repeated = good.clone();
            let value = good.get(name).expect("a header").clone();
            repeated.append(name, value);
            assert_eq!(
                verify(&repeated, BODY, &keys, now()),
                Err(Refusal::Headers),
                "{name} repeated"
            );
            let mut opaque = good.clone();
            opaque.insert(
                name,
                HeaderValue::from_bytes(b"caf\xc3\xa9").expect("an opaque value"),
            );
            assert_eq!(
                verify(&opaque, BODY, &keys, now()),
                Err(Refusal::Headers),
                "{name} not ASCII"
            );
        }
    }

    #[test]
    fn an_id_has_one_shape_without_a_dot() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let seconds = now().timestamp();
        let longest = "a".repeat(255);
        let headers = signed_by(&key, &longest, seconds, BODY);
        assert_eq!(verify(&headers, BODY, &keys, now()), Ok(read(&longest)));
        // Each is signed as if it were valid, so only the shape refuses it.
        for id in ["", "msg.1", "msg 1", "msg/1", "msg+1", &"a".repeat(256)] {
            let headers = signed_by(&key, id, seconds, BODY);
            assert_eq!(
                verify(&headers, BODY, &keys, now()),
                Err(Refusal::Id),
                "{id:?}"
            );
        }
    }

    #[test]
    fn a_timestamp_is_digits_alone() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let seconds = now().timestamp();
        for timestamp in [
            String::new(),
            format!("+{seconds}"),
            format!("{seconds}.0"),
            format!(" {seconds}"),
            format!("{seconds}e0"),
            "-1".to_owned(),
            "99999999999999999999".to_owned(),
        ] {
            let signature = format!("v1,{}", STANDARD.encode(mac(&key, ID, &timestamp, BODY)));
            let headers = headers(ID, &timestamp, &signature);
            assert_eq!(
                verify(&headers, BODY, &keys, now()),
                Err(Refusal::Timestamp),
                "{timestamp:?}"
            );
        }
    }

    /// The MAC of a key over an ID, a timestamp written as given, and a body, for
    /// timestamps that the fake's signer cannot write.
    fn mac(key: &SigningKey, id: &str, timestamp: &str, body: &[u8]) -> Vec<u8> {
        use hmac::{Hmac, KeyInit, Mac};
        let written = key.written();
        let secret = STANDARD
            .decode(written.trim_start_matches("whsec_"))
            .expect("a written key");
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&secret).expect("any length");
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(body);
        mac.finalize().into_bytes().to_vec()
    }

    #[test]
    fn the_mac_covers_the_timestamp_as_received() {
        // Leading zeros are digits, and the MAC covers them as sent.
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let padded = format!("0{}", now().timestamp());
        let signature = format!("v1,{}", STANDARD.encode(mac(&key, ID, &padded, BODY)));
        let headers = headers(ID, &padded, &signature);
        assert_eq!(verify(&headers, BODY, &keys, now()), Ok(read(ID)));
        // A signature over the number without its zero does not cover what was received.
        let unpadded = key.sign(ID, now().timestamp(), BODY);
        let headers = self::headers(ID, &padded, &unpadded);
        assert_eq!(
            verify(&headers, BODY, &keys, now()),
            Err(Refusal::Signature)
        );
    }

    #[test]
    fn entries_that_are_no_v1_signature_are_skipped() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let seconds = now().timestamp();
        let timestamp = seconds.to_string();
        let good = key.sign(ID, seconds, BODY);
        let tag = good.trim_start_matches("v1,");
        // As in the reference library's test of malformed signatures.
        for listed in [
            format!("v1 {good}"),
            format!(",,, v1,not,base64  {good}"),
            format!("v1, {good}"),
            format!("v1a,{tag} {good}"),
        ] {
            let headers = headers(ID, &timestamp, &listed);
            assert_eq!(
                verify(&headers, BODY, &keys, now()),
                Ok(read(ID)),
                "{listed}"
            );
        }
        let short = STANDARD.encode(&mac(&key, ID, &timestamp, BODY)[..31]);
        for listed in [
            format!("v2,{tag}"),
            format!("V1,{tag}"),
            format!("v1a,{tag}"),
            format!("v1,{short}"),
            tag.to_owned(),
            String::new(),
        ] {
            let headers = headers(ID, &timestamp, &listed);
            assert_eq!(
                verify(&headers, BODY, &keys, now()),
                Err(Refusal::Signature),
                "{listed}"
            );
        }
    }

    #[test]
    fn a_verified_body_names_its_payment_and_nothing_else_is_checked() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let seconds = now().timestamp();
        let unreadable: [&[u8]; 5] = [
            b"not json",
            br#"{"data":{}}"#,
            br#"{"type":"payment.updated"}"#,
            br#"{"data":{"payment_id":"not a uuid"}}"#,
            br#"{"data":{"payment_id":7}}"#,
        ];
        for body in unreadable {
            let headers = signed_by(&key, ID, seconds, body);
            assert_eq!(
                verify(&headers, body, &keys, now()),
                Err(Refusal::Body),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        // Another type, no timestamp and fields that the service does not know.
        let body = br#"{"type":"anything","data":{"payment_id":"0196a5b4-c3d2-7e8f-9a0b-1c2d3e4f5a6b","more":1},"extra":[]}"#;
        let headers = signed_by(&key, ID, seconds, body);
        assert_eq!(verify(&headers, body, &keys, now()), Ok(read(ID)));
    }

    #[test]
    fn the_first_check_that_fails_refuses_and_the_body_waits_for_the_signature() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let stale = now().timestamp() - TOLERANCE_SECONDS - 1;
        let unsigned = "v1,AAAA";
        // An ID with a dot and a stale time: the ID's shape is checked first.
        let headers = signed_by(&key, "msg.1", stale, BODY);
        assert_eq!(verify(&headers, BODY, &keys, now()), Err(Refusal::Id));
        // A stale time and no valid signature: the window is checked first.
        let headers = self::headers(ID, &stale.to_string(), unsigned);
        assert_eq!(verify(&headers, BODY, &keys, now()), Err(Refusal::Window));
        // A body that is not JSON and no valid signature: the body is never parsed.
        let headers = self::headers(ID, &now().timestamp().to_string(), unsigned);
        assert_eq!(
            verify(&headers, b"not json", &keys, now()),
            Err(Refusal::Signature)
        );
    }

    #[test]
    fn a_list_of_keys_is_written_whsec_and_base64_of_24_to_64_bytes() {
        for length in [24, 32, 64] {
            let written = format!("whsec_{}", STANDARD.encode(vec![7_u8; length]));
            assert!(SigningKeys::parse(&written).is_ok(), "{length}");
        }
        let two = format!(
            "{}  {}\n",
            SigningKey::generate().written(),
            SigningKey::generate().written()
        );
        assert!(SigningKeys::parse(&two).is_ok());
        for (listed, error) in [
            (String::new(), KeyError::None),
            ("  \n ".to_owned(), KeyError::None),
            (
                format!("whsec_{}", STANDARD.encode([7_u8; 23])),
                KeyError::Length(23),
            ),
            (
                format!("whsec_{}", STANDARD.encode([7_u8; 65])),
                KeyError::Length(65),
            ),
            (STANDARD.encode([7_u8; 32]), KeyError::Prefix),
            ("whsec_not!base64".to_owned(), KeyError::Base64),
            (
                format!("{} whsec_", SigningKey::generate().written()),
                KeyError::Length(0),
            ),
        ] {
            assert_eq!(
                SigningKeys::parse(&listed).unwrap_err(),
                error,
                "{listed:?}"
            );
        }
    }

    #[test]
    fn only_a_23505_on_the_notifications_key_is_a_copy() {
        assert!(is_copy(Some("23505"), Some("psp_notifications_pkey")));
        for (code, constraint) in [
            (Some("23505"), Some("payment_requests_pkey")),
            (Some("23505"), None),
            (Some("23514"), Some("psp_notifications_pkey")),
            (Some("42501"), None),
            (None, Some("psp_notifications_pkey")),
            (None, None),
        ] {
            assert!(!is_copy(code, constraint), "{code:?} {constraint:?}");
        }
    }

    #[test]
    fn keys_print_none_of_their_bytes() {
        let key = SigningKey::generate();
        let keys = keys_of(&[&key]);
        let printed = format!("{keys:?}");
        assert_eq!(printed, "SigningKeys(..)");
        let error = SigningKeys::parse(&format!("{} nope", key.written())).unwrap_err();
        assert!(!error.to_string().contains("nope"), "{error}");
    }
}
