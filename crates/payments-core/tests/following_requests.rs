// Author: Nicola Antonello

//! Following a request (ADR-0033, ADR-0045): where it stands, derived from rows, as its
//! merchant reads it. Each state is reached as the server reaches it: the merchant
//! records the request, the sender sends it, the fake decides it, and a catch-up applies
//! the decision.

mod common;

use std::error::Error;

use axum::http::StatusCode;
use common::payments::{AUTHORIZED, REQUESTED, create_command, fee_rate};
use common::psp::{ScriptedPsp, caught_up, decided, hand_over_and_step, sender_of};
use common::{merchant_id, request_id, runtime_pool, tenant_pool};
use mock_psp::MockPsp;
use mock_psp::wire::Detail;
use payments_core::ids::{MerchantId, PaymentId, RequestId};
use payments_core::money::PositiveAmount;
use payments_core::payments::{create_payment, register_merchant};
use payments_core::requests::{
    Applied, Ask, Followed, RecordRequest, RequestKind, Standing, find_request, record_request,
};
use payments_core::sender::Sender;
use payments_core::tenancy::Tenant;
use payments_core::times::BusinessTime;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};

type TestResult = Result<(), Box<dyn Error>>;

/// Each of two refunds of one capture.
const REFUNDED: i64 = 1_000;

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a positive amount")
}

fn time(at: DateTime<Utc>) -> BusinessTime {
    BusinessTime::new(at).expect("the fake's times are whole microseconds in range")
}

/// A merchant with its pool, the system role's pool, which sends and catches up, and a
/// fake PSP with the sender that the server's task would hold.
struct World {
    system: PgPool,
    tenant: PgPool,
    merchant: MerchantId,
    fake: MockPsp,
    sender: Sender,
}

