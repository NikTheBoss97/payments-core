// Author: Nicola Antonello

//! Errors as a client sees them: `application/problem+json` (RFC 9457, ADR-0023).
//!
//! A problem's text is fixed here, so it can never reveal SQL, a constraint or another
//! merchant's data. Its body comes from one pure function, because a key's record stores
//! the bytes that a replay returns (ADR-0026).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

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
