// Author: Nicola Antonello

//! The PSP over HTTP: the client that reads a payment's history (ADR-0031, ADR-0036),
//! and the service's own reading of the wire format (ADR-0038). The fake speaks the same
//! format with types of its own, so the two share the format and no code (ADR-0029).
//!
//! An answer wrong as a whole is [`Unreadable`], and nothing of it is applied. A fact
//! that the service cannot place keeps why, and stops its payment there (ADR-0041).
//! Fields that the service does not know are ignored.
//!
//! The lints are the same as in `money`: this module reads amounts.
#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::accounts::Psp;
use crate::ids::PaymentId;
use crate::money::PositiveAmount;
use crate::times::BusinessTime;

/// The most bytes that a history's body may hold: far above any payment's history, and
/// a bound on what a broken PSP can make the service hold in memory (ADR-0041).
pub const MAX_BODY: usize = 1_048_576;

/// What a failed transport leaves as the source of an error.
type BoxError = Box<dyn StdError + Send + Sync>;

/// Reads one PSP's histories. Its client never retries by itself, so that nothing below
/// the sweep retries (ADR-0032, ADR-0036), and a timeout bounds each call.
pub struct PspClient {
    psp: Psp,
    /// `http://` and the PSP's authority, without a trailing slash.
    base: String,
    /// `Bearer` and the service's key, marked sensitive.
    authorization: HeaderValue,
    timeout: Duration,
    client: Client<HttpConnector, Full<Bytes>>,
}

// Written by hand, so that no log can show the key.
impl fmt::Debug for PspClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PspClient")
            .field("psp", &self.psp)
            .field("base", &self.base)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// Why a client could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidClient {
    /// The PSP's address is `http://` and an authority, such as `http://127.0.0.1:8080`,
    /// with no path: TLS belongs to deployment (ADR-0036).
    #[error("{0:?} is not http:// and an authority")]
    Address(String),
    /// The key cannot be sent in a header.
    #[error("the API key cannot be sent in a header")]
    Key,
}

/// Why a history could not be read. Nothing was applied, and the sweep reads it again
/// (ADR-0032).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// No whole answer: the connection was refused, or broke.
    #[error("the PSP gave no answer")]
    NoAnswer {
        #[source]
        source: BoxError,
    },
    /// The timeout ended the call (ADR-0036).
    #[error("the PSP did not answer in time")]
    TimedOut,
    /// A 429, or a 503 with `Retry-After`: call again later, and not before
    /// `retry_after` seconds if the PSP said (ADR-0032).
    #[error("the PSP asked to be called again later")]
    TryLater { retry_after: Option<u32> },
    /// Any other status than 200 and 404.
    #[error("the PSP answered {status}")]
    Status { status: StatusCode },
    /// A 200 whose body is not the payment's history (ADR-0041).
    #[error("the PSP's answer is not the payment's history")]
    Unreadable {
        #[source]
        source: Unreadable,
    },
}

impl PspClient {
    /// A client of `psp` at `base_url`, whose calls carry `api_key` and wait at most
    /// `timeout` (ADR-0036).
    ///
    /// # Errors
    ///
    /// See [`InvalidClient`].
    pub fn new(
        psp: Psp,
        base_url: &str,
        api_key: &str,
        timeout: Duration,
    ) -> Result<Self, InvalidClient> {
        let base = base_url.trim_end_matches('/');
        let plain = base.parse::<Uri>().is_ok_and(|uri| {
            uri.scheme_str() == Some("http")
                && uri.authority().is_some()
                && matches!(uri.path(), "" | "/")
                && uri.query().is_none()
        });
        if !plain {
            return Err(InvalidClient::Address(base_url.to_owned()));
        }
        let mut authorization =
            HeaderValue::try_from(format!("Bearer {api_key}")).map_err(|_| InvalidClient::Key)?;
        authorization.set_sensitive(true);
        Ok(Self {
            psp,
            base: base.to_owned(),
            authorization,
            timeout,
            client: Client::builder(TokioExecutor::new())
                .retry_canceled_requests(false)
                .build_http(),
        })
    }

    /// The PSP that this client reads.
    #[must_use]
    pub fn psp(&self) -> Psp {
        self.psp
    }

