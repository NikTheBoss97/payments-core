// Author: Nicola Antonello

//! The operator's routes (ADR-0024).
//!
//! Each runs the flow that every `POST` shares, under the operator's own keys (ADR-0026). An
//! operator's command has one final answer, known before it runs.

use axum::response::Response;
use serde::{Deserialize, Serialize};

use super::credential::Operator;
use super::keyed::{Attempt, KeyedRequest};
use super::path_id::PathId;
use super::payments::json_text;
use super::problem::Problem;
use crate::credentials::ApiKeySecret;
use crate::idempotency::Answer;
use crate::ids::{ApiKeyId, MerchantId};
use crate::money::BasisPoints;
use crate::operator::{OperatorError, issue_api_key, register_merchant_sealed, revoke_api_key};

/// What a command came to, and the answer that it gives if it applied.
fn settled(result: Result<(), OperatorError>, answer: Answer) -> Result<Attempt, Problem> {
    match result {
        Ok(()) => Ok(Attempt::Answered(answer)),
        Err(OperatorError::KeyTaken) => Ok(Attempt::KeyTaken),
        Err(OperatorError::NotFound) => Err(Problem::NotFound),
        Err(err @ OperatorError::NotConfirmed(_)) => Err(Problem::Unavailable.because(&err)),
        Err(err @ OperatorError::Invariant(_)) => Err(Problem::Internal.because(&err)),
    }
}

/// The body of `POST /merchants`. The operator sets the fee (ADR-0024).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterBody {
    fee_bps: u16,
}

/// What `POST /merchants` answers.
#[derive(Serialize)]
struct Registered {
    merchant_id: String,
}

/// `POST /merchants`.
pub(super) async fn register(operator: Operator, request: KeyedRequest) -> Response {
    request
        .answer_operator(operator.pool(), || attempt_register(&operator, &request))
        .await
}

async fn attempt_register(operator: &Operator, request: &KeyedRequest) -> Result<Attempt, Problem> {
    let body: RegisterBody = request.body_as()?;
    let fee_rate = BasisPoints::new(body.fee_bps).map_err(|_| Problem::MalformedBody)?;
    let id = MerchantId::now_v7();
    let answer = Answer {
        status: 201,
        body: json_text(&Registered {
            merchant_id: id.to_string(),
        }),
    };
    let seal = request.operator_seal(&answer);
    let result = register_merchant_sealed(operator.pool(), &seal, id, fee_rate).await;
    settled(result, answer)
}

/// What `POST /merchants/{id}/api-keys` stores, and what a retry gets. It has no field for
/// the secret, so that no replay can show one (ADR-0026).
#[derive(Serialize)]
struct IssuedKey {
    api_key_id: String,
}

/// What the first answer to `POST /merchants/{id}/api-keys` shows, and the only time that
/// the secret is shown (ADR-0024).
#[derive(Serialize)]
struct NewKey<'a> {
    api_key_id: String,
    secret: &'a str,
}

/// `POST /merchants/{id}/api-keys`.
pub(super) async fn issue_key(
    operator: Operator,
    PathId(merchant): PathId<MerchantId>,
    request: KeyedRequest,
) -> Response {
    request
        .answer_operator(operator.pool(), || {
            attempt_issue(&operator, merchant, &request)
        })
        .await
}

async fn attempt_issue(
    operator: &Operator,
    merchant: MerchantId,
    request: &KeyedRequest,
) -> Result<Attempt, Problem> {
    request.no_body()?;
    let id = ApiKeyId::now_v7();
    let secret = ApiKeySecret::generate();
    let stored = Answer {
        status: 201,
        body: json_text(&IssuedKey {
            api_key_id: id.to_string(),
        }),
    };
    let seal = request.operator_seal(&stored);
    let result = issue_api_key(operator.pool(), &seal, id, merchant, &secret.digest()).await;
    settled(
        result,
        Answer {
            status: 201,
            body: json_text(&NewKey {
                api_key_id: id.to_string(),
                secret: secret.expose(),
            }),
        },
    )
}

/// What `POST /api-keys/{id}/revoke` answers.
#[derive(Serialize)]
struct RevokedKey {
    api_key_id: String,
}

/// `POST /api-keys/{id}/revoke`. Revoking a revoked key answers too: what the request asks
/// for already holds.
pub(super) async fn revoke_key(
    operator: Operator,
    PathId(id): PathId<ApiKeyId>,
    request: KeyedRequest,
) -> Response {
    request
        .answer_operator(operator.pool(), || attempt_revoke(&operator, id, &request))
        .await
}

async fn attempt_revoke(
    operator: &Operator,
    id: ApiKeyId,
    request: &KeyedRequest,
) -> Result<Attempt, Problem> {
    request.no_body()?;
    let answer = Answer {
        status: 200,
        body: json_text(&RevokedKey {
            api_key_id: id.to_string(),
        }),
    };
    let seal = request.operator_seal(&answer);
    let result = revoke_api_key(operator.pool(), &seal, id).await;
    settled(result, answer)
}
