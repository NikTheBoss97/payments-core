// Author: Nicola Antonello

//! A merchant's routes (ADR-0023), and the one extractor that gives every
//! `/payments/{id}` route its ID (ADR-0025).

use axum::extract::{FromRequestParts, Path};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use uuid::Uuid;

use super::problem::Problem;
use super::{AppState, Merchant};
use crate::ids::PaymentId;
use crate::payments::{Payment, find_payment};

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
            Err(_) => Err(Problem::Unavailable),
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
        serde_json::to_string(value).expect("strings and numbers always serialize"),
    )
        .into_response()
}