    /// The PSP's history of a payment: every fact that it holds about it, in its order
    /// (ADR-0031). A 404 means that the PSP holds nothing for the payment (ADR-0032),
    /// which reads as a history without facts.
    ///
    /// # Errors
    ///
    /// See [`ReadError`].
    pub async fn history(&self, payment: PaymentId) -> Result<History, ReadError> {
        let request = self
            .request(Method::GET, &format!("/v1/payments/{payment}/history"))
            .body(Full::new(Bytes::new()))
            .expect("an address checked when the client was made, and a UUID in the path");
        let answer = self.call(request).await?;
        // A body above the bound is unreadable, whatever its status.
        let Some(body) = answer.body else {
            return Err(ReadError::Unreadable {
                source: Unreadable::TooLarge,
            });
        };
        let retry_after = answer.retry_after;
        match answer.status {
            StatusCode::OK => {
                read_history(payment, &body).map_err(|source| ReadError::Unreadable { source })
            }
            StatusCode::NOT_FOUND => Ok(History {
                payment,
                facts: Vec::new(),
            }),
            StatusCode::TOO_MANY_REQUESTS => Err(ReadError::TryLater { retry_after }),
            StatusCode::SERVICE_UNAVAILABLE if retry_after.is_some() => {
                Err(ReadError::TryLater { retry_after })
            }
            status => Err(ReadError::Status { status }),
        }
    }

    /// A call to the PSP at `path`, carrying the service's key. Its body comes next.
    fn request(&self, method: Method, path: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, self.authorization.clone())
    }

    /// Makes one call, bounded by the timeout around it and by [`MAX_BODY`], and never
    /// retried (ADR-0032, ADR-0036).
    async fn call(&self, request: Request<Full<Bytes>>) -> Result<Answer, Unanswered> {
        let call = async {
            let response = self
                .client
                .request(request)
                .await
                .map_err(|source| Unanswered::NoAnswer(source.into()))?;
            let status = response.status();
            let retry_after = retry_after(response.headers());
            let body = match Limited::new(response.into_body(), MAX_BODY).collect().await {
                Ok(collected) => Some(collected.to_bytes()),
                Err(source) if source.is::<LengthLimitError>() => None,
                Err(source) => return Err(Unanswered::NoAnswer(source)),
            };
            Ok(Answer {
                status,
                retry_after,
                body,
            })
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| Unanswered::TimedOut)?
    }
}

/// What the PSP answered to one call.
struct Answer {
    status: StatusCode,
    retry_after: Option<u32>,
    /// The body, or none when it held more than [`MAX_BODY`] bytes.
    body: Option<Bytes>,
}

/// Why a call ended without a whole answer.
enum Unanswered {
    /// The connection was refused, or broke.
    NoAnswer(BoxError),
    /// The timeout ended the call (ADR-0036).
    TimedOut,
}

impl From<Unanswered> for ReadError {
    fn from(unanswered: Unanswered) -> Self {
        match unanswered {
            Unanswered::NoAnswer(source) => Self::NoAnswer { source },
            Unanswered::TimedOut => Self::TimedOut,
        }
    }
}

/// A `Retry-After` in seconds. The HTTP-date form reads as none.
fn retry_after(headers: &HeaderMap) -> Option<u32> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A payment's history in the service's own types (ADR-0031, ADR-0038): its facts at
/// positions 1, 2, 3 and so on, in the PSP's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    payment: PaymentId,
    facts: Vec<Fact>,
}

impl History {
    #[must_use]
    pub fn payment(&self) -> PaymentId {
        self.payment
    }

    #[must_use]
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }
}

/// One fact, at its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// The PSP's own ID for it, of the shape of ADR-0037.
    id: String,
    position: i32,
    detail: Result<FactDetail, Unplaced>,
}

impl Fact {
    /// The PSP's own ID for the fact, unique across the PSP (ADR-0031).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Where the fact stands in the history, counted from 1.
    #[must_use]
    pub fn position(&self) -> i32 {
        self.position
    }

    /// What the fact says, or why the service cannot place it (ADR-0041).
    ///
    /// # Errors
    ///
    /// [`Unplaced`] for a fact that the service cannot place.
    pub fn detail(&self) -> Result<FactDetail, &Unplaced> {
        self.detail.as_ref().copied()
    }
}