impl World {
    async fn new(pool: &PgPool) -> Result<Self, Box<dyn Error>> {
        let system = runtime_pool(pool).await?;
        let merchant = merchant_id(1);
        register_merchant(&system, merchant, fee_rate()).await?;
        let fake = MockPsp::start().await?;
        Ok(Self {
            tenant: tenant_pool(pool).await?,
            system,
            merchant,
            sender: sender_of(&fake),
            fake,
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

    /// Records request `n` as the merchant, and nothing more.
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
        record_request(self.me(), &command).await?;
        Ok(command.request)
    }

    /// Records request `n` and sends it, as the server does once the route commits: the
    /// fake holds it.
    async fn sent(
        &mut self,
        n: u32,
        payment: PaymentId,
        ask: Ask,
    ) -> Result<RequestId, Box<dyn Error>> {
        let request = self.recorded(n, payment, ask).await?;
        hand_over_and_step(&mut self.sender, &self.system, &request).await;
        Ok(request)
    }

    /// Applies what the fake decided about this payment.
    async fn caught_up(&self, payment: PaymentId) {
        caught_up(&self.system, &self.fake, payment).await;
    }

    /// Payment `n`, authorized for [`AUTHORIZED`] and caught up. Its requests are
    /// numbered from `10 * n`.
    async fn authorized(&mut self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.created(n).await?;
        let authorize = self
            .sent(10 * n, payment, Ask::Authorize(amount(REQUESTED)))
            .await?;
        self.fake.approve_for(authorize.get(), AUTHORIZED);
        self.caught_up(payment).await;
        Ok(payment)
    }

    /// Payment `n`, captured for [`AUTHORIZED`] and caught up.
    async fn captured(&mut self, n: u32) -> Result<PaymentId, Box<dyn Error>> {
        let payment = self.authorized(n).await?;
        let capture = self
            .sent(10 * n + 1, payment, Ask::Capture(amount(AUTHORIZED)))
            .await?;
        self.fake.approve(capture.get());
        self.caught_up(payment).await;
        Ok(payment)
    }

    async fn follow(
        &self,
        payment: PaymentId,
        request: &RequestId,
    ) -> Result<Option<Followed>, Box<dyn Error>> {
        Ok(find_request(self.me(), payment, request).await?)
    }

    async fn standing(
        &self,
        payment: PaymentId,
        request: &RequestId,
    ) -> Result<Standing, Box<dyn Error>> {
        let followed = self.follow(payment, request).await?;
        Ok(followed.expect("the merchant's own request").standing)
    }

    /// What the fake decided about `request`: the PSP's word, to compare with ours.
    fn decided(&self, payment: PaymentId, request: &RequestId) -> Detail {
        decided(&self.fake, payment, request)
    }

    async fn close(self) {
        self.system.close().await;
        self.tenant.close().await;
    }
}

#[sqlx::test]
async fn a_request_is_recorded_then_acknowledged_then_applied(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .recorded(10, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    let asked = |standing| Followed {
        kind: RequestKind::Authorize,
        amount: Some(amount(REQUESTED)),
        standing,
    };
    assert_eq!(
        world.follow(payment, &authorize).await?,
        Some(asked(Standing::Recorded))
    );

    hand_over_and_step(&mut world.sender, &world.system, &authorize).await;
    assert_eq!(
        world.follow(payment, &authorize).await?,
        Some(asked(Standing::Acknowledged))
    );

    // The PSP decides, but only its history, once caught up, moves the request
    // (ADR-0031).
    world.fake.approve_for(authorize.get(), AUTHORIZED);
    assert_eq!(
        world.standing(payment, &authorize).await?,
        Standing::Acknowledged
    );
    world.caught_up(payment).await;
    let Detail::Authorized { expires_at, .. } = world.decided(payment, &authorize) else {
        panic!("the fake authorized it");
    };
    assert_eq!(
        world.follow(payment, &authorize).await?,
        Some(asked(Standing::Applied(Applied::Authorized {
            amount: amount(AUTHORIZED),
            expires_at: time(expires_at),
        })))
    );

    // A later request of the payment stands on its own: the authorization's answer and
    // decision are not its own.
    let capture = world
        .recorded(11, payment, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    assert_eq!(world.standing(payment, &capture).await?, Standing::Recorded);
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_refused_request_is_told_apart_from_one_in_flight(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let declined = world.created(1).await?;
    let authorize = world
        .sent(10, declined, Ask::Authorize(amount(REQUESTED)))
        .await?;
    world.fake.decline(authorize.get());
    world.caught_up(declined).await;
    assert_eq!(
        world.standing(declined, &authorize).await?,
        Standing::Refused
    );

    let refused = world.authorized(2).await?;
    let capture = world
        .sent(21, refused, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    world.fake.refuse(capture.get());
    world.caught_up(refused).await;
    assert_eq!(world.standing(refused, &capture).await?, Standing::Refused);

    let in_flight = world.authorized(3).await?;
    let capture = world
        .sent(31, in_flight, Ask::Capture(amount(AUTHORIZED)))
        .await?;
    assert_eq!(
        world.standing(in_flight, &capture).await?,
        Standing::Acknowledged
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_applied_request_shows_what_its_step_applied(pool: PgPool) -> TestResult {
    let mut world = World::new(&pool).await?;
    let payment = world.captured(1).await?;
    let capture = request_id(11);
    let Detail::Captured { captured_at, .. } = world.decided(payment, &capture) else {
        panic!("the fake captured it");
    };
    assert_eq!(
        world.follow(payment, &capture).await?,
        Some(Followed {
            kind: RequestKind::Capture,
            amount: Some(amount(AUTHORIZED)),
            standing: Standing::Applied(Applied::Captured {
                amount: amount(AUTHORIZED),
                captured_at: time(captured_at),
            }),
        })
    );

    // Two refunds of one amount: each request shows its own refund's time, which only
    // the link from its fact to its step tells apart (ADR-0045).
    let mut refunds = Vec::new();
    for n in [12, 13] {
        let refund = world
            .sent(n, payment, Ask::Refund(amount(REFUNDED)))
            .await?;
        world.fake.approve(refund.get());
        world.caught_up(payment).await;
        let Detail::Refunded { refunded_at, .. } = world.decided(payment, &refund) else {
            panic!("the fake refunded it");
        };
        refunds.push((refund, time(refunded_at)));
    }
    assert_ne!(
        refunds[0].1, refunds[1].1,
        "the fake stamps each refund apart"
    );
    for (refund, refunded_at) in refunds {
        assert_eq!(
            world.follow(payment, &refund).await?,
            Some(Followed {
                kind: RequestKind::Refund,
                amount: Some(amount(REFUNDED)),
                standing: Standing::Applied(Applied::Refunded {
                    amount: amount(REFUNDED),
                    refunded_at,
                }),
            })
        );
    }

    // A void asks for no amount, and its step has no record.
    let authorized = world.authorized(2).await?;
    let void = world.sent(21, authorized, Ask::Void).await?;
    world.fake.approve(void.get());
    world.caught_up(authorized).await;
    assert_eq!(
        world.follow(authorized, &void).await?,
        Some(Followed {
            kind: RequestKind::Void,
            amount: None,
            standing: Standing::Applied(Applied::Voided),
        })
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_request_that_the_psp_calls_malformed_stays_recorded(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let authorize = world
        .recorded(10, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    let psp = ScriptedPsp::start().await;
    psp.answer_sends(StatusCode::UNPROCESSABLE_ENTITY, None, Vec::new());
    let mut sender = Sender::new(psp.client(), Utc::now());
    hand_over_and_step(&mut sender, &world.system, &authorize).await;
    let answer = sqlx::query_scalar!(
        "SELECT answer FROM payment_request_answers WHERE request_id = $1",
        authorize.get()
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(answer, "malformed", "the sender recorded the PSP's refusal");
    // A bug of ours, for an operator: the merchant keeps seeing it recorded (ADR-0037).
    assert_eq!(
        world.standing(payment, &authorize).await?,
        Standing::Recorded
    );
    world.close().await;
    Ok(())
}

#[sqlx::test]
async fn only_its_own_payment_and_merchant_find_a_request(pool: PgPool) -> TestResult {
    let world = World::new(&pool).await?;
    let payment = world.created(1).await?;
    let other_payment = world.created(2).await?;
    let authorize = world
        .recorded(10, payment, Ask::Authorize(amount(REQUESTED)))
        .await?;
    let other_merchant = merchant_id(2);
    register_merchant(&world.system, other_merchant, fee_rate()).await?;

    assert!(world.follow(payment, &authorize).await?.is_some());
    assert_eq!(world.follow(other_payment, &authorize).await?, None);
    assert_eq!(world.follow(payment, &request_id(99)).await?, None);
    let stranger = Tenant::new(&world.tenant, other_merchant);
    assert_eq!(find_request(stranger, payment, &authorize).await?, None);
    world.close().await;
    Ok(())
}
