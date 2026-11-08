// Author: Nicola Antonello

//! The JSON that the fake and the server exchange (ADR-0029, ADR-0031, ADR-0032).
//!
//! These are the fake's types. The server reads and writes the same format with types of
//! its own: the two share the format, not code.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The cap of a ledger amount (ADR-0003).
const CAP: i64 = 1_000_000_000_000_000;

/// What the service asks of the PSP (ADR-0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Authorize,
    Capture,
    Refund,
    Void,
}

/// A request, as `POST /v1/requests` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The service's `RequestId`: the request's idempotency key, and its reference on
    /// every send (ADR-0028).
    pub id: Uuid,
    pub payment_id: Uuid,
    pub kind: RequestKind,
    /// In minor units. A void asks for nothing.
    pub amount: i64,
    /// An ISO 4217 code.
    pub currency: String,
}

impl Request {
    /// Whether the PSP could act on it: an amount for its kind, up to the cap of a ledger
    /// amount, and a currency of three capital letters. The fake answers anything else
    /// with a 400, as malformed (ADR-0032).
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        let amount = match self.kind {
            RequestKind::Void => self.amount == 0,
            RequestKind::Authorize | RequestKind::Capture | RequestKind::Refund => {
                (1..=CAP).contains(&self.amount)
            }
        };
        let currency =
            self.currency.len() == 3 && self.currency.bytes().all(|b| b.is_ascii_uppercase());
        amount && currency
    }
}

/// The answer to a request that the PSP holds: it decides later (ADR-0028).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Received {
    pub id: Uuid,
    /// Always `received`.
    pub status: String,
}

impl Received {
    #[must_use]
    pub fn of(id: Uuid) -> Self {
        Self {
            id,
            status: "received".to_owned(),
        }
    }
}

/// A payment's history (ADR-0031): its facts in the PSP's order, and every request that
/// the PSP took for it, decided or not, which the sender looks up before it sends a
/// request again (ADR-0028).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub payment_id: Uuid,
    pub facts: Vec<Fact>,
    pub requests: Vec<Request>,
}

/// One fact, at its position, counted from 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    /// The PSP's own ID for the fact, unique across the PSP.
    pub id: String,
    pub position: u32,
    #[serde(flatten)]
    pub detail: Detail,
}

/// What happened, in the PSP's words: the past tense of an outcome, never one of the
/// service's commands. The catch-up maps each to the step that it makes, or to a
/// refusal (ADR-0037).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Detail {
    /// An authorization, for at most the amount asked (ADR-0016), and when it lapses.
    Authorized {
        request_id: Uuid,
        amount: i64,
        expires_at: DateTime<Utc>,
    },
    Declined {
        request_id: Uuid,
    },
    Captured {
        request_id: Uuid,
        amount: i64,
        captured_at: DateTime<Utc>,
    },
    CaptureRefused {
        request_id: Uuid,
    },
    Voided {
        request_id: Uuid,
    },
    VoidRefused {
        request_id: Uuid,
    },
    /// The authorization lapsed, which only the PSP decides.
    Expired,
    /// The capture settled, which only the PSP decides.
    Settled,
    Refunded {
        request_id: Uuid,
        amount: i64,
        refunded_at: DateTime<Utc>,
    },
    RefundRefused {
        request_id: Uuid,
    },
}

/// The one type of event: a payment changed (ADR-0031).
pub const EVENT_TYPE: &str = "payment.updated";

/// The body of a notification: thin, in the payload structure of Standard Webhooks, and
/// naming the payment but nothing that the PSP decided (ADR-0030, ADR-0031).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    #[serde(rename = "type")]
    pub event_type: String,
    /// When the change happened, which may be long before a delivery.
    pub timestamp: DateTime<Utc>,
    pub data: Subject,
}

impl Notification {
    /// That this payment changed at this time.
    #[must_use]
    pub fn payment_updated(payment_id: Uuid, timestamp: DateTime<Utc>) -> Self {
        Self {
            event_type: EVENT_TYPE.to_owned(),
            timestamp,
            data: Subject { payment_id },
        }
    }

    /// The bytes that are sent and signed.
    #[must_use]
    pub fn payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a notification is plain data")
    }
}

