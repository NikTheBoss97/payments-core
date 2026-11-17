// Author: Nicola Antonello

//! An event's body (ADR-0049): what a merchant's endpoint receives, in Standard Webhooks'
//! payload structure. It is built from rows that never change, the event and its fact's
//! decision, so every attempt sends the same bytes, and each attempt signs them afresh
//! (ADR-0051).

use serde::Serialize;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use uuid::Uuid;

use crate::ids::EventId;
use crate::times::BusinessTime;

/// The one type of event (ADR-0049).
const PAYMENT_UPDATED: &str = "payment.updated";

/// Why an event's body could not be read. Nothing was sent, and a later attempt reads it
/// again.
#[derive(Debug, thiserror::Error)]
#[error("the body of event {event} could not be read")]
pub struct BodyError {
    event: EventId,
    #[source]
    source: sqlx::Error,
}

/// The body of an event: the bytes that every attempt to deliver it sends (ADR-0049), or
/// `None` when no event has this ID.
///
/// # Errors
///
/// [`BodyError`] when the event's rows cannot be read.
pub async fn body(pool: &PgPool, event: EventId) -> Result<Option<Vec<u8>>, BodyError> {
    // A fact decides at most one request (payment_request_decisions_fact_key), so the join
    // gives at most one row.
    let row = sqlx::query!(
        r#"SELECT e.payment_id, e.recorded_at, d.request_id AS "request_id?"
             FROM payment_events e
             LEFT JOIN payment_request_decisions d
                    ON d.payment_id = e.payment_id AND d.position = e.position
            WHERE e.id = $1"#,
        event.get()
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| BodyError { event, source })?;
    Ok(row.map(|row| written(row.payment_id, row.request_id, row.recorded_at)))
}

/// What a merchant receives, in ADR-0049's order.
#[derive(Serialize)]
struct Body {
    #[serde(rename = "type")]
    kind: &'static str,
    /// The event's `recorded_at`, written as the API writes times.
    timestamp: String,
    data: Data,
}

#[derive(Serialize)]
struct Data {
    payment_id: Uuid,
    /// Only when the event's fact decides a request.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<Uuid>,
}

/// The body's bytes.
fn written(payment: Uuid, request: Option<Uuid>, recorded_at: OffsetDateTime) -> Vec<u8> {
    let timestamp = BusinessTime::new(recorded_at)
        .expect("a time that PostgreSQL stored is one that it stores")
        .written();
    let body = Body {
        kind: PAYMENT_UPDATED,
        timestamp,
        data: Data {
            payment_id: payment,
            request_id: request,
        },
    };
    serde_json::to_vec(&body).expect("strings and IDs always serialize")
}
