// Author: Nicola Antonello

//! A merchant's requests to the PSP (ADR-0028, ADR-0039).
//!
//! A request is recorded here, not sent: under the payment's lock and its merchant, in
//! the transaction that stores its 202 under the key. The sender sends it after that
//! commit. Some requests are refused at once: on a stopped payment, by the table of
//! moves, by an amount's limit, or because they conflict with a request already
//! recorded. A request is in flight until a fact of the PSP's decides it (ADR-0037).
//!
//! The lints are the same as in `money`: this module handles amounts.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::fmt;

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::idempotency::Decided;
use crate::ids::{PaymentId, RequestId};
use crate::lifecycle::{self, Event, IllegalTransition, PaymentStatus, Refused};
use crate::money::PositiveAmount;
use crate::payments::{Failed, begin_locked, commit_unit, store_sealed, stored_time};
use crate::psp::FactKind;
use crate::sender::Answer;
use crate::tenancy::Scope;
use crate::times::BusinessTime;

/// What a request asks the PSP to do: a closed set of labels, parsed once (ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Authorize,
    Capture,
    Refund,
    Void,
}

impl RequestKind {
    /// The label that the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authorize => "authorize",
            Self::Capture => "capture",
            Self::Refund => "refund",
            Self::Void => "void",
        }
    }

    /// The kind with this label, if there is one.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "authorize" => Some(Self::Authorize),
            "capture" => Some(Self::Capture),
            "refund" => Some(Self::Refund),
            "void" => Some(Self::Void),
            _ => None,
        }
    }
}

impl fmt::Display for RequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// What a merchant asks of the PSP, with the amount that its kind takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// An authorization, of at most the payment's amount.
    Authorize(PositiveAmount),
    /// A capture, of at most the amount authorized.
    Capture(PositiveAmount),
    /// A refund, of at most what stays refundable once the refunds in flight are
    /// counted.
    Refund(PositiveAmount),
    /// A void of the authorization, which asks for no amount.
    Void,
}

impl Ask {
    fn kind(self) -> RequestKind {
        match self {
            Self::Authorize(_) => RequestKind::Authorize,
            Self::Capture(_) => RequestKind::Capture,
            Self::Refund(_) => RequestKind::Refund,
            Self::Void => RequestKind::Void,
        }
    }

    /// The amount asked, as `payment_requests` stores it: nothing for a void.
    fn amount(self) -> i64 {
        match self {
            Self::Authorize(amount) | Self::Capture(amount) | Self::Refund(amount) => amount.get(),
            Self::Void => 0,
        }
    }
}

/// The command that records a request. Its ID identifies the command, and the request
/// at the PSP (ADR-0028).
#[derive(Debug)]
pub struct RecordRequest {
    pub request: RequestId,
    pub payment: PaymentId,
    pub ask: Ask,
}

impl RecordRequest {
    /// What this command asks, in the terms that a retry compares (ADR-0019).
    fn asked(&self) -> Asked {
        Asked {
            payment: self.payment.get(),
            kind: self.ask.kind(),
            amount: self.ask.amount(),
        }
    }

    fn recorded(&self) -> RequestRecorded {
        RequestRecorded {
            request: self.request.get(),
            payment: self.payment,
        }
    }
}

/// A recorded request, as its 202 names it (ADR-0033).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestRecorded {
    pub request: Uuid,
    pub payment: PaymentId,
}

/// What a [`RecordRequest`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// This call recorded the request.
    Recorded(RequestRecorded),
    /// An earlier call recorded it, with the same content.
    AlreadyRecorded(RequestRecorded),
}

/// A conflict with a request already recorded, or with a stop (ADR-0028, ADR-0039,
/// ADR-0042).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Conflict {
    /// The catch-up stopped the payment for an operator. Money that the PSP moved now
    /// could not reach the ledger, so the payment takes no request, whatever its status.
    #[error("this payment is stopped until an operator looks at it")]
    Stopped,
    /// The payment has a request to authorize it, decided or not: a payment has one
    /// authorization.
    #[error("an authorization of this payment was requested already")]
    AuthorizationRequested,
    /// A capture or a void waits for the PSP's decision, which another capture or void
    /// would race.
    #[error("a {0} of this payment waits for the PSP's decision")]
    Pending(RequestKind),
}

