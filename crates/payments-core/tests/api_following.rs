// Author: Nicola Antonello

//! `GET /payments/{id}/requests/{request_id}` (ADR-0033, ADR-0045): where a request stands,
//! as its merchant reads it over HTTP. Until the four routes record requests, the tests
//! record them through the library, and each reaches its state through the fake.

mod common;

use std::error::Error;
use std::fmt::Display;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::api::{Pools, Reply, a_merchant, a_payment, bearer, get, get_with_authorization, send};
use common::payments::{AUTHORIZED, REQUESTED};
use common::psp::{caught_up, decided, hand_over_and_step, sender_of};
use common::request_id;
use mock_psp::MockPsp;
use mock_psp::wire::Detail;
use payments_core::api::problem::Problem;
use payments_core::api::router;
use payments_core::credentials::ApiKeySecret;
use payments_core::ids::{MerchantId, PaymentId, RequestId};
use payments_core::money::PositiveAmount;
use payments_core::requests::{Ask, RecordRequest, record_request};
use payments_core::sender::Sender;
use payments_core::tenancy::Tenant;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// One of the refunds of a capture.
const REFUNDED: i64 = 1_000;

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

/// `GET /payments/{payment}/requests/{request}`, with this secret.
fn read(payment: impl Display, request: impl Display, secret: &ApiKeySecret) -> Request<Body> {
    get_with_authorization(
        &format!("/payments/{payment}/requests/{request}"),
        &bearer(secret),
    )
}

/// A time that the API wrote, which must be RFC 3339 in UTC with `Z`.
fn time_in(value: &Value) -> DateTime<Utc> {
    let text = value.as_str().expect("a time is a string");
    assert!(text.ends_with('Z'), "{text}");
    DateTime::parse_from_rfc3339(text)
        .expect("an RFC 3339 time")
        .with_timezone(&Utc)
}

/// A merchant with its key, the server's two pools, and a fake PSP with the sender that the
/// server's task would hold.
struct World {
    pools: Pools,
    merchant: MerchantId,
    secret: ApiKeySecret,
    fake: MockPsp,
    sender: Sender,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let pools = Pools::login(pool).await?;
        let (merchant, secret) = a_merchant(&pools, 1).await?;
        let fake = MockPsp::start().await?;
        Ok(Self {
            sender: sender_of(&fake),
            pools,
            merchant,
            secret,
            fake,
        })
    }

    async fn payment(&self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        a_payment(&self.pools, self.merchant, n).await
    }

    /// Records request `n` as the merchant, as the four routes will (ADR-0045).
    async fn recorded(
        &self,
        n: u32,
        payment: PaymentId,
        ask: Ask,
    ) -> Result<RequestId, Box<dyn Error>> {
        let command = RecordRequest {
            request: request_id(n),
            payment,
            ask,
        };
        record_request(Tenant::new(&self.pools.tenant, self.merchant), &command).await?;
        Ok(command.request)
    }

    /// Records request `n` and sends it: the fake holds it.
    async fn sent(
        &mut self,
        n: u32,
        payment: PaymentId,
        ask: Ask,
    ) -> Result<RequestId, Box<dyn Error>> {
        let request = self.recorded(n, payment, ask).await?;
        hand_over_and_step(&mut self.sender, &self.pools.system, &request).await;
        Ok(request)
    }

    async fn caught_up(&self, payment: PaymentId) {
        caught_up(&self.pools.system, &self.fake, payment).await;
    }

    /// Payment `n`, authorized for [`AUTHORIZED`] and caught up. Its requests are
    /// numbered from `10 * n`.
    async fn authorized(&mut self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.payment(n).await?;
        let authorize = self
            .sent(10 * n, payment, Ask::Authorize(amount(REQUESTED)))
            .await?;
        self.fake.approve_for(authorize.get(), AUTHORIZED);
        self.caught_up(payment).await;
        Ok(payment)
    }

    /// The merchant's read of its request.
    async fn read(&self, payment: PaymentId, request: &RequestId) -> Reply {
        send(
            router(self.pools.state()),
            read(payment, request, &self.secret),
        )
        .await
    }

    /// The body of a read that answered 200.
    async fn body(&self, payment: PaymentId, request: &RequestId) -> Result<Value, Box<dyn Error>> {
        let reply = self.read(payment, request).await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        assert_eq!(reply.header("content-type"), "application/json");
        Ok(serde_json::from_str(&reply.body)?)
    }

    async fn close(self) {
        self.pools.close().await;
    }
}

