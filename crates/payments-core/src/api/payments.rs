// Author: Nicola Antonello

//! A merchant's routes (ADR-0023), and the one extractor that gives every
//! `/payments/{id}` route its ID (ADR-0025).

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::keyed::{Attempt, KeyedRequest};
use super::problem::Problem;
use super::{AppState, Merchant};
use crate::accounts::Psp;
use crate::idempotency::Answer;
use crate::ids::{AccountId, PaymentId, TransitionId};
use crate::lifecycle::Refused;
use crate::money::{Currency, PositiveAmount};
use crate::payments::{
    AccountCandidates, CreateError, CreatePayment, Outcome, Payment, Recorded, create_payment,
    find_payment, merchant_fee_rate,
};

/// A payment of the merchant that asked. A route that takes one cannot reach another
/// merchant's payment: holding this proves that its merchant is the credential's.
#[derive(Debug)]
pub struct OwnPayment {
    pub merchant: Merchant,
    pub id: PaymentId,
    pub payment: Payment,
}

impl FromRequestParts<AppState> for OwnPayment {
    type Rejection = Problem;

    /// The credential comes first, so that someone with no key learns nothing. Then the
    /// payment is read as the merchant, under its policy, and its merchant is compared
    /// with the credential's, so isolation is enforced twice. A stranger's payment, no
    /// payment, and an ID that is not one all read the same, so an ID's existence does
    /// not leak. A payment's merchant never changes, so this cannot go stale.
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Problem> {
        let merchant = Merchant::from_request_parts(parts, state).await?;
        let Path(text) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| Problem::NotFound)?;
        let id = parse_payment_id(&text).ok_or(Problem::NotFound)?;
        match find_payment(merchant.tenant(), id).await {
            Ok(Some(payment)) if payment.merchant == merchant.id() => Ok(Self {
                merchant,
                id,
                payment,
            }),
            Ok(_) => Err(Problem::NotFound),
            Err(err) => Err(Problem::Unavailable.because(&err)),
        }
    }
}

/// A payment's ID in the one spelling that the API uses. Any other spelling of a real ID
/// is refused, because one payment has one path: a retry that spelled it differently
/// would read as another request (ADR-0026).
fn parse_payment_id(text: &str) -> Option<PaymentId> {
    let id = Uuid::try_parse(text).ok()?;
    if id.to_string() != text {
        return None;
    }
    PaymentId::try_from(id).ok()
}

/// Where a command landed, as a client sees it: the `Recorded` of ADR-0019, with statuses
/// as their labels (ADR-0023).
#[derive(Serialize)]
struct Reply {
    seq: i32,
    from: &'static str,
    to: &'static str,
    refundable: i64,
}

impl From<&Recorded> for Reply {
    fn from(recorded: &Recorded) -> Self {
        Self {
            seq: recorded.seq,
            from: recorded.from.as_str(),
            to: recorded.to.as_str(),
            refundable: recorded.refundable,
        }
    }
}

/// What `POST /payments` answers: the new payment's ID and the reply.
#[derive(Serialize)]
struct Created {
    payment_id: String,
    #[serde(flatten)]
    reply: Reply,
}

/// The body of `POST /payments`. It names no merchant, which comes from the credential,
/// and no fee, which the operator sets, and an unknown field is refused (ADR-0024).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    currency: String,
    amount: i64,
}

/// `POST /payments`.
pub(super) async fn create(merchant: Merchant, request: KeyedRequest) -> Response {
    request
        .answer(merchant.tenant(), || attempt_create(&merchant, &request))
        .await
}

async fn attempt_create(merchant: &Merchant, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let body: CreateBody = request.body_as()?;
    let currency = Currency::parse(&body.currency).map_err(|_| Problem::MalformedBody)?;
    let amount = PositiveAmount::new(body.amount).map_err(|_| Problem::MalformedBody)?;
    let fee_rate = merchant_fee_rate(merchant.tenant())
        .await
        .map_err(|err| Problem::Unavailable.because(&err))?
        .expect("api_keys_merchant_fkey: the merchant that a credential names exists");
    let id = PaymentId::now_v7();
    let command = CreatePayment {
        id,
        step: TransitionId::now_v7(),
        merchant: merchant.id(),
        // The only provider until phase 4 (ADR-0023).
        psp: Psp::Mock,
        currency,
        amount,
        fee_rate,
        accounts: AccountCandidates {
            fee_revenue: AccountId::now_v7(),
            merchant_payable: AccountId::now_v7(),
            psp_receivable: AccountId::now_v7(),
        },
    };
    let render = move |decided: Result<&Recorded, &Refused>| match decided {
        Ok(recorded) => Answer {
            status: 201,
            body: json_text(&Created {
                payment_id: id.to_string(),
                reply: recorded.into(),
            }),
        },
        Err(refused) => unreachable!("the table of moves has no refusal of a create: {refused}"),
    };
    let seal = request.seal(&render);
    match create_payment(merchant.tenant().sealed(&seal), &command).await {
        Ok(Outcome::Applied(recorded) | Outcome::AlreadyApplied(recorded)) => {
            Ok(Attempt::Answered(render(Ok(&recorded))))
        }
        Err(CreateError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(CreateError::UnknownCurrency { .. }) => Err(Problem::UnknownCurrency),
        Err(err @ CreateError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(
            err @ (CreateError::UnknownMerchant { .. }
            | CreateError::Account { .. }
            | CreateError::Mismatch { .. }
            | CreateError::Invariant { .. }),
        ) => Err(Problem::Internal.because(&err)),
    }
}

/// A payment as `GET /payments/{id}` shows it: its terms, its status and what can still be
/// refunded (ADR-0023).
#[derive(Serialize)]
struct PaymentView<'a> {
    payment_id: String,
    psp: &'a str,
    currency: &'a str,
    amount: i64,
    fee_bps: u16,
    status: &'a str,
    refundable: i64,
}

/// `GET /payments/{id}`.
pub(super) async fn read_payment(own: OwnPayment) -> Response {
    let OwnPayment { id, payment, .. } = own;
    json(
        StatusCode::OK,
        &PaymentView {
            payment_id: id.to_string(),
            psp: payment.psp.as_str(),
            currency: payment.currency.as_str(),
            amount: payment.amount.get(),
            fee_bps: payment.fee_rate.get(),
            status: payment.status.as_str(),
            refundable: payment.refundable,
        },
    )
}

/// A JSON answer.
fn json(status: StatusCode, value: &impl Serialize) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        json_text(value),
    )
        .into_response()
}

fn json_text(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("strings and numbers always serialize")
}