/// Why a command was refused. Every refusal is final, and stored under its key
/// (ADR-0026, ADR-0039).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The table of moves, or an amount's limit.
    #[error(transparent)]
    Refused(#[from] Refused),
    /// A request already recorded.
    #[error(transparent)]
    Conflict(#[from] Conflict),
}

impl From<IllegalTransition> for Refusal {
    fn from(illegal: IllegalTransition) -> Self {
        Self::Refused(illegal.into())
    }
}

/// Why a [`RecordRequest`] was not recorded. Only [`RequestError::NotConfirmed`] may
/// have recorded it.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// No payment of this merchant has this ID.
    #[error("payment {payment} does not exist")]
    NotFound { payment: PaymentId },
    /// The table of moves, an amount's limit or a request already recorded refuses it.
    /// Nothing is recorded, and a retry is decided again (ADR-0019).
    #[error(transparent)]
    Refused(Refusal),
    /// The request's ID is recorded with other content: a bug, never retried.
    #[error("request {request} is recorded with other content")]
    Mismatch { request: Uuid },
    /// The idempotency key already holds an answer, so this attempt rolled back whole.
    /// Look the key up for the answer (ADR-0026).
    #[error("the idempotency key already holds an answer")]
    KeyTaken,
    /// The database caught what the domain types should have prevented, or refused the
    /// statement, as it refuses a request outside a merchant's scope (ADR-0037): a bug,
    /// never retried.
    #[error("request {request}: the database rejected it as invalid")]
    Invariant {
        request: Uuid,
        #[source]
        source: sqlx::Error,
    },
    /// The request may or may not be recorded. Send the identical command again later,
    /// never one with a new ID (ADR-0019).
    #[error("request {request} may or may not be recorded: send the command again")]
    NotConfirmed {
        request: Uuid,
        #[source]
        source: sqlx::Error,
    },
}

impl RequestError {
    fn new(command: &RecordRequest, failed: Failed<Refusal>) -> Self {
        let request = command.request.get();
        match failed {
            Failed::NotFound => Self::NotFound {
                payment: command.payment,
            },
            Failed::Refused(refusal) => Self::Refused(refusal),
            Failed::Mismatch => Self::Mismatch { request },
            Failed::KeyTaken => Self::KeyTaken,
            Failed::Duplicate(source) | Failed::Invariant(source) => {
                Self::Invariant { request, source }
            }
            Failed::NotConfirmed(source) => Self::NotConfirmed { request, source },
        }
    }
}

/// Records a request to the PSP, in a merchant's scope: only a merchant asks the PSP
/// for something (ADR-0037). Under a key, the seal stores the 202, or the refusal, in
/// the same transaction (ADR-0026). The PSP hears of the request only once the sender
/// sends it, after this commits (ADR-0028).
///
/// # Errors
///
/// See [`RequestError`].
pub async fn record_request<'a>(
    scope: impl Into<Scope<'a>>,
    command: &RecordRequest,
) -> Result<RequestOutcome, RequestError> {
    record(scope.into(), command)
        .await
        .map_err(|failed| RequestError::new(command, failed))
}

/// A request's unit of work, from `BEGIN` to `COMMIT`, in the order of a command's
/// (ADR-0019).
async fn record(
    scope: Scope<'_>,
    command: &RecordRequest,
) -> Result<RequestOutcome, Failed<Refusal>> {
    let mut tx = begin_locked(scope, command.payment).await?;
    match record_in(&mut tx, command).await {
        Ok(RequestOutcome::Recorded(recorded)) => {
            store_sealed(&mut tx, scope, Decided::Requested(&recorded)).await?;
            commit_unit(tx).await?;
            Ok(RequestOutcome::Recorded(recorded))
        }
        // A retry wrote nothing, so dropping the transaction ends it.
        Ok(replayed) => Ok(replayed),
        // A refusal wrote nothing. Under a key it is stored, so that a late copy of the
        // request cannot turn it into an effect (ADR-0026).
        Err(Failed::Refused(refusal)) => {
            if scope.seal().is_some() {
                store_sealed(&mut tx, scope, Decided::Refused(refusal)).await?;
                commit_unit(tx).await?;
            }
            Err(Failed::Refused(refusal))
        }
        Err(Failed::Duplicate(duplicate)) => {
            // The insert aborted the transaction, which released the lock (ADR-0018).
            // Dropping it gives its connection back before the lookup asks for one.
            drop(tx);
            look_up_again(scope, command, duplicate).await
        }
        // Any other failure aborted the transaction: drop it, because committing it
        // would report success (ADR-0014).
        Err(failed) => Err(failed),
    }
}

