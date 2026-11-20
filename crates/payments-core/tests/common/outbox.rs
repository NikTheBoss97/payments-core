// Author: Nicola Antonello

//! The outbox's rows as the catch-up writes them, for the relay's tests (ADR-0049,
//! ADR-0053, ADR-0057): the PSP's fact about a payment, its event, and one delivery of the
//! event to each endpoint, in one transaction of the system role. Every fact is an
//! authorization, since the table does not judge the order of kinds (ADR-0037).

use payments_core::ids::{DeliveryId, EndpointId, EventId};
use payments_core::relay::Delivery;
use payments_core::signing::MasterKey;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::api::new_master_key;
use super::payments::{NewPayment, Step, register_merchant};
use super::runtime_transaction;

/// A master key made now, as the relay's tests use it.
pub fn master() -> MasterKey {
    MasterKey::parse(&new_master_key()).expect("a key that new_master_key wrote")
}

/// A new merchant and its payment, committed with the payment's first step. Returns the
/// merchant and the payment.
pub async fn new_payment(pool: &PgPool) -> sqlx::Result<(Uuid, Uuid)> {
    let mut tx = runtime_transaction(pool).await?;
    let merchant = register_merchant(&mut tx).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    Step::first(payment.id).insert(&mut tx).await?;
    tx.commit().await?;
    Ok((merchant, payment.id))
}

/// Registers an endpoint of `merchant` at `url` as the system role, past the route's
/// checks, so that a test can store what the route would refuse.
pub async fn insert_endpoint(
    pool: &PgPool,
    endpoint: EndpointId,
    merchant: Uuid,
    url: &str,
) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(pool).await?;
    sqlx::query!(
        "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
        endpoint.get(),
        merchant,
        url
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Writes, in the transaction of `conn`, the PSP's fact at `position` of `payment`, its
/// event, and the event's deliveries to these endpoints of `merchant`. Returns the
/// deliveries as the relay learns of them.
pub async fn write_event(
    conn: &mut PgConnection,
    merchant: Uuid,
    payment: Uuid,
    position: i32,
    event: EventId,
    deliveries: &[(DeliveryId, EndpointId)],
) -> sqlx::Result<Vec<Delivery>> {
    let prev_position = position
        .checked_sub(1)
        .expect("a fact's position is at least 1")
        .max(1);
    sqlx::query!(
        "INSERT INTO payment_facts \
             (payment_id, position, prev_position, psp, psp_fact_id, kind, amount) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        payment,
        position,
        prev_position,
        "mock",
        format!("fct_{}", event.get().simple()),
        "authorize",
        100_i64
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO payment_events (id, merchant_id, payment_id, position) \
         VALUES ($1, $2, $3, $4)",
        event.get(),
        merchant,
        payment,
        position
    )
    .execute(&mut *conn)
    .await?;
    for (delivery, endpoint) in deliveries {
        sqlx::query!(
            "INSERT INTO webhook_deliveries (id, event_id, endpoint_id, merchant_id) \
             VALUES ($1, $2, $3, $4)",
            delivery.get(),
            event.get(),
            endpoint.get(),
            merchant
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(deliveries
        .iter()
        .map(|&(id, endpoint)| Delivery {
            id,
            endpoint,
            event,
        })
        .collect())
}

/// [`write_event`] in a transaction of its own, committed.
pub async fn event(
    pool: &PgPool,
    merchant: Uuid,
    payment: Uuid,
    position: i32,
    event: EventId,
    deliveries: &[(DeliveryId, EndpointId)],
) -> sqlx::Result<Vec<Delivery>> {
    let mut tx = runtime_transaction(pool).await?;
    let written = write_event(&mut tx, merchant, payment, position, event, deliveries).await?;
    tx.commit().await?;
    Ok(written)
}

/// Records the success of a delivery, as the relay does.
pub async fn succeed(pool: &PgPool, delivery: DeliveryId) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(pool).await?;
    sqlx::query!(
        "INSERT INTO webhook_delivery_successes (delivery_id) VALUES ($1)",
        delivery.get()
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Disables an endpoint for `reason`, as the operator or the relay does.
pub async fn disable(pool: &PgPool, endpoint: EndpointId, reason: &str) -> sqlx::Result<()> {
    let mut tx = runtime_transaction(pool).await?;
    sqlx::query!(
        "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason) VALUES ($1, $2)",
        endpoint.get(),
        reason
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}