/// What a fact says, in the service's terms: the step that it makes, or the refusal that
/// closes a request without one (ADR-0037). The wire calls each by the PSP's own word,
/// in the past tense (ADR-0038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactDetail {
    /// An authorization, for at most the amount asked, and when it lapses.
    Authorized {
        request: Uuid,
        amount: PositiveAmount,
        expires_at: BusinessTime,
    },
    Declined {
        request: Uuid,
    },
    Captured {
        request: Uuid,
        amount: PositiveAmount,
        captured_at: BusinessTime,
    },
    CaptureRefused {
        request: Uuid,
    },
    Voided {
        request: Uuid,
    },
    VoidRefused {
        request: Uuid,
    },
    /// The authorization lapsed, which only the PSP decides.
    Expired,
    /// The capture settled, which only the PSP decides.
    Settled,
    Refunded {
        request: Uuid,
        amount: PositiveAmount,
        refunded_at: BusinessTime,
    },
    RefundRefused {
        request: Uuid,
    },
}

impl FactDetail {
    /// The kind that the service stores.
    #[must_use]
    pub fn kind(self) -> FactKind {
        match self {
            Self::Authorized { .. } => FactKind::Authorize,
            Self::Declined { .. } => FactKind::Decline,
            Self::Captured { .. } => FactKind::Capture,
            Self::CaptureRefused { .. } => FactKind::CaptureRefused,
            Self::Voided { .. } => FactKind::Void,
            Self::VoidRefused { .. } => FactKind::VoidRefused,
            Self::Expired => FactKind::Expire,
            Self::Settled => FactKind::Settle,
            Self::Refunded { .. } => FactKind::Refund,
            Self::RefundRefused { .. } => FactKind::RefundRefused,
        }
    }

    /// The request that the fact decides, if it decides one.
    #[must_use]
    pub fn request(self) -> Option<Uuid> {
        match self {
            Self::Authorized { request, .. }
            | Self::Declined { request }
            | Self::Captured { request, .. }
            | Self::CaptureRefused { request }
            | Self::Voided { request }
            | Self::VoidRefused { request }
            | Self::Refunded { request, .. }
            | Self::RefundRefused { request } => Some(request),
            Self::Expired | Self::Settled => None,
        }
    }

    /// What the fact moved: the amount of an authorization, a capture or a refund, and
    /// 0 for the other kinds, as `payment_facts_amount_check` stores it.
    #[must_use]
    pub fn amount(self) -> i64 {
        match self {
            Self::Authorized { amount, .. }
            | Self::Captured { amount, .. }
            | Self::Refunded { amount, .. } => amount.get(),
            Self::Declined { .. }
            | Self::CaptureRefused { .. }
            | Self::Voided { .. }
            | Self::VoidRefused { .. }
            | Self::Expired
            | Self::Settled
            | Self::RefundRefused { .. } => 0,
        }
    }
}

/// The kind of a stored fact: the step that it makes, or one of the three refusals
/// that close a request without one (ADR-0037). A closed set of labels, parsed once
/// (ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    Authorize,
    Decline,
    Capture,
    CaptureRefused,
    Void,
    VoidRefused,
    Expire,
    Settle,
    Refund,
    RefundRefused,
}

impl FactKind {
    /// Every kind, in the order of `payment_facts_kind_check`.
    pub const ALL: [Self; 10] = [
        Self::Authorize,
        Self::Decline,
        Self::Capture,
        Self::CaptureRefused,
        Self::Void,
        Self::VoidRefused,
        Self::Expire,
        Self::Settle,
        Self::Refund,
        Self::RefundRefused,
    ];

    /// The label that the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authorize => "authorize",
            Self::Decline => "decline",
            Self::Capture => "capture",
            Self::CaptureRefused => "capture_refused",
            Self::Void => "void",
            Self::VoidRefused => "void_refused",
            Self::Expire => "expire",
            Self::Settle => "settle",
            Self::Refund => "refund",
            Self::RefundRefused => "refund_refused",
        }
    }

    /// Parses the label that the database stores.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == label)
    }
}

