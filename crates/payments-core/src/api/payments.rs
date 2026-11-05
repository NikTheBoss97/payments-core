// Author: Nicola Antonello

//! A merchant's routes (ADR-0023), and the one extractor that gives every
//! `/payments/{id}` route its ID (ADR-0025).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use sqlx::types::chrono::{DateTime, Utc};

use super::keyed::{Attempt, KeyedRequest};
use super::path_id::PathId;
use super::problem::Problem;
use super::{AppState, Merchant};
use crate::accounts::Psp;
use crate::idempotency::Answer;
use crate::ids::{AccountId, PaymentId, TransactionId, TransitionId};
use crate::lifecycle::Refused;
use crate::money::{Currency, PositiveAmount};
use crate::payments::{
    self as commands, AccountCandidates, Authorize, AuthorizeError, Capture, CaptureError,
    CreateError, CreatePayment, Outcome, Payment, Recorded, Refund, RefundError, StepCommand,
    StepError, create_payment, find_payment, merchant_fee_rate,
};
use crate::times::BusinessTime;

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
        let PathId(id) = PathId::<PaymentId>::from_request_parts(parts, state).await?;
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
    let amount = amount_of(body.amount)?;
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

/// The final answer of a command on a payment: what the table of moves gave. Its seal stores
/// this and the response shows it, so a replay returns the same bytes (ADR-0026).
fn decided(outcome: Result<&Recorded, &Refused>) -> Answer {
    match outcome {
        Ok(recorded) => Answer {
            status: 200,
            body: json_text(&Reply::from(recorded)),
        },
        Err(Refused::Illegal(_)) => Problem::IllegalMove.answer(),
        Err(Refused::AboveLimit { .. }) => Problem::AmountAboveLimit.answer(),
    }
}

/// A command that applied, under a seal. Each attempt has fresh IDs, so `AlreadyApplied`
/// is never reached through HTTP (ADR-0026), and it would hold the same reply.
fn applied(outcome: Outcome) -> Attempt {
    let (Outcome::Applied(recorded) | Outcome::AlreadyApplied(recorded)) = outcome;
    Attempt::Answered(decided(Ok(&recorded)))
}

/// A command that the table of moves refused, whose refusal is stored under the key.
fn refused(refused: Refused) -> Attempt {
    Attempt::Answered(decided(Err(&refused)))
}

fn amount_of(minor: i64) -> Result<PositiveAmount, Problem> {
    PositiveAmount::new(minor).map_err(|_| Problem::MalformedBody)
}

/// An RFC 3339 string as a business time, which proves that it can be stored (ADR-0022).
/// It has to name its offset.
fn time_of(text: &str) -> Result<BusinessTime, Problem> {
    let time = DateTime::parse_from_rfc3339(text).map_err(|_| Problem::MalformedBody)?;
    BusinessTime::new(time.with_timezone(&Utc)).map_err(|_| Problem::MalformedBody)
}

/// The body of `POST /payments/{id}/authorize`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeBody {
    amount: i64,
    expires_at: String,
}

/// `POST /payments/{id}/authorize`, the merchant's stand-in for the PSP's answer until
/// phase 4 (ADR-0023).
pub(super) async fn authorize(own: OwnPayment, request: KeyedRequest) -> Response {
    request
        .answer(own.merchant.tenant(), || attempt_authorize(&own, &request))
        .await
}

