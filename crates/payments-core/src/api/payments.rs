// Author: Nicola Antonello

//! A merchant's routes (ADR-0023), and the one extractor that gives every
//! `/payments/{id}` route its ID (ADR-0025).

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::keyed::{Accepted, Attempt, KeyedRequest};
use super::path_id::{PathId, PathRequestId};
use super::problem::Problem;
use super::{AppState, Merchant};
use crate::accounts::Psp;
use crate::idempotency::{Answer, Decided};
use crate::ids::{AccountId, PaymentId, RequestId, TransitionId};
use crate::lifecycle::Refused;
use crate::money::{Currency, PositiveAmount};
use crate::payments::{
    self as commands, AccountCandidates, CreateError, CreatePayment, Outcome, Payment, Recorded,
    StepCommand, StepError, create_payment, find_payment, merchant_fee_rate,
};
use crate::requests::{
    Applied, Ask, Conflict, Followed, RecordRequest, Refusal, RequestError, RequestOutcome,
    Standing, find_request, record_request,
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
    let render = move |decided: Decided<'_>| match decided {
        Decided::Applied(recorded) => Answer {
            status: 201,
            body: json_text(&Created {
                payment_id: id.to_string(),
                reply: recorded.into(),
            }),
        },
        Decided::Requested(requested) => {
            unreachable!("a create records no request: {requested:?}")
        }
        Decided::Refused(refused) => {
            unreachable!("the table of moves has no refusal of a create: {refused}")
        }
    };
    let seal = request.seal(&render);
    match create_payment(merchant.tenant().sealed(&seal), &command).await {
        Ok(Outcome::Applied(recorded) | Outcome::AlreadyApplied(recorded)) => {
            Ok(Attempt::Answered(render(Decided::Applied(&recorded))))
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

/// The final answer of a command on a payment: its step, or what refused it. Its seal stores
/// this and the response shows it, so a replay returns the same bytes (ADR-0026).
fn decided(outcome: Decided<'_>) -> Answer {
    match outcome {
        Decided::Applied(recorded) => Answer {
            status: 200,
            body: json_text(&Reply::from(recorded)),
        },
        Decided::Requested(requested) => Answer {
            status: 202,
            body: json_text(&Accepted {
                request_id: requested.request.to_string(),
                payment_id: requested.payment.to_string(),
            }),
        },
        Decided::Refused(Refusal::Refused(Refused::Illegal(_))) => Problem::IllegalMove.answer(),
        Decided::Refused(Refusal::Refused(Refused::AboveLimit { .. })) => {
            Problem::AmountAboveLimit.answer()
        }
        Decided::Refused(Refusal::Conflict(Conflict::AuthorizationRequested)) => {
            Problem::AuthorizationRequested.answer()
        }
        Decided::Refused(Refusal::Conflict(Conflict::Pending(_))) => {
            Problem::CaptureOrVoidPending.answer()
        }
        Decided::Refused(Refusal::Conflict(Conflict::Stopped)) => Problem::PaymentStopped.answer(),
    }
}

/// A command that applied, under a seal. Each attempt has fresh IDs, so `AlreadyApplied`
/// is never reached through HTTP (ADR-0026), and it would hold the same reply.
fn applied(outcome: Outcome) -> Attempt {
    let (Outcome::Applied(recorded) | Outcome::AlreadyApplied(recorded)) = outcome;
    Attempt::Answered(decided(Decided::Applied(&recorded)))
}

/// A command that was refused, whose refusal is stored under the key.
fn refused(refusal: impl Into<Refusal>) -> Attempt {
    Attempt::Answered(decided(Decided::Refused(refusal.into())))
}

fn amount_of(minor: i64) -> Result<PositiveAmount, Problem> {
    PositiveAmount::new(minor).map_err(|_| Problem::MalformedBody)
}

/// The body of a request that asks the PSP for an amount: `authorize`, `capture` and
/// `refund`. The PSP's business times come from its facts, so a body that still names one is
/// refused, as any unknown field is (ADR-0046).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AmountBody {
    amount: i64,
}

/// `POST /payments/{id}/authorize`: records a request to authorize the payment (ADR-0028).
pub(super) async fn authorize(
    State(state): State<AppState>,
    own: OwnPayment,
    request: KeyedRequest,
) -> Response {
    request
        .answer(own.merchant.tenant(), || {
            attempt_with_amount(&state, &own, &request, Ask::Authorize)
        })
        .await
}

/// `POST /payments/{id}/capture`: records a request to capture it.
pub(super) async fn capture(
    State(state): State<AppState>,
    own: OwnPayment,
    request: KeyedRequest,
) -> Response {
    request
        .answer(own.merchant.tenant(), || {
            attempt_with_amount(&state, &own, &request, Ask::Capture)
        })
        .await
}

/// `POST /payments/{id}/refund`: records a request to refund it.
pub(super) async fn refund(
    State(state): State<AppState>,
    own: OwnPayment,
    request: KeyedRequest,
) -> Response {
    request
        .answer(own.merchant.tenant(), || {
            attempt_with_amount(&state, &own, &request, Ask::Refund)
        })
        .await
}

/// A request whose body names its amount.
async fn attempt_with_amount(
    state: &AppState,
    own: &OwnPayment,
    request: &KeyedRequest,
    ask: fn(PositiveAmount) -> Ask,
) -> Result<Attempt, Problem> {
    let body: AmountBody = request.body_as()?;
    attempt_request(state, own, request, ask(amount_of(body.amount)?)).await
}

/// Records a request to the PSP, under the payment's lock and the key's seal, and once it has
/// committed hands it over to the sender's task, only if this attempt recorded it (ADR-0028,
/// ADR-0043, ADR-0046). Each attempt has a fresh ID, so `AlreadyRecorded` is never reached
/// through HTTP, and it would hold the same answer.
async fn attempt_request(
    state: &AppState,
    own: &OwnPayment,
    request: &KeyedRequest,
    ask: Ask,
) -> Result<Attempt, Problem> {
    let command = RecordRequest {
        request: RequestId::now_v7(),
        payment: own.id,
        ask,
    };
    let seal = request.seal(&decided);
    match record_request(own.merchant.tenant().sealed(&seal), &command).await {
        Ok(RequestOutcome::Recorded(recorded)) => {
            state.hand_over(command.request);
            Ok(Attempt::Answered(decided(Decided::Requested(&recorded))))
        }
        Ok(RequestOutcome::AlreadyRecorded(recorded)) => {
            Ok(Attempt::Answered(decided(Decided::Requested(&recorded))))
        }
        Err(RequestError::Refused(refusal)) => Ok(refused(refusal)),
        Err(RequestError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(RequestError::NotFound { .. }) => Err(Problem::NotFound),
        Err(err @ RequestError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(err @ (RequestError::Mismatch { .. } | RequestError::Invariant { .. })) => {
            Err(Problem::Internal.because(&err))
        }
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
        Err(StepError::Illegal(illegal)) => Ok(refused(illegal)),
        Err(StepError::AuthorizationRequested) => Ok(refused(Conflict::AuthorizationRequested)),
        Err(StepError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(StepError::NotFound { .. }) => Err(Problem::NotFound),
        Err(err @ StepError::NotConfirmed { .. }) => Err(Problem::Unavailable.because(&err)),
        Err(err @ (StepError::Mismatch { .. } | StepError::Invariant { .. })) => {
            Err(Problem::Internal.because(&err))
        }
    }
}

/// `POST /payments/{id}/void`: records a request to void the authorization. It takes no
/// body, as before.
pub(super) async fn void(
    State(state): State<AppState>,
    own: OwnPayment,
    request: KeyedRequest,
) -> Response {
    request
        .answer(own.merchant.tenant(), || async {
            request.no_body()?;
            attempt_request(&state, &own, &request, Ask::Void).await
        })
        .await
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

/// A payment as `GET /payments/{id}` shows it: its terms, its status, what can still be
/// refunded (ADR-0023), and its authorization (ADR-0033).
#[derive(Serialize)]
struct PaymentView<'a> {
    payment_id: String,
    psp: &'a str,
    currency: &'a str,
    amount: i64,
    fee_bps: u16,
    status: &'a str,
    refundable: i64,
    /// `None` until the payment has one.
    authorization: Option<AuthorizationView>,
}

/// A payment's authorization: the amount authorized and when it lapses (ADR-0033).
#[derive(Serialize)]
struct AuthorizationView {
    amount: i64,
    expires_at: String,
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
            authorization: payment
                .authorization
                .map(|authorization| AuthorizationView {
                    amount: authorization.amount.get(),
                    expires_at: written(authorization.expires_at),
                }),
        },
    )
}

/// What `GET /payments/{id}/requests/{request_id}` answers (ADR-0033, ADR-0045).
#[derive(Serialize)]
struct RequestView {
    request_id: String,
    payment_id: String,
    kind: &'static str,
    /// `None` for a void, which asks for no amount.
    amount: Option<i64>,
    state: &'static str,
    /// `None` until the request applies.
    applied: Option<AppliedView>,
}

/// What an applied request's step did, named as its record names it (ADR-0045).
#[derive(Serialize)]
#[serde(untagged)]
enum AppliedView {
    Authorized { amount: i64, expires_at: String },
    Captured { amount: i64, captured_at: String },
    Refunded { amount: i64, refunded_at: String },
    Voided {},
}

impl RequestView {
    fn new(request: &RequestId, payment: PaymentId, followed: Followed) -> Self {
        let applied = match followed.standing {
            Standing::Applied(applied) => Some(match applied {
                Applied::Authorized { amount, expires_at } => AppliedView::Authorized {
                    amount: amount.get(),
                    expires_at: written(expires_at),
                },
                Applied::Captured {
                    amount,
                    captured_at,
                } => AppliedView::Captured {
                    amount: amount.get(),
                    captured_at: written(captured_at),
                },
                Applied::Refunded {
                    amount,
                    refunded_at,
                } => AppliedView::Refunded {
                    amount: amount.get(),
                    refunded_at: written(refunded_at),
                },
                Applied::Voided => AppliedView::Voided {},
            }),
            Standing::Recorded | Standing::Acknowledged | Standing::Refused => None,
        };
        Self {
            request_id: request.get().to_string(),
            payment_id: payment.to_string(),
            kind: followed.kind.as_str(),
            amount: followed.amount.map(PositiveAmount::get),
            state: followed.standing.as_str(),
            applied,
        }
    }
}

/// A time as the API writes it: RFC 3339 in UTC with `Z`, with the fractional digits that
/// it needs (ADR-0045).
fn written(time: BusinessTime) -> String {
    // `%.f` writes no digits for a whole second, and otherwise 3, 6 or 9, as many
    // as it takes; a business time has 6 at most (ADR-0022).
    time.get().format("%Y-%m-%dT%H:%M:%S%.fZ").to_string()
}

/// `GET /payments/{id}/requests/{request_id}`: where a request of the payment stands
/// (ADR-0033, ADR-0045). The payment's extractor checks the credential and the merchant
/// first, so another merchant's request, another payment's and none read as one 404.
pub(super) async fn read_request(
    own: OwnPayment,
    PathRequestId(request): PathRequestId,
) -> Result<Response, Problem> {
    let OwnPayment { merchant, id, .. } = own;
    match find_request(merchant.tenant(), id, &request).await {
        Ok(Some(followed)) => Ok(json(
            StatusCode::OK,
            &RequestView::new(&request, id, followed),
        )),
        Ok(None) => Err(Problem::NotFound),
        Err(err) => Err(Problem::Unavailable.because(&err)),
    }
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

#[cfg(test)]
mod tests {
    use sqlx::types::chrono::DateTime;

    use super::*;
    use crate::requests::RequestKind;

    #[test]
    fn a_time_is_written_in_utc_with_the_digits_that_it_needs() {
        let written_at = |micros: i64| {
            let time = DateTime::from_timestamp_micros(micros).expect("a time in range");
            written(BusinessTime::new(time).expect("a time that PostgreSQL stores"))
        };
        // 2030-01-01T00:00:00Z, and 120 ms and 1 us after it.
        assert_eq!(written_at(1_893_456_000_000_000), "2030-01-01T00:00:00Z");
        assert_eq!(
            written_at(1_893_456_000_120_000),
            "2030-01-01T00:00:00.120Z"
        );
        assert_eq!(
            written_at(1_893_456_000_000_001),
            "2030-01-01T00:00:00.000001Z"
        );
    }

    #[test]
    fn a_recorded_request_is_answered_202_with_its_ids() {
        let id = |text| uuid::Uuid::parse_str(text).expect("a UUID");
        let recorded = crate::requests::RequestRecorded {
            request: id("019b76da-a800-7000-8000-00000000000a"),
            payment: PaymentId::try_from(id("019b76da-a800-7000-8000-000000000001"))
                .expect("a UUIDv7"),
        };
        let answer = decided(Decided::Requested(&recorded));
        assert_eq!(answer.status, 202);
        assert_eq!(
            answer.body,
            r#"{"request_id":"019b76da-a800-7000-8000-00000000000a","payment_id":"019b76da-a800-7000-8000-000000000001"}"#
        );
    }

    #[test]
    fn each_conflict_is_answered_with_its_own_problem() {
        for (conflict, problem) in [
            (
                Conflict::AuthorizationRequested,
                Problem::AuthorizationRequested,
            ),
            (
                Conflict::Pending(RequestKind::Capture),
                Problem::CaptureOrVoidPending,
            ),
            (
                Conflict::Pending(RequestKind::Void),
                Problem::CaptureOrVoidPending,
            ),
            (Conflict::Stopped, Problem::PaymentStopped),
        ] {
            assert_eq!(
                decided(Decided::Refused(conflict.into())),
                problem.answer(),
                "{conflict:?}"
            );
        }
    }
}