/// What a notification is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    pub payment_id: Uuid,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const REQUEST: Uuid = Uuid::from_u128(1);
    const PAYMENT: Uuid = Uuid::from_u128(2);

    /// 2026-01-01T00:00:00.123456Z.
    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp_micros(1_767_225_600_123_456).expect("a time in range")
    }

    /// The JSON of a fact at position 1 with this detail, and the fact read back from it.
    fn as_json(detail: Detail) -> Value {
        let fact = Fact {
            id: "fct_1".to_owned(),
            position: 1,
            detail,
        };
        let value = serde_json::to_value(&fact).expect("a fact is plain data");
        let read: Fact = serde_json::from_value(value.clone()).expect("a fact reads back");
        assert_eq!(read, fact);
        value
    }

    #[test]
    fn each_fact_reads_in_the_psps_words_with_the_fields_of_its_kind() {
        let request = "00000000-0000-0000-0000-000000000001";
        let time = "2026-01-01T00:00:00.123456Z";
        let request_id = REQUEST;
        for (detail, expected) in [
            (
                Detail::Authorized {
                    request_id,
                    amount: 8_000,
                    expires_at: at(),
                },
                json!({"kind": "authorized", "request_id": request, "amount": 8_000,
                       "expires_at": time}),
            ),
            (
                Detail::Declined { request_id },
                json!({"kind": "declined", "request_id": request}),
            ),
            (
                Detail::Captured {
                    request_id,
                    amount: 8_000,
                    captured_at: at(),
                },
                json!({"kind": "captured", "request_id": request, "amount": 8_000,
                       "captured_at": time}),
            ),
            (
                Detail::CaptureRefused { request_id },
                json!({"kind": "capture_refused", "request_id": request}),
            ),
            (
                Detail::Voided { request_id },
                json!({"kind": "voided", "request_id": request}),
            ),
            (
                Detail::VoidRefused { request_id },
                json!({"kind": "void_refused", "request_id": request}),
            ),
            (Detail::Expired, json!({"kind": "expired"})),
            (Detail::Settled, json!({"kind": "settled"})),
            (
                Detail::Refunded {
                    request_id,
                    amount: 100,
                    refunded_at: at(),
                },
                json!({"kind": "refunded", "request_id": request, "amount": 100,
                       "refunded_at": time}),
            ),
            (
                Detail::RefundRefused { request_id },
                json!({"kind": "refund_refused", "request_id": request}),
            ),
        ] {
            let mut expected = expected;
            let fields = expected.as_object_mut().expect("an object");
            fields.insert("id".to_owned(), json!("fct_1"));
            fields.insert("position".to_owned(), json!(1));
            assert_eq!(as_json(detail), expected);
        }
    }

    #[test]
    fn a_history_lists_facts_then_requests() {
        let history = History {
            payment_id: PAYMENT,
            facts: vec![],
            requests: vec![Request {
                id: REQUEST,
                payment_id: PAYMENT,
                kind: RequestKind::Authorize,
                amount: 10_000,
                currency: "EUR".to_owned(),
            }],
        };
        assert_eq!(
            serde_json::to_string(&history).expect("plain data"),
            r#"{"payment_id":"00000000-0000-0000-0000-000000000002","facts":[],"requests":[{"id":"00000000-0000-0000-0000-000000000001","payment_id":"00000000-0000-0000-0000-000000000002","kind":"authorize","amount":10000,"currency":"EUR"}]}"#
        );
    }

    #[test]
    fn a_notification_is_thin() {
        let payload = Notification::payment_updated(PAYMENT, at()).payload();
        assert_eq!(
            String::from_utf8(payload).expect("JSON is UTF-8"),
            r#"{"type":"payment.updated","timestamp":"2026-01-01T00:00:00.123456Z","data":{"payment_id":"00000000-0000-0000-0000-000000000002"}}"#
        );
    }

    #[test]
    fn a_request_is_received_and_refuses_fields_it_does_not_know() {
        assert_eq!(
            serde_json::to_value(Received::of(REQUEST)).expect("plain data"),
            json!({"id": "00000000-0000-0000-0000-000000000001", "status": "received"})
        );
        let unknown = json!({"id": REQUEST, "payment_id": PAYMENT, "kind": "capture",
                             "amount": 1, "currency": "EUR", "captured_at": "now"});
        assert!(serde_json::from_value::<Request>(unknown).is_err());
    }

    #[test]
    fn a_request_has_an_amount_for_its_kind_and_a_currency() {
        let request = |kind, amount, currency: &str| Request {
            id: REQUEST,
            payment_id: PAYMENT,
            kind,
            amount,
            currency: currency.to_owned(),
        };
        for good in [
            request(RequestKind::Authorize, 1, "EUR"),
            request(RequestKind::Capture, CAP, "USD"),
            request(RequestKind::Refund, 1, "JPY"),
            request(RequestKind::Void, 0, "EUR"),
        ] {
            assert!(good.is_well_formed(), "{good:?}");
        }
        for bad in [
            request(RequestKind::Authorize, 0, "EUR"),
            request(RequestKind::Capture, CAP + 1, "EUR"),
            request(RequestKind::Refund, -1, "EUR"),
            request(RequestKind::Void, 1, "EUR"),
            request(RequestKind::Capture, 1, "eur"),
            request(RequestKind::Capture, 1, "EURO"),
            request(RequestKind::Capture, 1, ""),
        ] {
            assert!(!bad.is_well_formed(), "{bad:?}");
        }
    }
}