#[sqlx::test]
async fn a_request_reads_recorded_then_acknowledged_then_applied(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let authorize = world
        .recorded(10, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    let reply = world.read(payment, &authorize).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(
        reply.body,
        format!(
            r#"{{"request_id":"{authorize}","payment_id":"{payment}","kind":"authorize","amount":10000,"state":"recorded","applied":null}}"#
        )
    );

    hand_over_and_step(&mut world.sender, &world.pools.system, &authorize).await;
    let body = world.body(payment, &authorize).await?;
    assert_eq!(body["state"], "acknowledged");
    assert_eq!(body["applied"], Value::Null);

    world.fake.approve_for(authorize.get(), AUTHORIZED);
    world.caught_up(payment).await;
    let Detail::Authorized { expires_at, .. } = decided(&world.fake, payment, &authorize) else {
        panic!("the fake authorized it");
    };
    let body = world.body(payment, &authorize).await?;
    assert_eq!(body["state"], "applied");
    assert_eq!(body["amount"], REQUESTED, "what was asked");
    assert_eq!(body["applied"]["amount"], AUTHORIZED, "less than asked");
    assert_eq!(time_in(&body["applied"]["expires_at"]), expires_at);
    assert_eq!(
        body["applied"].as_object().map(|applied| applied.len()),
        Some(2)
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn each_kind_shows_what_its_step_applied(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let payment = world.authorized(1).await?;
    let capture = world
        .sent(11, payment, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.approve(capture.get());
    world.caught_up(payment).await;
    let Detail::Captured { captured_at, .. } = decided(&world.fake, payment, &capture) else {
        panic!("the fake captured it");
    };
    let body = world.body(payment, &capture).await?;
    assert_eq!(body["kind"], "capture");
    assert_eq!(body["state"], "applied");
    assert_eq!(body["applied"]["amount"], AUTHORIZED);
    assert_eq!(time_in(&body["applied"]["captured_at"]), captured_at);
    assert_eq!(
        body["applied"].as_object().map(|applied| applied.len()),
        Some(2)
    );

    let refund = world
        .sent(12, payment, Ask::Refund(amount(REFUNDED)))
        .await?;
    world.fake.approve(refund.get());
    world.caught_up(payment).await;
    let Detail::Refunded { refunded_at, .. } = decided(&world.fake, payment, &refund) else {
        panic!("the fake refunded it");
    };
    let body = world.body(payment, &refund).await?;
    assert_eq!(body["kind"], "refund");
    assert_eq!(body["amount"], REFUNDED);
    assert_eq!(body["applied"]["amount"], REFUNDED);
    assert_eq!(time_in(&body["applied"]["refunded_at"]), refunded_at);
    assert_eq!(
        body["applied"].as_object().map(|applied| applied.len()),
        Some(2)
    );

    // A void asks for no amount, and its step has no record to show.
    let authorized = world.authorized(2).await?;
    let void = world.sent(21, authorized, Ask::Void).await?;
    world.fake.approve(void.get());
    world.caught_up(authorized).await;
    let reply = world.read(authorized, &void).await;
    assert_eq!(
        reply.body,
        format!(
            r#"{{"request_id":"{void}","payment_id":"{authorized}","kind":"void","amount":null,"state":"applied","applied":{{}}}}"#
        )
    );

    // A refused capture reads refused, with nothing applied.
    let refused = world.authorized(3).await?;
    let capture = world
        .sent(31, refused, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.refuse(capture.get());
    world.caught_up(refused).await;
    let body = world.body(refused, &capture).await?;
    assert_eq!(body["state"], "refused");
    assert_eq!(body["applied"], Value::Null);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_request_of_another_payment_or_merchant_reads_like_none(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let other = world.payment(2).await?;
    let authorize = world
        .recorded(10, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    let canonical = authorize.to_string();
    let (_, stranger) = a_merchant(&world.pools, 2).await?;

    // Another payment of the same merchant, no such request, and each other spelling of
    // the request's ID: one request has one path (ADR-0026).
    for (on, request) in [
        (other, canonical.clone()),
        (payment, request_id(99).to_string()),
        (payment, canonical.to_uppercase()),
        (payment, canonical.replace('-', "")),
        (payment, canonical.replacen('0', "%30", 1)),
        (payment, "not-a-uuid".to_owned()),
        (payment, Uuid::nil().to_string()),
    ] {
        let reply = send(
            router(world.pools.state()),
            read(on, &request, &world.secret),
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_FOUND,
            "{on}/{request}: {reply:?}"
        );
        assert_eq!(reply.body, Problem::NotFound.body(), "{on}/{request}");
    }
    // Another merchant's key reaches neither the payment nor its request.
    let reply = send(
        router(world.pools.state()),
        read(payment, &authorize, &stranger),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{reply:?}");
    assert_eq!(reply.body, Problem::NotFound.body());
    // Its own merchant reads it.
    assert_eq!(world.read(payment, &authorize).await.status, StatusCode::OK);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_credential_comes_before_the_ids(pool: PgPool) -> TestResult {
    // Someone with no key learns nothing, not even which IDs are well formed.
    let world = World::new(&pool).await?;
    let payment = world.payment(1).await?;
    let authorize = world
        .recorded(10, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    for path in [
        format!("/payments/{payment}/requests/{authorize}"),
        format!("/payments/{payment}/requests/not-a-uuid"),
        "/payments/not-a-uuid/requests/not-a-uuid".to_owned(),
    ] {
        let reply = send(router(world.pools.state()), get(&path)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(reply.body, Problem::Unauthorized.body(), "{path}");
    }
    world.close().await;
    Ok(())
}