/// Identity first, then the one read and the decision, then the row (ADR-0019), inside
/// a transaction that the caller has begun and locked. After an error, drop it.
async fn record_in(
    tx: &mut Transaction<'_, Postgres>,
    command: &RecordRequest,
) -> Result<RequestOutcome, Failed<Refusal>> {
    // A retry that waited behind its own first attempt finds it, where the state would
    // now refuse it.
    if let Some(stored) = stored(tx, command.request.get())
        .await
        .map_err(Failed::database)?
    {
        return same_request(command, &stored);
    }
    let state = read_state(tx, command.payment)
        .await
        .map_err(Failed::database)?
        .ok_or(Failed::NotFound)?;
    decide(command.ask, &state).map_err(Failed::Refused)?;
    insert(tx, command).await.map_err(Failed::database)?;
    Ok(RequestOutcome::Recorded(command.recorded()))
}

/// A write met a duplicate key. Under the lock again, the request with the command's ID
/// decides. None means that the ID is recorded where this merchant cannot see it, or
/// that a writer skipped the lock: a bug (ADR-0019).
async fn look_up_again(
    scope: Scope<'_>,
    command: &RecordRequest,
    duplicate: sqlx::Error,
) -> Result<RequestOutcome, Failed<Refusal>> {
    let mut tx = begin_locked(scope, command.payment).await?;
    match stored(&mut tx, command.request.get())
        .await
        .map_err(Failed::database)?
    {
        Some(stored) => same_request(command, &stored),
        None => Err(Failed::Duplicate(duplicate)),
    }
}

/// The stored request is this command's, or its ID was reused for another.
fn same_request(
    command: &RecordRequest,
    stored: &Asked,
) -> Result<RequestOutcome, Failed<Refusal>> {
    if *stored == command.asked() {
        Ok(RequestOutcome::AlreadyRecorded(command.recorded()))
    } else {
        Err(Failed::Mismatch)
    }
}

/// What a request asks, as a retry compares it.
#[derive(Debug, PartialEq, Eq)]
struct Asked {
    payment: Uuid,
    kind: RequestKind,
    amount: i64,
}

/// The request with this ID, if this merchant can see it.
async fn stored(tx: &mut Transaction<'_, Postgres>, request: Uuid) -> sqlx::Result<Option<Asked>> {
    let row = sqlx::query!(
        "SELECT payment_id, kind, amount FROM payment_requests WHERE id = $1",
        request
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| Asked {
        payment: row.payment_id,
        kind: RequestKind::parse(&row.kind)
            .expect("payment_requests_kind_check allows only known kinds"),
        amount: row.amount,
    }))
}

/// What a request's one read under the lock found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestState {
    /// Whether the catch-up stopped the payment (ADR-0042).
    stopped: bool,
    status: PaymentStatus,
    /// What can still be refunded, after the payment's newest step.
    refundable: i64,
    /// The payment's amount.
    requested: PositiveAmount,
    /// The amount authorized, once there is an authorization.
    authorized: Option<PositiveAmount>,
    /// Whether the payment has a request to authorize it, decided or not.
    authorization_requested: bool,
    /// What the refunds in flight ask, together.
    refunds_in_flight: i64,
    /// The capture or void in flight. There is at most one, because each is refused
    /// while another is in flight.
    capture_or_void_in_flight: Option<RequestKind>,
}