async fn attempt_authorize(own: &OwnPayment, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let body: AuthorizeBody = request.body_as()?;
    let command = Authorize {
        step: TransitionId::now_v7(),
        payment: own.id,
        amount: amount_of(body.amount)?,
        expires_at: time_of(&body.expires_at)?,
    };
    let seal = request.seal(&decided);
    let scope = own.merchant.tenant().sealed(&seal);
    match commands::authorize(scope, &command).await {
        Ok(outcome) => Ok(applied(outcome)),
        Err(AuthorizeError::Refused(refusal)) => Ok(refused(refusal)),
        Err(AuthorizeError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(AuthorizeError::NotFound { .. }) => Err(Problem::NotFound),
        Err(err @ AuthorizeError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(err @ (AuthorizeError::Mismatch { .. } | AuthorizeError::Invariant { .. })) => {
            Err(Problem::Internal.because(&err))
        }
    }
}

/// The body of `POST /payments/{id}/capture`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureBody {
    amount: i64,
    captured_at: String,
}

/// `POST /payments/{id}/capture`.
pub(super) async fn capture(own: OwnPayment, request: KeyedRequest) -> Response {
    request
        .answer(own.merchant.tenant(), || attempt_capture(&own, &request))
        .await
}

async fn attempt_capture(own: &OwnPayment, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let body: CaptureBody = request.body_as()?;
    let command = Capture {
        step: TransitionId::now_v7(),
        posting: TransactionId::now_v7(),
        payment: own.id,
        amount: amount_of(body.amount)?,
        captured_at: time_of(&body.captured_at)?,
    };
    let seal = request.seal(&decided);
    let scope = own.merchant.tenant().sealed(&seal);
    match commands::capture(scope, &command).await {
        Ok(outcome) => Ok(applied(outcome)),
        Err(CaptureError::Refused(refusal)) => Ok(refused(refusal)),
        Err(CaptureError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(CaptureError::NotFound { .. }) => Err(Problem::NotFound),
        Err(err @ CaptureError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(
            err @ (CaptureError::MissingAccount { .. }
            | CaptureError::Mismatch { .. }
            | CaptureError::Invariant { .. }),
        ) => Err(Problem::Internal.because(&err)),
    }
}

/// The body of `POST /payments/{id}/refund`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefundBody {
    amount: i64,
    refunded_at: String,
}

/// `POST /payments/{id}/refund`.
pub(super) async fn refund(own: OwnPayment, request: KeyedRequest) -> Response {
    request
        .answer(own.merchant.tenant(), || attempt_refund(&own, &request))
        .await
}

async fn attempt_refund(own: &OwnPayment, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let body: RefundBody = request.body_as()?;
    let command = Refund {
        step: TransitionId::now_v7(),
        posting: TransactionId::now_v7(),
        payment: own.id,
        amount: amount_of(body.amount)?,
        refunded_at: time_of(&body.refunded_at)?,
    };
    let seal = request.seal(&decided);
    let scope = own.merchant.tenant().sealed(&seal);
    match commands::refund(scope, &command).await {
        Ok(outcome) => Ok(applied(outcome)),
        Err(RefundError::Refused(refusal)) => Ok(refused(refusal)),
        Err(RefundError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(RefundError::NotFound { .. }) => Err(Problem::NotFound),
        Err(err @ RefundError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(
            err @ (RefundError::MissingAccount { .. }
            | RefundError::Mismatch { .. }
            | RefundError::Invariant { .. }),
        ) => Err(Problem::Internal.because(&err)),
    }
}

/// The command of a route that takes no body. Not even an empty object: two bodies that it
/// ignored would still read as two requests under a key.
fn step_command(own: &OwnPayment, request: &KeyedRequest) -> Result<StepCommand, Problem> {
    request.no_body()?;
    Ok(StepCommand {
        step: TransitionId::now_v7(),
        payment: own.id,
    })
}

/// What a step with no amount came to.
fn after_step(result: Result<Outcome, StepError>) -> Result<Attempt, Problem> {
    match result {
        Ok(outcome) => Ok(applied(outcome)),
        Err(StepError::Illegal(illegal)) => Ok(refused(illegal.into())),
        Err(StepError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(StepError::NotFound { .. }) => Err(Problem::NotFound),
        Err(err @ StepError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(err @ (StepError::Mismatch { .. } | StepError::Invariant { .. })) => {
            Err(Problem::Internal.because(&err))
        }
    }
}

/// `POST /payments/{id}/void`.
pub(super) async fn void(own: OwnPayment, request: KeyedRequest) -> Response {
    request
        .answer(own.merchant.tenant(), || attempt_void(&own, &request))
        .await
}

async fn attempt_void(own: &OwnPayment, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let command = step_command(own, request)?;
    let seal = request.seal(&decided);
    after_step(commands::void(own.merchant.tenant().sealed(&seal), &command).await)
}

/// `POST /payments/{id}/cancel`.
pub(super) async fn cancel(own: OwnPayment, request: KeyedRequest) -> Response {
    request
        .answer(own.merchant.tenant(), || attempt_cancel(&own, &request))
        .await
}

async fn attempt_cancel(own: &OwnPayment, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let command = step_command(own, request)?;
    let seal = request.seal(&decided);
    after_step(commands::cancel(own.merchant.tenant().sealed(&seal), &command).await)
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

pub(super) fn json_text(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("strings and numbers always serialize")
}