/// Why the service cannot place a fact: its payment stops there with `no_place`
/// (ADR-0041).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unplaced {
    /// A kind that the service does not know, or a field missing or of another type.
    #[error("{0}")]
    Shape(String),
    /// An amount outside 1 to the cap of ADR-0003.
    #[error("amount {0} is outside 1 to the cap")]
    Amount(i64),
    /// A time that is not RFC 3339, or cannot be stored (ADR-0022).
    #[error("{0:?} is not a time that can be stored")]
    Time(String),
}

/// Why a 200 is not the payment's history (ADR-0041). Nothing of it is applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unreadable {
    #[error("the body holds more than {MAX_BODY} bytes")]
    TooLarge,
    /// Not the JSON of a history.
    #[error("the body is not a history: {0}")]
    Shape(String),
    #[error("the history is of payment {found}")]
    OtherPayment { found: Uuid },
    /// The facts are not numbered 1, 2, 3 and so on, in order.
    #[error("fact {expected} of the history has position {found}")]
    Position { expected: i32, found: u32 },
    /// A fact's ID is not of the shape of ADR-0037.
    #[error("the fact at position {position} has an ID of another shape")]
    FactId { position: i32 },
}

/// A history as the wire carries it. Fields that the service does not know, such as
/// the requests that the sender looks up, are ignored (ADR-0041).
#[derive(Deserialize)]
struct WireHistory {
    payment_id: Uuid,
    facts: Vec<WireFact>,
}