/// The one read under the lock, in a later statement than the lock: at READ COMMITTED it
/// sees every request and fact that the lock's previous holder committed (ADR-0018).
async fn read_state(
    tx: &mut Transaction<'_, Postgres>,
    payment: PaymentId,
) -> sqlx::Result<Option<RequestState>> {
    let row = sqlx::query!(
        r#"SELECT EXISTS (SELECT 1 FROM payment_stops s WHERE s.payment_id = p.id)
                      AS "stopped!",
                  newest.to_status, newest.refundable,
                  p.amount AS requested,
                  authorized.amount AS "authorized?",
                  EXISTS (SELECT 1 FROM payment_requests r
                           WHERE r.payment_id = p.id AND r.kind = $2)
                      AS "authorization_requested!",
                  COALESCE((SELECT SUM(r.amount) FROM payment_requests r
                             WHERE r.payment_id = p.id AND r.kind = $3
                               AND NOT EXISTS (SELECT 1 FROM payment_request_decisions d
                                                WHERE d.request_id = r.id)), 0)::bigint
                      AS "refunds_in_flight!",
                  (SELECT r.kind FROM payment_requests r
                    WHERE r.payment_id = p.id AND r.kind IN ($4, $5)
                      AND NOT EXISTS (SELECT 1 FROM payment_request_decisions d
                                       WHERE d.request_id = r.id)
                    LIMIT 1) AS "capture_or_void_in_flight?"
             FROM payments p
            CROSS JOIN LATERAL (
                  SELECT t.to_status, t.refundable
                    FROM payment_transitions t
                   WHERE t.payment_id = p.id
                   ORDER BY t.seq DESC
                   LIMIT 1
                  ) newest
             LEFT JOIN payment_authorizations authorized ON authorized.payment_id = p.id
            WHERE p.id = $1"#,
        payment.get(),
        RequestKind::Authorize.as_str(),
        RequestKind::Refund.as_str(),
        RequestKind::Capture.as_str(),
        RequestKind::Void.as_str()
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| RequestState {
        stopped: row.stopped,
        status: PaymentStatus::parse(&row.to_status)
            .expect("payment_transitions_edge_check allows only known statuses"),
        refundable: row.refundable,
        requested: PositiveAmount::new(row.requested)
            .expect("payments_amount_check keeps an amount between 1 and the cap"),
        authorized: row.authorized.map(|amount| {
            PositiveAmount::new(amount)
                .expect("an authorization's amount is its step's, between 1 and the cap")
        }),
        authorization_requested: row.authorization_requested,
        refunds_in_flight: row.refunds_in_flight,
        capture_or_void_in_flight: row.capture_or_void_in_flight.map(|label| {
            RequestKind::parse(&label).expect("payment_requests_kind_check allows only known kinds")
        }),
    }))
}

/// Whether a request may be recorded: a stop first, since a stopped payment takes no
/// request whatever its status (ADR-0042); then the table of moves and the amount's
/// limit, as for a step; then the requests already recorded (ADR-0028, ADR-0039).
fn decide(ask: Ask, state: &RequestState) -> Result<(), Refusal> {
    if state.stopped {
        return Err(Conflict::Stopped.into());
    }
    match ask {
        Ask::Authorize(amount) => {
            lifecycle::authorize(state.status, amount, state.requested)?;
            if state.authorization_requested {
                return Err(Conflict::AuthorizationRequested.into());
            }
        }
        Ask::Capture(amount) => {
            // Before an authorization, the status refuses a capture whatever its limit.
            let limit = state.authorized.unwrap_or(state.requested);
            lifecycle::capture(state.status, amount, limit)?;
            no_capture_or_void_in_flight(state)?;
        }
        Ask::Void => {
            lifecycle::next(state.status, Event::Void)?;
            no_capture_or_void_in_flight(state)?;
        }
        Ask::Refund(amount) => {
            // Each refund in flight fitted what stayed refundable when it was recorded,
            // and only a fact that decides one lowers what is refundable, so this is
            // never below zero. Books that say otherwise refund nothing more.
            let limit = state
                .refundable
                .checked_sub(state.refunds_in_flight)
                .map_or(0, |limit| limit.max(0));
            lifecycle::refund(state.status, amount, limit)?;
        }
    }
    Ok(())
}

fn no_capture_or_void_in_flight(state: &RequestState) -> Result<(), Conflict> {
    match state.capture_or_void_in_flight {
        Some(kind) => Err(Conflict::Pending(kind)),
        None => Ok(()),
    }
}

