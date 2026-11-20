// Author: Nicola Antonello

//! What a merchant's endpoint receives (ADR-0049, ADR-0051): an event's body, read from the
//! rows that the catch-up wrote, signed under the endpoint's secret, and checked by a fake
//! merchant with code of its own (ADR-0054, ADR-0056).
//!
//! Events come from catch-ups against a fake PSP over real HTTP (ADR-0029), as in
//! `writing_events.rs`. The system role reads them, as the relay will. Master keys are made
//! at runtime.

mod common;

use std::error::Error;

use axum::body::Bytes;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::payments::{AUTHORIZED, REQUESTED, create_command, fee_rate};
use common::psp::{client_of, send};
use common::{merchant_id, request_id, runtime_pool, tenant_pool};
use http_body_util::Full;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use mock_merchant::signature::Refusal;
use mock_merchant::{MockMerchant, Verdict};
use mock_psp::MockPsp;
use payments_core::catch_up::{CaughtUp, catch_up};
use payments_core::deliveries::Handover;
use payments_core::events::body;
use payments_core::ids::{EndpointId, EventId, MerchantId, PaymentId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::signing::{MasterKey, Signed, sign};
use payments_core::tenancy::Tenant;
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// The address of a merchant's handler, as the operator would register it.
const URL: &str = "http://localhost:8080/webhooks";

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// A master key of 32 bytes from the operating system.
fn master() -> MasterKey {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's randomness");
    MasterKey::parse(&format!("whsec_{}", STANDARD.encode(bytes))).expect("32 bytes")
}

/// A merchant with its pool, the system role's pool, which catches up and reads events,
/// and a fake PSP.
struct World {
    system: PgPool,
    tenant: PgPool,
    merchant: MerchantId,
    fake: MockPsp,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let system = runtime_pool(pool).await?;
        let merchant = merchant_id(1);
        register_merchant(&system, merchant, fee_rate()).await?;
        Ok(Self {
            tenant: tenant_pool(pool).await?,
            system,
            merchant,
            fake: MockPsp::start().await?,
        })
    }

    fn me(&self) -> Tenant<'_> {
        Tenant::new(&self.tenant, self.merchant)
    }

    /// Payment `n`, for [`REQUESTED`].
    async fn created(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let command = create_command(n, self.merchant);
        create_payment(self.me(), &command).await?;
        Ok(command.id)
    }

    /// Records request `n` as the merchant and posts it to the fake. Returns its ID.
    async fn ask(&self, n: u32, payment: PaymentId, ask: Ask) -> Result<Uuid, Box<dyn Error>> {
        let command = RecordRequest {
            request: request_id(n),
            payment,
            ask,
        };
        record_request(self.me(), &command).await?;
        send(&self.fake, &command).await;
        Ok(command.request.get())
    }

    /// Catches the payment up, and checks that it applied `applied` new facts.
    async fn caught_up(&self, payment: PaymentId, applied: u32) -> TestResult {
        let caught_up = catch_up(
            &self.system,
            &client_of(&self.fake),
            payment,
            &Handover::none(),
        )
        .await?;
        assert_eq!(caught_up, CaughtUp::Current { applied }, "{payment}");
        Ok(())
    }

    /// Payment `n`, authorized by the PSP for [`AUTHORIZED`] and caught up, with the ID of
    /// the request that its authorization decides. Its requests are numbered from `10 * n`.
    async fn authorized(&self, n: u32) -> Result<(PaymentId, Uuid), Box<dyn Error>> {
        let payment = self.created(n).await?;
        let authorize = self
            .ask(10 * n, payment, Ask::Authorize(amount(REQUESTED)))
            .await?;
        self.fake.approve_for(authorize, AUTHORIZED);
        self.caught_up(payment, 1).await?;
        Ok((payment, authorize))
    }

    async fn close(self) {
        self.system.close().await;
        self.tenant.close().await;
    }
}

/// The payment's events, by position: their IDs and when they were recorded.
async fn events_of(
    pool: &PgPool,
    payment: PaymentId,
) -> Result<Vec<(EventId, OffsetDateTime)>, Box<dyn Error>> {
    let rows = sqlx::query!(
        "SELECT id, recorded_at FROM payment_events WHERE payment_id = $1 ORDER BY position",
        payment.get()
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| Ok((EventId::try_from(row.id)?, row.recorded_at)))
        .collect()
}

/// The body that ADR-0049 describes, written out: its fields in that order, the event's
/// `recorded_at` as the API writes times, and `request_id` only when there is one.
fn expected(payment: PaymentId, request: Option<Uuid>, recorded_at: OffsetDateTime) -> String {
    let timestamp = BusinessTime::new(recorded_at)
        .expect("a time that PostgreSQL stores")
        .written();
    match request {
        Some(request) => format!(
            r#"{{"type":"payment.updated","timestamp":"{timestamp}","data":{{"payment_id":"{payment}","request_id":"{request}"}}}}"#
        ),
        None => format!(
            r#"{{"type":"payment.updated","timestamp":"{timestamp}","data":{{"payment_id":"{payment}"}}}}"#
        ),
    }
}

