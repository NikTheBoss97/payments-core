// Author: Nicola Antonello

//! A request under an idempotency key (ADR-0026).
//!
//! The flow is one for every `POST`. Look the key up: an answer found is replayed, and
//! one to a different request is a reuse. Otherwise build the command with fresh IDs and
//! run it sealed, so that its answer commits with its effect. If another attempt got
//! there first, this one rolled back whole, and the next look finds the answer.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::problem::Problem;
use crate::idempotency::{
    Answer, Decided, Fingerprint, IdempotencyKey, OperatorSeal, Recall, RecallError, Seal, recall,
    recall_operator,
};
use crate::tenancy::Tenant;

/// How a command's final answer is rendered, for its seal and for this response.
pub(super) type Render = dyn Fn(Decided<'_>) -> Answer + Sync;

/// What a 202 says: the request that it recorded, and its payment (ADR-0033).
#[derive(Serialize, Deserialize)]
pub(super) struct Accepted {
    pub(super) request_id: String,
    pub(super) payment_id: String,
}

impl Accepted {
    /// Where the merchant follows the request.
    fn location(&self) -> String {
        format!("/payments/{}/requests/{}", self.payment_id, self.request_id)
    }
}

/// A request's key, what the key would be used for, and its raw body, which the
/// fingerprint needs as exact bytes. The body is parsed only once the key is free.
#[derive(Debug)]
pub struct KeyedRequest {
    key: IdempotencyKey,
    fingerprint: Fingerprint,
    body: Bytes,
}

impl<S: Send + Sync> FromRequest<S> for KeyedRequest {
    type Rejection = Problem;

    async fn from_request(request: Request, state: &S) -> Result<Self, Problem> {
        let key = key_of(request.headers()).ok_or(Problem::MalformedKey)?;
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let body = Bytes::from_request(request, state)
            .await
            .map_err(|rejection| {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Problem::BodyTooLarge
                } else {
                    Problem::MalformedBody
                }
            })?;
        Ok(Self {
            key,
            fingerprint: Fingerprint::of(method.as_str(), &path, &body),
            body,
        })
    }
}

/// The key in a request's one `Idempotency-Key` header. Two would read as a list, which
/// is not a string.
fn key_of(headers: &HeaderMap) -> Option<IdempotencyKey> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    IdempotencyKey::from_header(value.to_str().ok()?).ok()
}

/// What an attempt at a request came to. A failure is a [`Problem`] instead: it is stored
/// nowhere, because either nothing was decided or the outcome is unknown.
#[derive(Debug)]
pub(super) enum Attempt {
    /// The final answer, which committed with the effect.
    Answered(Answer),
    /// Another attempt committed first, and this one rolled back whole.
    KeyTaken,
}

impl KeyedRequest {
    /// Runs the flow for a merchant, where `attempt` builds the command with fresh IDs and
    /// runs it sealed.
    pub(super) async fn answer<F, Fut>(&self, tenant: Tenant<'_>, attempt: F) -> Response
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Attempt, Problem>>,
    {
        self.run(|| recall(tenant, &self.key, self.fingerprint), attempt)
            .await
    }

    /// The same flow for the operator, whose keys are apart from every merchant's
    /// (ADR-0026).
    pub(super) async fn answer_operator<F, Fut>(&self, pool: &PgPool, attempt: F) -> Response
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Attempt, Problem>>,
    {
        self.run(
            || recall_operator(pool, &self.key, self.fingerprint),
            attempt,
        )
        .await
    }

    /// A lost race looks the key up again: the row that it met is committed and is never
    /// removed, so that look finds it, and this loops at most once.
    async fn run<L, Look, F, Fut>(&self, look_up: L, attempt: F) -> Response
    where
        L: Fn() -> Look,
        Look: Future<Output = Result<Recall, RecallError>>,
        F: Fn() -> Fut,
        Fut: Future<Output = Result<Attempt, Problem>>,
    {
        loop {
            match look_up().await {
                Ok(Recall::Unused) => {}
                Ok(Recall::Answered(answer)) => return respond(&answer),
                Ok(Recall::Reused) => return Problem::KeyReused.into_response(),
                Err(err) => return Problem::Unavailable.because(&err).into_response(),
            }
            match attempt().await {
                Ok(Attempt::Answered(answer)) => return respond(&answer),
                Ok(Attempt::KeyTaken) => {}
                Err(problem) => return problem.into_response(),
            }
        }
    }

    /// What a command stores with its effect.
    pub(super) fn seal<'a>(&'a self, answer: &'a Render) -> Seal<'a> {
        Seal {
            key: &self.key,
            fingerprint: self.fingerprint,
            answer,
        }
    }

    /// What an operator's command stores with its effect.
    pub(super) fn operator_seal<'a>(&'a self, answer: &'a Answer) -> OperatorSeal<'a> {
        OperatorSeal {
            key: &self.key,
            fingerprint: self.fingerprint,
            answer,
        }
    }

    /// The body as JSON. Unknown fields are refused by the type that it is parsed into.
    pub(super) fn body_as<T: DeserializeOwned>(&self) -> Result<T, Problem> {
        serde_json::from_slice(&self.body).map_err(|_| Problem::MalformedBody)
    }

    /// For a route that takes no body.
    pub(super) fn no_body(&self) -> Result<(), Problem> {
        if self.body.is_empty() {
            Ok(())
        } else {
            Err(Problem::MalformedBody)
        }
    }
}

/// An answer as a response, the same whichever attempt made it. Only a final answer is
/// ever stored, and those below 400 are the JSON of an applied command.
fn respond(answer: &Answer) -> Response {
    let status = u16::try_from(answer.status)
        .ok()
        .and_then(|status| StatusCode::from_u16(status).ok())
        .expect("the CHECK on status in both tables of answers keeps it between 200 and 499");
    let content_type = if status.is_success() {
        "application/json"
    } else {
        "application/problem+json"
    };
    let mut response = (
        status,
        [(header::CONTENT_TYPE, content_type)],
        answer.body.clone(),
    )
        .into_response();
    if status == StatusCode::ACCEPTED {
        // A recorded request: where to follow it comes from the stored body, so that a
        // replay carries the same headers as the first answer (ADR-0046).
        let accepted: Accepted = serde_json::from_str(&answer.body)
            .expect("a stored 202 is the JSON of a recorded request");
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::from_str(&accepted.location())
                .expect("two hyphenated UUIDs and slashes make a header"),
        );
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCEPTED: &str = r#"{"request_id":"019b76da-a800-7000-8000-00000000000a","payment_id":"019b76da-a800-7000-8000-000000000001"}"#;

    #[test]
    fn a_recorded_requests_answer_says_where_to_follow_it_each_time() {
        let answer = Answer {
            status: 202,
            body: ACCEPTED.to_owned(),
        };
        // The first answer and its replay are rendered by the same function.
        for response in [respond(&answer), respond(&answer)] {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let headers = response.headers();
            assert_eq!(
                headers[header::LOCATION],
                "/payments/019b76da-a800-7000-8000-000000000001/requests/019b76da-a800-7000-8000-00000000000a"
            );
            assert_eq!(headers[header::RETRY_AFTER], "1");
            assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        }
    }

    #[test]
    fn no_other_answer_names_a_request() {
        for answer in [
            Answer {
                status: 200,
                body: "{}".to_owned(),
            },
            Answer {
                status: 409,
                body: "{}".to_owned(),
            },
        ] {
            let response = respond(&answer);
            assert!(response.headers().get(header::LOCATION).is_none());
            assert!(response.headers().get(header::RETRY_AFTER).is_none());
        }
    }
}
