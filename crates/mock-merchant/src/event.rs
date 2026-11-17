// Author: Nicola Antonello

//! What a delivery's body holds: one thin event (ADR-0049), read strictly, so that a body
//! of any other shape fails a test (ADR-0056).

use serde::{Deserialize, Deserializer};
use time::OffsetDateTime;
use uuid::Uuid;

/// One event, as the server sends it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: Kind,
    /// When the event was recorded: its `recorded_at`.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub data: Data,
}

/// The one type of event (ADR-0049).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Kind {
    #[serde(rename = "payment.updated")]
    PaymentUpdated,
}

/// What an event names: its payment, and the request that its fact decides, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Data {
    pub payment_id: Uuid,
    /// Absent unless the fact decides a request: `null` is refused (ADR-0049).
    #[serde(default, deserialize_with = "present")]
    pub request_id: Option<Uuid>,
}

/// A field that holds a value whenever it appears.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Uuid>, D::Error> {
    Uuid::deserialize(deserializer).map(Some)
}

impl Event {
    /// Reads a body as one event.
    ///
    /// # Errors
    ///
    /// The body is not JSON of this shape.
    pub fn read(body: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(body)
    }
}

#[cfg(test)]
mod tests {
    use time::{Date, Month};

    use super::*;

    const PAYMENT: Uuid = Uuid::from_u128(0x0196_f6a0_0000_7000_8000_0000_0000_0001);
    const REQUEST: Uuid = Uuid::from_u128(0x0196_f6a0_0000_7000_8000_0000_0000_0002);

    fn utc(hour: u8, minute: u8, second: u8, micro: u32) -> OffsetDateTime {
        Date::from_calendar_date(2026, Month::November, 16)
            .and_then(|day| day.with_hms_micro(hour, minute, second, micro))
            .expect("a valid time")
            .assume_utc()
    }

    #[test]
    fn reads_an_event_that_decides_a_request_and_one_that_does_not() {
        for (body, expected) in [
            (
                r#"{"type":"payment.updated","timestamp":"2026-11-16T10:20:30.123456Z","data":{"payment_id":"0196f6a0-0000-7000-8000-000000000001","request_id":"0196f6a0-0000-7000-8000-000000000002"}}"#,
                Event {
                    kind: Kind::PaymentUpdated,
                    timestamp: utc(10, 20, 30, 123_456),
                    data: Data {
                        payment_id: PAYMENT,
                        request_id: Some(REQUEST),
                    },
                },
            ),
            (
                r#"{"type":"payment.updated","timestamp":"2026-11-16T11:20:30+01:00","data":{"payment_id":"0196f6a0-0000-7000-8000-000000000001"}}"#,
                Event {
                    kind: Kind::PaymentUpdated,
                    timestamp: utc(10, 20, 30, 0),
                    data: Data {
                        payment_id: PAYMENT,
                        request_id: None,
                    },
                },
            ),
        ] {
            assert_eq!(Event::read(body.as_bytes()).expect(body), expected);
        }
    }

    #[test]
    fn refuses_any_other_shape() {
        let payment = r#""payment_id":"0196f6a0-0000-7000-8000-000000000001""#;
        let time = r#""timestamp":"2026-11-16T10:20:30Z""#;
        for body in [
            // Another type.
            format!(r#"{{"type":"payment.created",{time},"data":{{{payment}}}}}"#),
            // A field beside the three.
            format!(r#"{{"type":"payment.updated",{time},"data":{{{payment}}},"id":"1"}}"#),
            // A field beside the payment and the request.
            format!(r#"{{"type":"payment.updated",{time},"data":{{{payment},"amount":1}}}}"#),
            // A request that is null rather than absent.
            format!(
                r#"{{"type":"payment.updated",{time},"data":{{{payment},"request_id":null}}}}"#
            ),
            // A payment that is not a UUID.
            format!(r#"{{"type":"payment.updated",{time},"data":{{"payment_id":"1"}}}}"#),
            // A time without its offset, a date alone, and a number.
            format!(
                r#"{{"type":"payment.updated","timestamp":"2026-11-16T10:20:30","data":{{{payment}}}}}"#
            ),
            format!(
                r#"{{"type":"payment.updated","timestamp":"2026-11-16","data":{{{payment}}}}}"#
            ),
            format!(r#"{{"type":"payment.updated","timestamp":1794824430,"data":{{{payment}}}}}"#),
            // No data.
            format!(r#"{{"type":"payment.updated",{time}}}"#),
            // An event followed by more.
            format!(r#"{{"type":"payment.updated",{time},"data":{{{payment}}}}} {{}}"#),
            // Not JSON.
            "payment.updated".to_owned(),
        ] {
            assert!(Event::read(body.as_bytes()).is_err(), "{body}");
        }
    }
}