async fn insert(tx: &mut Transaction<'_, Postgres>, command: &RecordRequest) -> sqlx::Result<()> {
    let asked = command.asked();
    sqlx::query!(
        "INSERT INTO payment_requests (id, payment_id, kind, amount) VALUES ($1, $2, $3, $4)",
        command.request.get(),
        asked.payment,
        asked.kind.as_str(),
        asked.amount
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Where a request stands, as its merchant follows it (ADR-0033, ADR-0045).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Followed {
    pub kind: RequestKind,
    /// The amount asked: `None` for a void, which asks for none.
    pub amount: Option<PositiveAmount>,
    pub standing: Standing,
}

/// A request's state, derived from rows and never stored (ADR-0033).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Only the request is recorded. One that the PSP refused as malformed stays here
    /// while an operator looks (ADR-0037).
    Recorded,
    /// The PSP holds it (ADR-0028).
    Acknowledged,
    /// The PSP's history applied it (ADR-0031).
    Applied(Applied),
    /// The PSP's history refused it.
    Refused,
}

impl Standing {
    /// The label that the API shows (ADR-0033).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Acknowledged => "acknowledged",
            Self::Applied(_) => "applied",
            Self::Refused => "refused",
        }
    }
}

/// What an applied request did: the amount of the step that applied it, and the business
/// time in that step's record (ADR-0045).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// An authorization, whose amount may be less than asked (ADR-0016).
    Authorized {
        amount: PositiveAmount,
        expires_at: BusinessTime,
    },
    Captured {
        amount: PositiveAmount,
        captured_at: BusinessTime,
    },
    Refunded {
        amount: PositiveAmount,
        refunded_at: BusinessTime,
    },
    Voided,
}

/// Why a request could not be followed.
#[derive(Debug, thiserror::Error)]
#[error("request {request} of payment {payment} could not be read")]
pub struct FollowError {
    request: Uuid,
    payment: PaymentId,
    #[source]
    source: sqlx::Error,
}

/// Why a decided request has its fact: `payment_request_decisions_fact_fkey`, under the
/// same policy as the decision.
const DECIDED_BY_A_FACT: &str = "payment_request_decisions_fact_fkey names an existing fact";

/// Why an applying fact reaches its step's record: the catch-up links each fact that it
/// applies to its step in the transaction that decides the request (ADR-0031), and LG004
/// keeps the record of a step that carries data.
const LINKED_TO_ITS_STEP: &str = "an applied fact is linked to its step, whose record LG004 keeps";