/// A fact's place in the history, which an answer must get right as a whole, and the
/// rest of its fields, read one fact at a time.
#[derive(Deserialize)]
struct WireFact {
    id: String,
    position: u32,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

/// What a fact says, in the PSP's words (ADR-0038).
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireDetail {
    Authorized {
        request_id: Uuid,
        amount: i64,
        expires_at: String,
    },
    Declined {
        request_id: Uuid,
    },
    Captured {
        request_id: Uuid,
        amount: i64,
        captured_at: String,
    },
    CaptureRefused {
        request_id: Uuid,
    },
    Voided {
        request_id: Uuid,
    },
    VoidRefused {
        request_id: Uuid,
    },
    Expired,
    Settled,
    Refunded {
        request_id: Uuid,
        amount: i64,
        refunded_at: String,
    },
    RefundRefused {
        request_id: Uuid,
    },
}

/// Reads a 200's body as the history of `payment` (ADR-0041).
///
/// # Errors
///
/// [`Unreadable`] for an answer wrong as a whole.
fn read_history(payment: PaymentId, body: &[u8]) -> Result<History, Unreadable> {
    let wire: WireHistory =
        serde_json::from_slice(body).map_err(|err| Unreadable::Shape(err.to_string()))?;
    if wire.payment_id != payment.get() {
        return Err(Unreadable::OtherPayment {
            found: wire.payment_id,
        });
    }
    let facts = (1_i32..)
        .zip(wire.facts)
        .map(|(expected, fact)| {
            if i32::try_from(fact.position) != Ok(expected) {
                return Err(Unreadable::Position {
                    expected,
                    found: fact.position,
                });
            }
            if !is_fact_id(&fact.id) {
                return Err(Unreadable::FactId { position: expected });
            }
            Ok(Fact {
                id: fact.id,
                position: expected,
                detail: detail(fact.rest),
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(History { payment, facts })
}

/// Whether a fact's ID has the shape of ADR-0037, which `payment_facts_psp_fact_id_check`
/// also holds: one to 255 of `[0-9A-Za-z_-]`, so never a `.`.
fn is_fact_id(id: &str) -> bool {
    (1..=255).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// One fact's fields after its ID and position, in the service's terms.
fn detail(fields: Map<String, Value>) -> Result<FactDetail, Unplaced> {
    let wire = serde_json::from_value::<WireDetail>(Value::Object(fields))
        .map_err(|err| Unplaced::Shape(err.to_string()))?;
    Ok(match wire {
        WireDetail::Authorized {
            request_id,
            amount,
            expires_at,
        } => FactDetail::Authorized {
            request: request_id,
            amount: amount_of(amount)?,
            expires_at: time_of(&expires_at)?,
        },
        WireDetail::Declined { request_id } => FactDetail::Declined {
            request: request_id,
        },
        WireDetail::Captured {
            request_id,
            amount,
            captured_at,
        } => FactDetail::Captured {
            request: request_id,
            amount: amount_of(amount)?,
            captured_at: time_of(&captured_at)?,
        },
        WireDetail::CaptureRefused { request_id } => FactDetail::CaptureRefused {
            request: request_id,
        },
        WireDetail::Voided { request_id } => FactDetail::Voided {
            request: request_id,
        },
        WireDetail::VoidRefused { request_id } => FactDetail::VoidRefused {
            request: request_id,
        },
        WireDetail::Expired => FactDetail::Expired,
        WireDetail::Settled => FactDetail::Settled,
        WireDetail::Refunded {
            request_id,
            amount,
            refunded_at,
        } => FactDetail::Refunded {
            request: request_id,
            amount: amount_of(amount)?,
            refunded_at: time_of(&refunded_at)?,
        },
        WireDetail::RefundRefused { request_id } => FactDetail::RefundRefused {
            request: request_id,
        },
    })
}

fn amount_of(minor: i64) -> Result<PositiveAmount, Unplaced> {
    PositiveAmount::new(minor).map_err(|_| Unplaced::Amount(minor))
}

/// An RFC 3339 time, as PostgreSQL stores it: anything finer than a microsecond is
/// dropped (ADR-0022).
fn time_of(text: &str) -> Result<BusinessTime, Unplaced> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|time| BusinessTime::new(time.with_timezone(&Utc)).ok())
        .ok_or_else(|| Unplaced::Time(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PAYMENT: Uuid = Uuid::from_u128(0x019b_76da_a800_7000_8000_0000_0000_0002);
    const REQUEST: Uuid = Uuid::from_u128(0x019b_76da_a800_7000_8000_0000_0000_0001);

    fn payment() -> PaymentId {
        PaymentId::try_from(PAYMENT).expect("a UUIDv7")
    }

    fn amount(minor: i64) -> PositiveAmount {
        PositiveAmount::new(minor).expect("a positive amount")
    }

    /// 2026-01-01T00:00:00.123456Z.
    fn at() -> BusinessTime {
        let time = DateTime::from_timestamp_micros(1_767_225_600_123_456).expect("in range");
        BusinessTime::new(time).expect("a time that can be stored")
    }

    /// A history of this payment with these facts, as the wire carries it.
    fn body(facts: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"payment_id": PAYMENT, "facts": facts, "requests": []}))
            .expect("JSON")
    }

    /// The facts of a history with one fact, at position 1, with these fields.
    fn one(fields: Value) -> Value {
        let mut fact = json!({"id": "fct_1", "position": 1});
        let object = fact.as_object_mut().expect("an object");
        for (name, value) in fields.as_object().expect("an object") {
            object.insert(name.clone(), value.clone());
        }
        json!([fact])
    }

    /// What the one fact of such a history says.
    fn detail_of(fields: Value) -> Result<FactDetail, Unplaced> {
        let history = read_history(payment(), &body(one(fields))).expect("readable");
        let [fact] = history.facts() else {
            panic!("one fact")
        };
        fact.detail().map_err(Clone::clone)
    }

    #[test]
    fn each_fact_reads_in_the_services_terms() {
        let request = REQUEST;
        let time = "2026-01-01T00:00:00.123456Z";
        for (fields, expected, kind, moved) in [
            (
                json!({"kind": "authorized", "request_id": request, "amount": 8_000,
                       "expires_at": time}),
                FactDetail::Authorized {
                    request,
                    amount: amount(8_000),
                    expires_at: at(),
                },
                "authorize",
                8_000,
            ),
            (
                json!({"kind": "declined", "request_id": request}),
                FactDetail::Declined { request },
                "decline",
                0,
            ),
            (
                json!({"kind": "captured", "request_id": request, "amount": 7_000,
                       "captured_at": time}),
                FactDetail::Captured {
                    request,
                    amount: amount(7_000),
                    captured_at: at(),
                },
                "capture",
                7_000,
            ),
            (
                json!({"kind": "capture_refused", "request_id": request}),
                FactDetail::CaptureRefused { request },
                "capture_refused",
                0,
            ),
            (
                json!({"kind": "voided", "request_id": request}),
                FactDetail::Voided { request },
                "void",
                0,
            ),
            (
                json!({"kind": "void_refused", "request_id": request}),
                FactDetail::VoidRefused { request },
                "void_refused",
                0,
            ),
            (json!({"kind": "expired"}), FactDetail::Expired, "expire", 0),
            (json!({"kind": "settled"}), FactDetail::Settled, "settle", 0),
            (
                json!({"kind": "refunded", "request_id": request, "amount": 100,
                       "refunded_at": time}),
                FactDetail::Refunded {
                    request,
                    amount: amount(100),
                    refunded_at: at(),
                },
                "refund",
                100,
            ),
            (
                json!({"kind": "refund_refused", "request_id": request}),
                FactDetail::RefundRefused { request },
                "refund_refused",
                0,
            ),
        ] {
            let read = detail_of(fields).expect("a fact that can be placed");
            assert_eq!(read, expected);
            assert_eq!(read.kind().as_str(), kind);
            assert_eq!(read.amount(), moved, "{kind}");
            let decides = !matches!(read, FactDetail::Expired | FactDetail::Settled);
            assert_eq!(read.request(), decides.then_some(request), "{kind}");
        }
    }

    #[test]
    fn a_kind_reads_back_from_its_label() {
        for kind in FactKind::ALL {
            assert_eq!(FactKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(FactKind::parse("captured"), None);
    }

    #[test]
    fn facts_read_in_order_and_unknown_fields_are_ignored() {
        let facts = json!([
            {"id": "fct_1", "position": 1, "kind": "authorized", "request_id": REQUEST,
             "amount": 8_000, "expires_at": "2026-01-01T00:00:00.123456Z",
             "network_reference": "abc"},
            {"id": "fct-2_B", "position": 2, "kind": "expired", "reason": "lapsed"},
        ]);
        let mut wire = json!({"payment_id": PAYMENT, "facts": facts, "livemode": false});
        wire["requests"] = json!([{"id": REQUEST, "anything": true}]);
        let history =
            read_history(payment(), &serde_json::to_vec(&wire).expect("JSON")).expect("readable");
        assert_eq!(history.payment(), payment());
        let read: Vec<_> = history
            .facts()
            .iter()
            .map(|fact| {
                (
                    fact.id(),
                    fact.position(),
                    fact.detail().map(FactDetail::kind),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                ("fct_1", 1, Ok(FactKind::Authorize)),
                ("fct-2_B", 2, Ok(FactKind::Expire)),
            ]
        );
    }

    #[test]
    fn a_fact_that_cannot_be_placed_keeps_why() {
        let request = REQUEST;
        let time = "2026-01-01T00:00:00Z";
        for fields in [
            json!({"kind": "chargeback", "request_id": request}),
            json!({"request_id": request}),
            json!({"kind": "captured", "request_id": request, "captured_at": time}),
            json!({"kind": "captured", "request_id": "not a uuid", "amount": 1,
                   "captured_at": time}),
            json!({"kind": "refunded", "request_id": request, "amount": "100",
                   "refunded_at": time}),
        ] {
            assert!(
                matches!(detail_of(fields.clone()), Err(Unplaced::Shape(_))),
                "{fields}"
            );
        }
        for minor in [0, -1, 1_000_000_000_000_001] {
            let fields = json!({"kind": "captured", "request_id": request, "amount": minor,
                                "captured_at": time});
            assert_eq!(detail_of(fields), Err(Unplaced::Amount(minor)));
        }
        // Every year that RFC 3339 can write can be stored, so only the form refuses one.
        for text in [
            "yesterday",
            "2026-01-01 00:00:00",
            "2026-01-01T00:00:00",
            "2026-13-01T00:00:00Z",
        ] {
            let fields = json!({"kind": "authorized", "request_id": request, "amount": 1,
                                "expires_at": text});
            assert_eq!(detail_of(fields), Err(Unplaced::Time(text.to_owned())));
        }
    }

    #[test]
    fn a_time_keeps_whole_microseconds_in_any_offset() {
        let fields = |text: &str| {
            json!({"kind": "authorized", "request_id": REQUEST, "amount": 1,
                   "expires_at": text})
        };
        for text in [
            "2026-01-01T00:00:00.123456Z",
            "2026-01-01T00:00:00.1234569Z",
            "2026-01-01T01:00:00.123456+01:00",
        ] {
            let expected = FactDetail::Authorized {
                request: REQUEST,
                amount: amount(1),
                expires_at: at(),
            };
            assert_eq!(detail_of(fields(text)), Ok(expected), "{text}");
        }
    }

    #[test]
    fn an_answer_wrong_as_a_whole_is_unreadable() {
        let fact =
            |id: &str, position: u32| json!({"id": id, "position": position, "kind": "settled"});
        let read = |body: &[u8]| read_history(payment(), body);
        for body in [
            b"not json".to_vec(),
            b"{}".to_vec(),
            serde_json::to_vec(&json!({"payment_id": PAYMENT, "facts": {}})).expect("JSON"),
            body(json!([{"position": 1, "kind": "settled"}])),
            body(json!([{"id": 1, "position": 1, "kind": "settled"}])),
            body(json!([{"id": "fct_1", "position": -1, "kind": "settled"}])),
            body(json!(["fct_1"])),
        ] {
            assert!(
                matches!(read(&body), Err(Unreadable::Shape(_))),
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
        let other = Uuid::from_u128(3);
        assert_eq!(
            read(&serde_json::to_vec(&json!({"payment_id": other, "facts": []})).expect("JSON")),
            Err(Unreadable::OtherPayment { found: other })
        );
        for (facts, expected, found) in [
            (json!([fact("a", 2)]), 1, 2),
            (json!([fact("a", 0)]), 1, 0),
            (json!([fact("a", 1), fact("b", 3)]), 2, 3),
            (json!([fact("a", 1), fact("b", 1)]), 2, 1),
            (json!([fact("a", 2), fact("b", 1)]), 1, 2),
        ] {
            assert_eq!(
                read(&body(facts)),
                Err(Unreadable::Position { expected, found })
            );
        }
        let long = "f".repeat(256);
        for id in ["", "fct.1", "fct 1", "fct/1", "fäct", long.as_str()] {
            assert_eq!(
                read(&body(json!([fact("fct_1", 1), fact(id, 2)]))),
                Err(Unreadable::FactId { position: 2 }),
                "{id:?}"
            );
        }
        assert!(read(&body(json!([fact(&"f".repeat(255), 1)]))).is_ok());
    }

    #[test]
    fn a_retry_after_is_read_in_seconds_only() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        for (value, read) in [
            ("120", Some(120)),
            (" 7 ", Some(7)),
            ("Wed, 21 Oct 2015 07:28:00 GMT", None),
            ("-1", None),
        ] {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static(value));
            assert_eq!(retry_after(&headers), read, "{value}");
        }
    }

    #[test]
    fn a_client_needs_a_plain_http_address_and_a_key_for_a_header() {
        let make = |address: &str, key: &str| {
            PspClient::new(Psp::Mock, address, key, Duration::from_secs(1))
                .map(|client| client.base)
        };
        // A path follows the base, so the base drops a final slash: `//v1/...` is not
        // `/v1/...`.
        for (address, base) in [
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://psp.test/", "http://psp.test"),
            ("http://psp.test", "http://psp.test"),
        ] {
            assert_eq!(make(address, "key"), Ok(base.to_owned()), "{address}");
        }
        for address in [
            "https://psp.test",
            "psp.test",
            "http://psp.test/v1",
            "http://psp.test/?a=1",
            "http:///path",
            "",
        ] {
            assert_eq!(
                make(address, "key"),
                Err(InvalidClient::Address(address.to_owned())),
                "{address}"
            );
        }
        assert_eq!(
            make("http://psp.test", "line\nbreak"),
            Err(InvalidClient::Key)
        );
    }

    #[test]
    fn a_client_never_shows_its_key() {
        let client = PspClient::new(
            Psp::Mock,
            "http://psp.test",
            "mock_sk_secret",
            Duration::from_secs(1),
        )
        .expect("a client");
        let shown = format!("{client:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("http://psp.test"), "{shown}");
        // Nor does the header that each request carries, if a request is ever shown.
        let header = format!("{:?}", client.authorization);
        assert!(!header.contains("secret"), "{header}");
    }
}
