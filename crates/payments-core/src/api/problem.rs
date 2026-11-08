// Author: Nicola Antonello

//! Errors as a client sees them: `application/problem+json` (RFC 9457, ADR-0023).
//!
//! A problem's text is fixed here, so it can never reveal SQL, a constraint or another
//! merchant's data. Its body comes from one pure function, because a key's record stores
//! the bytes that a replay returns (ADR-0026).

use std::error::Error;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::idempotency::Answer;

/// A problem's `type` is an absolute URI into the document that explains it.
const TYPE_BASE: &str = "https://github.com/NikTheBoss97/payments-core/blob/main/docs/api.md#";

/// Every way that a request can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// A missing, malformed, unknown or revoked credential: one answer for all of them,
    /// so that a client learns nothing about a key that it does not hold (ADR-0024).
    Unauthorized,
    /// No such route, payment, merchant or API key, or another merchant's payment.
    NotFound,
    /// A route that does not take this method.
    MethodNotAllowed,
    /// No `Idempotency-Key` header, or one that is not a Structured Field string of 1 to
    /// 255 visible ASCII characters.
    MalformedKey,
    /// A body that is not what its route takes, including a field that it does not know.
    MalformedBody,
    /// A body above the limit, which is axum's default of 2 MB.
    BodyTooLarge,
    /// The key holds the answer to a different request (ADR-0026).
    KeyReused,
    /// A currency that is well formed but not one that the service knows (ADR-0023).
    UnknownCurrency,
    /// The payment's status does not allow this move. It is final, so it is stored under
    /// the key (ADR-0026).
    IllegalMove,
    /// The amount is above what the payment allows. It is final, so it is stored under the
    /// key (ADR-0026).
    AmountAboveLimit,
    /// The payment has a request to authorize it already, decided or not. It is final, so it
    /// is stored under the key (ADR-0039).
    AuthorizationRequested,
    /// A capture or a void of the payment waits for the provider's decision. It is final, so
    /// it is stored under the key (ADR-0039).
    CaptureOrVoidPending,
    /// The database could not answer, or the outcome is unknown. Sending the request
    /// again is safe.
    Unavailable,
    /// A bug, or a panic. It says nothing of its cause.
    Internal,
}

/// The members of RFC 9457 that this API uses.
#[derive(Serialize)]
struct Document<'a> {
    #[serde(rename = "type")]
    kind: String,
    title: &'a str,
    status: u16,
}