/// Reads where request `request` of payment `payment` stands, or `None` when the payment
/// has no such request, or when another merchant owns it and the scope is a merchant's
/// (ADR-0025, ADR-0033).
///
/// # Errors
///
/// [`FollowError`] when the request cannot be read.
pub async fn find_request<'a>(
    scope: impl Into<Scope<'a>>,
    payment: PaymentId,
    request: &RequestId,
) -> Result<Option<Followed>, FollowError> {
    let failure = |source| FollowError {
        request: request.get(),
        payment,
        source,
    };
    let mut tx = scope.into().begin().await.map_err(failure)?;
    // One statement, so the request, its answer, its decision and the record of the step
    // that applied it come from one snapshot. A request has at most one answer and one
    // decision, a decision names one fact, a fact links to at most one step, and a step
    // has at most one record.
    let row = sqlx::query!(
        r#"SELECT r.kind, r.amount,
                  EXISTS (SELECT 1 FROM payment_request_answers a
                           WHERE a.request_id = r.id AND a.answer = $3) AS "acknowledged!",
                  d.fact_kind AS "fact_kind?",
                  f.amount AS "applied?",
                  au.expires_at AS "expires_at?",
                  c.captured_at AS "captured_at?",
                  rf.refunded_at AS "refunded_at?"
             FROM payment_requests r
             LEFT JOIN payment_request_decisions d ON d.request_id = r.id
             LEFT JOIN payment_facts f
                    ON f.payment_id = d.payment_id AND f.position = d.position
             LEFT JOIN payment_transition_facts l
                    ON l.payment_id = d.payment_id AND l.position = d.position
             LEFT JOIN payment_authorizations au ON au.transition_id = l.transition_id
             LEFT JOIN payment_captures c ON c.transition_id = l.transition_id
             LEFT JOIN payment_refunds rf ON rf.transition_id = l.transition_id
            WHERE r.id = $1 AND r.payment_id = $2"#,
        request.get(),
        payment.get(),
        Answer::Acknowledged.as_str()
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(failure)?;
    Ok(row.map(|row| {
        let kind = RequestKind::parse(&row.kind)
            .expect("payment_requests_kind_check allows only known kinds");
        let amount = match kind {
            RequestKind::Void => None,
            RequestKind::Authorize | RequestKind::Capture | RequestKind::Refund => Some(
                PositiveAmount::new(row.amount)
                    .expect("payment_requests_amount_check keeps it between 1 and the cap"),
            ),
        };
        let fact = row.fact_kind.as_deref().map(|label| {
            FactKind::parse(label).expect("payment_facts_kind_check allows only known kinds")
        });
        let applied = || {
            PositiveAmount::new(row.applied.expect(DECIDED_BY_A_FACT))
                .expect("payment_facts_amount_check keeps an applying fact's amount positive")
        };
        let standing = match fact {
            None if row.acknowledged => Standing::Acknowledged,
            None => Standing::Recorded,
            Some(FactKind::Authorize) => Standing::Applied(Applied::Authorized {
                amount: applied(),
                expires_at: stored_time(row.expires_at.expect(LINKED_TO_ITS_STEP)),
            }),
            Some(FactKind::Capture) => Standing::Applied(Applied::Captured {
                amount: applied(),
                captured_at: stored_time(row.captured_at.expect(LINKED_TO_ITS_STEP)),
            }),
            Some(FactKind::Refund) => Standing::Applied(Applied::Refunded {
                amount: applied(),
                refunded_at: stored_time(row.refunded_at.expect(LINKED_TO_ITS_STEP)),
            }),
            Some(FactKind::Void) => Standing::Applied(Applied::Voided),
            Some(
                FactKind::Decline
                | FactKind::CaptureRefused
                | FactKind::VoidRefused
                | FactKind::RefundRefused,
            ) => Standing::Refused,
            Some(FactKind::Expire | FactKind::Settle) => {
                unreachable!(
                    "payment_request_decisions_kinds_check decides a request by its own kinds"
                )
            }
        };
        Followed {
            kind,
            amount,
            standing,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(minor: i64) -> PositiveAmount {
        PositiveAmount::new(minor).expect("a positive amount")
    }

    #[test]
    fn each_standing_is_shown_with_its_label() {
        let standings = [
            Standing::Recorded,
            Standing::Acknowledged,
            Standing::Applied(Applied::Voided),
            Standing::Refused,
        ];
        assert_eq!(
            standings.map(Standing::as_str),
            ["recorded", "acknowledged", "applied", "refused"]
        );
    }

    /// A payment of 100, authorized for 80 once it is past `created`, with the 80
    /// captured and refundable at `captured` and `settled`, and nothing in flight.
    fn at(status: PaymentStatus) -> RequestState {
        use PaymentStatus as S;
        let authorized = (status != S::Created).then(|| amount(80));
        let refundable = if matches!(status, S::Captured | S::Settled) {
            80
        } else {
            0
        };
        RequestState {
            stopped: false,
            status,
            refundable,
            requested: amount(100),
            authorized,
            authorization_requested: authorized.is_some(),
            refunds_in_flight: 0,
            capture_or_void_in_flight: None,
        }
    }

    fn illegal(status: PaymentStatus, event: Event) -> Result<(), Refusal> {
        Err(IllegalTransition { status, event }.into())
    }

    fn above(amount: i64, limit: i64) -> Result<(), Refusal> {
        Err(Refused::AboveLimit { amount, limit }.into())
    }

    #[test]
    fn the_table_of_moves_and_the_limits_decide_as_for_a_step() {
        use PaymentStatus as S;
        for (state, ask, expected) in [
            (at(S::Created), Ask::Authorize(amount(100)), Ok(())),
            (at(S::Created), Ask::Authorize(amount(101)), above(101, 100)),
            (
                at(S::Authorized),
                Ask::Authorize(amount(1)),
                illegal(S::Authorized, Event::Authorize),
            ),
            (at(S::Authorized), Ask::Capture(amount(80)), Ok(())),
            (at(S::Authorized), Ask::Capture(amount(81)), above(81, 80)),
            (
                at(S::Created),
                Ask::Capture(amount(1)),
                illegal(S::Created, Event::Capture),
            ),
            (at(S::Authorized), Ask::Void, Ok(())),
            (at(S::Created), Ask::Void, illegal(S::Created, Event::Void)),
            (
                at(S::Captured),
                Ask::Void,
                illegal(S::Captured, Event::Void),
            ),
            (at(S::Captured), Ask::Refund(amount(79)), Ok(())),
            (at(S::Settled), Ask::Refund(amount(80)), Ok(())),
            (at(S::Captured), Ask::Refund(amount(81)), above(81, 80)),
            // With nothing refundable, any refund is tried as the final one.
            (
                at(S::Authorized),
                Ask::Refund(amount(1)),
                illegal(S::Authorized, Event::FinalRefund),
            ),
            (
                at(S::Refunded),
                Ask::Refund(amount(1)),
                illegal(S::Refunded, Event::FinalRefund),
            ),
        ] {
            assert_eq!(decide(ask, &state), expected, "{ask:?} at {}", state.status);
        }
    }

    #[test]
    fn a_stopped_payment_takes_no_request_whatever_its_status() {
        // Each ask would be recorded, or refused for another reason, if the payment were
        // not stopped: the stop comes first (ADR-0042).
        for &status in PaymentStatus::ALL {
            let state = RequestState {
                stopped: true,
                ..at(status)
            };
            for ask in [
                Ask::Authorize(amount(100)),
                Ask::Capture(amount(80)),
                Ask::Void,
                Ask::Refund(amount(1)),
            ] {
                assert_eq!(
                    decide(ask, &state),
                    Err(Conflict::Stopped.into()),
                    "{ask:?} at {status}"
                );
            }
        }
    }

    #[test]
    fn a_payment_gets_one_authorization_request() {
        let state = RequestState {
            authorization_requested: true,
            ..at(PaymentStatus::Created)
        };
        assert_eq!(
            decide(Ask::Authorize(amount(100)), &state),
            Err(Conflict::AuthorizationRequested.into())
        );
    }

    #[test]
    fn the_status_decides_before_a_conflict() {
        // An authorization decided already: the move is illegal, as it would be for a
        // step, rather than a conflict with the request that led to it.
        assert_eq!(
            decide(Ask::Authorize(amount(1)), &at(PaymentStatus::Authorized)),
            illegal(PaymentStatus::Authorized, Event::Authorize)
        );
        let captured_while_voiding = RequestState {
            capture_or_void_in_flight: Some(RequestKind::Void),
            ..at(PaymentStatus::Captured)
        };
        assert_eq!(
            decide(Ask::Void, &captured_while_voiding),
            illegal(PaymentStatus::Captured, Event::Void)
        );
    }

    #[test]
    fn a_capture_or_a_void_waits_for_the_one_in_flight() {
        for in_flight in [RequestKind::Capture, RequestKind::Void] {
            let state = RequestState {
                capture_or_void_in_flight: Some(in_flight),
                ..at(PaymentStatus::Authorized)
            };
            for ask in [Ask::Capture(amount(1)), Ask::Void] {
                assert_eq!(
                    decide(ask, &state),
                    Err(Conflict::Pending(in_flight).into()),
                    "{ask:?} while a {in_flight} is in flight"
                );
            }
        }
    }

    #[test]
    fn a_refund_counts_the_refunds_in_flight() {
        let state = RequestState {
            refunds_in_flight: 50,
            ..at(PaymentStatus::Captured)
        };
        assert_eq!(decide(Ask::Refund(amount(30)), &state), Ok(()));
        assert_eq!(decide(Ask::Refund(amount(31)), &state), above(31, 30));
        let all_in_flight = RequestState {
            refunds_in_flight: 80,
            ..at(PaymentStatus::Settled)
        };
        assert_eq!(decide(Ask::Refund(amount(1)), &all_in_flight), above(1, 0));
        // Books that say more is in flight than refundable refund nothing more.
        let broken = RequestState {
            refunds_in_flight: 90,
            ..at(PaymentStatus::Captured)
        };
        assert_eq!(decide(Ask::Refund(amount(1)), &broken), above(1, 0));
    }

    #[test]
    fn a_kind_reads_back_from_its_label() {
        for kind in [
            RequestKind::Authorize,
            RequestKind::Capture,
            RequestKind::Refund,
            RequestKind::Void,
        ] {
            assert_eq!(RequestKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(RequestKind::parse("settle"), None);
        assert_eq!(RequestKind::parse("Authorize"), None);
    }

    #[test]
    fn a_void_asks_for_no_amount() {
        assert_eq!(Ask::Void.amount(), 0);
        assert_eq!(Ask::Refund(amount(7)).amount(), 7);
        assert_eq!(Ask::Capture(amount(7)).kind(), RequestKind::Capture);
    }
}