/// Posts a signed attempt to a merchant's endpoint, as the relay will, and returns the
/// answer's status.
async fn post(url: &str, signed: &Signed, body: Vec<u8>) -> Result<StatusCode, Box<dyn Error>> {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let request = Request::post(url)
        .header("content-type", "application/json")
        .header("webhook-id", &signed.id)
        .header("webhook-timestamp", &signed.timestamp)
        .header("webhook-signature", &signed.signature)
        .body(Full::new(Bytes::from(body)))?;
    Ok(client.request(request).await?.status())
}

#[sqlx::test]
async fn each_events_body_names_its_payment_and_the_request_that_its_fact_decides(
    pool: PgPool,
) -> TestResult {
    let world = World::new(&pool).await?;
    // Authorized, captured and settled: the first two facts decide requests, the
    // settlement none.
    let (paid, authorize) = world.authorized(1).await?;
    let capture = world
        .ask(11, paid, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.approve(capture);
    world.fake.settle(paid.get());
    world.caught_up(paid, 2).await?;
    // A lapse decides no request.
    let (lapsed, lapsed_authorize) = world.authorized(2).await?;
    world.fake.expire(lapsed.get());
    world.caught_up(lapsed, 1).await?;
    // A refusal decides its request without a step.
    let (kept, kept_authorize) = world.authorized(3).await?;
    let refused = world
        .ask(31, kept, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.refuse(refused);
    world.caught_up(kept, 1).await?;

    for (payment, requests) in [
        (paid, vec![Some(authorize), Some(capture), None]),
        (lapsed, vec![Some(lapsed_authorize), None]),
        (kept, vec![Some(kept_authorize), Some(refused)]),
    ] {
        let events = events_of(&pool, payment).await?;
        assert_eq!(events.len(), requests.len(), "{payment}");
        for ((event, recorded_at), request) in events.into_iter().zip(requests) {
            let read = body(&world.system, event)
                .await?
                .ok_or("the event exists")?;
            assert_eq!(
                String::from_utf8(read)?,
                expected(payment, request, recorded_at)
            );
        }
    }
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_events_body_is_the_same_at_every_read(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let (payment, _) = world.authorized(1).await?;
    let (event, _) = *events_of(&pool, payment)
        .await?
        .first()
        .ok_or("the authorization's event")?;
    let before = body(&world.system, event).await?;
    assert!(before.is_some(), "the event exists");
    // Later facts change the payment, and never an earlier event's body.
    let capture = world
        .ask(11, payment, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.approve(capture);
    world.caught_up(payment, 1).await?;
    assert_eq!(body(&world.system, event).await?, before);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_event_that_does_not_exist_has_no_body(pool: PgPool) -> TestResult {
    let system = runtime_pool(&pool).await?;
    assert_eq!(body(&system, EventId::now_v7()).await?, None);
    system.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_merchant_takes_an_event_signed_under_its_endpoints_secret(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let master = master();
    let endpoint = EndpointId::now_v7();
    sqlx::query!(
        "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
        endpoint.get(),
        world.merchant.get(),
        URL
    )
    .execute(&world.system)
    .await?;
    let merchant = MockMerchant::start().await?;
    // The secret that registering the endpoint answers, which the merchant keeps
    // (ADR-0050, ADR-0056).
    assert!(
        merchant
            .trust(&master.secret_of(endpoint).written())
            .is_ok(),
        "the merchant keeps the endpoint's secret"
    );

    // The delivery that the authorization's catch-up wrote for the endpoint.
    let (payment, authorize) = world.authorized(1).await?;
    let delivery = sqlx::query!(
        "SELECT d.event_id, d.endpoint_id FROM webhook_deliveries d
           JOIN payment_events e ON e.id = d.event_id
          WHERE e.payment_id = $1",
        payment.get()
    )
    .fetch_one(&world.system)
    .await?;
    assert_eq!(delivery.endpoint_id, endpoint.get());
    let event = EventId::try_from(delivery.event_id)?;
    let sent = body(&world.system, event)
        .await?
        .ok_or("the event exists")?;

    // Signed at the attempt's own time, which the merchant checks against its clock.
    let signed = sign(
        &master.secret_of(endpoint),
        event,
        OffsetDateTime::now_utc(),
        &sent,
    );
    assert_eq!(
        post(merchant.url(), &signed, sent.clone()).await?,
        StatusCode::OK
    );
    let taken = merchant.events();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].id, event.to_string());
    assert_eq!(
        (
            taken[0].event.data.payment_id,
            taken[0].event.data.request_id
        ),
        (payment.get(), Some(authorize))
    );

    // The same bytes signed for another endpoint: refused, and never taken.
    let elsewhere = sign(
        &master.secret_of(EndpointId::now_v7()),
        event,
        OffsetDateTime::now_utc(),
        &sent,
    );
    assert_eq!(
        post(merchant.url(), &elsewhere, sent).await?,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        merchant.arrivals().last().map(|arrival| arrival.verdict),
        Some(Verdict::Refused(Refusal::Signature))
    );
    assert_eq!(merchant.events().len(), 1);
    merchant.stop().await;
    world.close().await;
    Ok(())
}