impl Problem {
    /// The status, the section of the API's document, and the title.
    fn parts(self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "The credential is missing, unknown or revoked",
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not-found", "No such resource"),
            Self::MethodNotAllowed => (
                StatusCode::METHOD_NOT_ALLOWED,
                "method-not-allowed",
                "This route does not take that method",
            ),
            Self::MalformedKey => (
                StatusCode::BAD_REQUEST,
                "malformed-key",
                "The Idempotency-Key header is missing or malformed",
            ),
            Self::MalformedBody => (
                StatusCode::BAD_REQUEST,
                "malformed-body",
                "The body is not what this route takes",
            ),
            Self::BodyTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "body-too-large",
                "The body is too large",
            ),
            Self::KeyReused => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "key-reused",
                "This idempotency key was used for a different request",
            ),
            Self::UnknownCurrency => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown-currency",
                "The service does not know this currency",
            ),
            Self::IllegalMove => (
                StatusCode::CONFLICT,
                "illegal-move",
                "The payment's status does not allow this",
            ),
            Self::AmountAboveLimit => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "amount-above-limit",
                "The amount is above what the payment allows",
            ),
            Self::AuthorizationRequested => (
                StatusCode::CONFLICT,
                "authorization-requested",
                "An authorization of this payment was requested already",
            ),
            Self::CaptureOrVoidPending => (
                StatusCode::CONFLICT,
                "capture-or-void-pending",
                "A capture or a void of this payment waits for the provider",
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "The service could not finish this: send the request again",
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal-error",
                "Something went wrong on the server",
            ),
        }
    }

    #[must_use]
    pub fn status(self) -> StatusCode {
        self.parts().0
    }

    /// The body, the same bytes every time.
    #[must_use]
    pub fn body(self) -> String {
        let (status, section, title) = self.parts();
        serde_json::to_string(&Document {
            kind: format!("{TYPE_BASE}{section}"),
            title,
            status: status.as_u16(),
        })
        .expect("strings and a number always serialize")
    }

    /// This problem as a final answer, for a key's record to hold (ADR-0026).
    #[must_use]
    pub(super) fn answer(self) -> Answer {
        Answer {
            status: i16::try_from(self.status().as_u16()).expect("an HTTP status fits"),
            body: self.body(),
        }
    }

    /// This problem, whose cause goes to the log, because its answer says nothing of it.
    /// The line holds the error and what caused it, and nothing of the request, so no
    /// header can reach it (ADR-0024).
    #[must_use]
    pub(super) fn because(self, error: &dyn Error) -> Self {
        let mut chain = error.to_string();
        let mut cause = error.source();
        while let Some(next) = cause {
            chain.push_str(": ");
            chain.push_str(&next.to_string());
            cause = next.source();
        }
        tracing::error!(status = self.status().as_u16(), "{chain}");
        self
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut response = (
            self.status(),
            [(header::CONTENT_TYPE, "application/problem+json")],
            self.body(),
        )
            .into_response();
        if self == Self::Unauthorized {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every problem. The match stops a new one from compiling until it is added here.
    const ALL: [Problem; 14] = [
        Problem::Unauthorized,
        Problem::NotFound,
        Problem::MethodNotAllowed,
        Problem::MalformedKey,
        Problem::MalformedBody,
        Problem::BodyTooLarge,
        Problem::KeyReused,
        Problem::UnknownCurrency,
        Problem::IllegalMove,
        Problem::AmountAboveLimit,
        Problem::AuthorizationRequested,
        Problem::CaptureOrVoidPending,
        Problem::Unavailable,
        Problem::Internal,
    ];

    fn _every_problem_is_listed(problem: Problem) {
        match problem {
            Problem::Unauthorized
            | Problem::NotFound
            | Problem::MethodNotAllowed
            | Problem::MalformedKey
            | Problem::MalformedBody
            | Problem::BodyTooLarge
            | Problem::KeyReused
            | Problem::UnknownCurrency
            | Problem::IllegalMove
            | Problem::AmountAboveLimit
            | Problem::AuthorizationRequested
            | Problem::CaptureOrVoidPending
            | Problem::Unavailable
            | Problem::Internal => {}
        }
    }

    #[test]
    fn every_problem_has_a_section_in_the_document_its_type_points_to() {
        let document = include_str!("../../../../docs/api.md");
        for problem in ALL {
            let (_, section, _) = problem.parts();
            let heading = format!("### {section}");
            assert!(
                document.lines().any(|line| line == heading),
                "docs/api.md has no {heading}"
            );
        }
    }

    #[test]
    fn a_body_is_json_that_agrees_with_its_status() {
        for problem in ALL {
            let body: serde_json::Value =
                serde_json::from_str(&problem.body()).expect("a body is JSON");
            assert_eq!(body["status"], problem.status().as_u16(), "{problem:?}");
            let kind = body["type"].as_str().expect("a type is a string");
            assert!(kind.starts_with("https://"), "not absolute: {kind}");
            assert!(body["title"].is_string(), "{problem:?}");
        }
    }

    #[test]
    fn no_two_problems_share_a_type() {
        let mut sections: Vec<&str> = ALL.iter().map(|problem| problem.parts().1).collect();
        sections.sort_unstable();
        sections.dedup();
        assert_eq!(sections.len(), ALL.len());
    }
}
