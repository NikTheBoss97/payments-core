// Author: Nicola Antonello

//! Catching a payment up with the PSP's history (ADR-0031, ADR-0039, ADR-0041): the one
//! way that a fact of the PSP's reaches a payment.
//!
//! A catch-up comes in two halves. [`read`] counts the facts that the service holds,
//! then reads the PSP's history, outside any transaction. [`apply`] takes the payment's
//! lock, checks that the facts held are the history's first facts, ID for ID, applies
//! each new fact through the command of its step, and commits once, through the one
//! function of ADR-0014. A fact that the service cannot place, or that contradicts what
//! it holds, stops the payment for an operator at its position, after the facts before
//! it. [`catch_up`] runs the two halves in a row, and a test runs them in any order.
//!
//! [`sweep`] catches up the payments that wait for the PSP, one at a time, for the facts
//! whose notifications never came, under the budget of retries that it shares with the
//! sender (ADR-0042).
//!
//! The lints are the same as in `money`: this module handles amounts.
#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::collections::HashMap;
use std::convert::Infallible;

use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::accounts::{AccountKey, Psp};
use crate::ids::{PaymentId, TransactionId, TransitionId};
use crate::lifecycle::{Event, IllegalTransition, PaymentStatus, StepKind};
use crate::payments::{
    Authorize, Books, Capture, Failed, Failure, Outcome, PlainRefusal, PostingRefusal, Refund,
    StepCommand, authorize_in, begin_locked, capture_in, commit_unit, failure_of, plain_in,
    read_books, refund_in,
};
use crate::psp::{Fact, FactDetail, FactKind, History, PspClient, ReadError};
use crate::requests::RequestKind;
use crate::retries::RetryBudget;

/// What the first half of a catch-up read: how many facts the service held before it
/// asked, and the PSP's history. Only [`read`] makes one.
#[derive(Debug)]
pub struct Read {
    counted: i32,
    psp: Psp,
    history: History,
}

/// Where a catch-up left a payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaughtUp {
    /// The facts held are the history's first facts, and the new ones after them were
    /// applied: `applied` of them, none when the history held nothing new, or was read
    /// before facts that the service holds now (ADR-0041).
    Current { applied: u32 },
    /// The catch-up stopped the payment for an operator, after applying `applied` new
    /// facts.
    Stopped { applied: u32, stop: Stop },
    /// An earlier catch-up stopped the payment, so nothing more is applied (ADR-0031).
    AlreadyStopped,
}

/// Where and why a catch-up stopped a payment (ADR-0039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stop {
    /// The position of the history at which it stopped.
    pub position: i32,
    pub reason: StopReason,
}

/// Why a catch-up stopped a payment (ADR-0039, ADR-0041).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The facts held are not the PSP's first facts.
    HistoryChanged,
    /// A fact that no step can take from the payment's status, or that the service
    /// cannot read.
    NoPlace,
    /// A decision on a request that the payment never recorded, on one decided already,
    /// or one that disagrees with it in kind or amount.
    Contradiction,
}

impl StopReason {
    /// The label that the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HistoryChanged => "history_changed",
            Self::NoPlace => "no_place",
            Self::Contradiction => "contradiction",
        }
    }
}

/// Why a catch-up failed. Nothing of it committed, unless it is
/// [`CatchUpError::NotConfirmed`], and the next catch-up applies only what is missing
/// (ADR-0039).
#[derive(Debug, thiserror::Error)]
pub enum CatchUpError {
    /// No payment has this ID.
    #[error("payment {payment} does not exist")]
    NotFound { payment: PaymentId },
    /// The PSP's history could not be read. The sweep reads it again (ADR-0032).
    #[error("the PSP's history of payment {payment} could not be read")]
    Psp {
        payment: PaymentId,
        #[source]
        source: ReadError,
    },
    /// An account of a posting does not exist. Creating the payment opens them, and
    /// accounts are never deleted, so a writer that skipped that broke a rule: a bug.
    #[error("account {key} does not exist")]
    MissingAccount { key: AccountKey },
    /// The database caught what the domain types should have prevented, or a writer
    /// that skipped the payment's lock got there first (ADR-0018): a bug, never retried.
    #[error("payment {payment}: the database rejected the catch-up as invalid")]
    Invariant {
        payment: PaymentId,
        #[source]
        source: sqlx::Error,
    },
    /// The catch-up may or may not have committed. The next one reads the history and
    /// the facts held again, and applies only what is missing (ADR-0039).
    #[error("payment {payment}: the catch-up may or may not have committed")]
    NotConfirmed {
        payment: PaymentId,
        #[source]
        source: sqlx::Error,
    },
}

/// What one pass of the sweep did: each payment that was due, in the order of their IDs,
/// with what catching it up did.
pub type Swept = Vec<(PaymentId, Result<CaughtUp, CatchUpError>)>;

/// Why a pass of the sweep could not list the payments that are due. It caught up none,
/// and the next pass lists them again.
#[derive(Debug, thiserror::Error)]
#[error("the payments that wait for the PSP could not be listed")]
pub struct SweepError {
    #[source]
    source: sqlx::Error,
}

/// One pass of the sweep (ADR-0031, ADR-0039, ADR-0041): catches up, one at a time,
/// every payment of this PSP that waits for it: one with a request not yet decided, an
/// authorization that lapses at or before `now`, or a capture not yet settled, which
/// may have been refunded in full first (ADR-0016). A stopped payment waits for an
/// operator instead. A failure is reported with its payment, and the pass goes on. The
/// caller reads `now` from its clock, and sets the cadence.
///
/// Each catch-up is a retry, so it spends a token of `budget` first, and the pass ends
/// when the bucket is empty. The pass goes through the due payments in ID order,
/// starting after `after`, the last payment that the pass before it caught up, and
/// wrapping round, so that every waiting payment gets its turn (ADR-0042).
///
/// # Errors
///
/// [`SweepError`] when the payments that are due cannot be listed.
pub async fn sweep(
    pool: &PgPool,
    psp: &PspClient,
    now: DateTime<Utc>,
    budget: &mut RetryBudget,
    after: Option<PaymentId>,
) -> Result<Swept, SweepError> {
    // In ID order from the place, for fairness and never for correctness (ADR-0005): the
    // IDs after `after` first, then the rest. With no place, the first key is NULL for
    // every payment, which leaves the order by ID.
    let due = sqlx::query_scalar!(
        r#"SELECT p.id
             FROM payments p
            CROSS JOIN LATERAL (
                  SELECT t.to_status
                    FROM payment_transitions t
                   WHERE t.payment_id = p.id
                   ORDER BY t.seq DESC
                   LIMIT 1
                  ) newest
            WHERE p.psp = $1
              AND NOT EXISTS (SELECT 1 FROM payment_stops s WHERE s.payment_id = p.id)
              AND (EXISTS (SELECT 1 FROM payment_requests r
                            WHERE r.payment_id = p.id
                              AND NOT EXISTS (SELECT 1 FROM payment_request_decisions d
                                               WHERE d.request_id = r.id))
                   OR (newest.to_status = $2
                       AND EXISTS (SELECT 1 FROM payment_authorizations a
                                    WHERE a.payment_id = p.id AND a.expires_at <= $3))
                   OR (newest.to_status IN ($4, $5)
                       AND NOT EXISTS (SELECT 1 FROM payment_facts f
                                        WHERE f.payment_id = p.id AND f.kind = $6)))
            ORDER BY p.id <= $7, p.id"#,
        psp.psp().as_str(),
        PaymentStatus::Authorized.as_str(),
        now,
        PaymentStatus::Captured.as_str(),
        PaymentStatus::Refunded.as_str(),
        FactKind::Settle.as_str(),
        after.map(PaymentId::get)
    )
    .fetch_all(pool)
    .await
    .map_err(|source| SweepError { source })?;
    let mut swept = Vec::with_capacity(due.len());
    for id in due {
        if !budget.take(now) {
            break;
        }
        let payment = PaymentId::try_from(id).expect("payments_id_check allows only UUIDv7 IDs");
        swept.push((payment, catch_up(pool, psp, payment).await));
    }
    Ok(swept)
}

/// Catches a payment up with its PSP's history: [`read`], then [`apply`].
///
/// # Errors
///
/// See [`CatchUpError`].
pub async fn catch_up(
    pool: &PgPool,
    psp: &PspClient,
    payment: PaymentId,
) -> Result<CaughtUp, CatchUpError> {
    let read = read(pool, psp, payment).await?;
    apply(pool, read).await
}

/// The first half of a catch-up: counts the facts held, then reads the history, both
/// outside any transaction (ADR-0031). The count comes first: a fact is written only
/// after a read that served it, so the PSP had served at least that many facts before
/// this read began (ADR-0041).
///
/// # Errors
///
/// See [`CatchUpError`].
pub async fn read(
    pool: &PgPool,
    psp: &PspClient,
    payment: PaymentId,
) -> Result<Read, CatchUpError> {
    let counted = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(position), 0) AS "counted!" FROM payment_facts
            WHERE payment_id = $1"#,
        payment.get()
    )
    .fetch_one(pool)
    .await
    .map_err(|source| database(payment, source))?;
    let history = psp
        .history(payment)
        .await
        .map_err(|source| CatchUpError::Psp { payment, source })?;
    Ok(Read {
        counted,
        psp: psp.psp(),
        history,
    })
}

/// The second half of a catch-up: under the payment's lock, checks the history against
/// the facts held, applies the new facts in the PSP's order, and commits once
/// (ADR-0039, ADR-0041).
///
/// # Errors
///
/// See [`CatchUpError`].
pub async fn apply(pool: &PgPool, read: Read) -> Result<CaughtUp, CatchUpError> {
    let Read {
        counted,
        psp,
        history,
    } = read;
    let payment = history.payment();
    let theirs = history.facts();
    let counted = usize::try_from(counted).expect("a count of facts is never negative");
    // Outside the transaction, as a command reads them: what the postings of new captures
    // and refunds need. Every new fact comes after the count.
    let books = if theirs.iter().skip(counted).any(posts) {
        let books = read_books(pool.into(), payment)
            .await
            .map_err(|failed| books_failure(payment, failed))?;
        Some(books)
    } else {
        None
    };
    let mut tx = begin_locked::<Infallible>(pool.into(), payment)
        .await
        .map_err(|failed| broken(payment, failed))?;
    let Some(held) = read_held(&mut tx, payment).await? else {
        return Err(CatchUpError::NotFound { payment });
    };
    if held.stopped {
        return Ok(CaughtUp::AlreadyStopped);
    }
    if let Some(position) = changed_at(&held.facts, theirs, counted) {
        let stop = Stop {
            position,
            reason: StopReason::HistoryChanged,
        };
        return stop_at(tx, payment, stop, 0).await;
    }
    let Some(new) = theirs.get(held.facts.len()..).filter(|new| !new.is_empty()) else {
        // Nothing after the facts held: the history held nothing new, or was read before
        // facts that a crossing catch-up committed (ADR-0041).
        return Ok(CaughtUp::Current { applied: 0 });
    };
    let mut applying = Applying {
        tx,
        payment,
        psp,
        books: books.as_ref(),
        settled: held.facts.iter().any(|(_, kind)| *kind == FactKind::Settle),
        requests: held.requests,
        applied: 0,
    };
    for fact in new {
        if let Some(reason) = applying.place(fact).await? {
            let stop = Stop {
                position: fact.position(),
                reason,
            };
            return stop_at(applying.tx, payment, stop, applying.applied).await;
        }
    }
    commit(applying.tx, payment).await?;
    Ok(CaughtUp::Current {
        applied: applying.applied,
    })
}

/// Whether a fact posts to the ledger, and so needs the payment's books.
fn posts(fact: &Fact) -> bool {
    matches!(
        fact.detail(),
        Ok(FactDetail::Captured { .. } | FactDetail::Refunded { .. })
    )
}

/// What the service holds of a payment, read under its lock, in statements after the
/// lock's (ADR-0018).
struct Held {
    stopped: bool,
    /// The PSP's ID and the kind of each fact held, by position.
    facts: Vec<(String, FactKind)>,
    requests: HashMap<Uuid, HeldRequest>,
}

/// A request of the payment, as the checks of a decision need it (ADR-0041).
struct HeldRequest {
    kind: RequestKind,
    amount: i64,
    /// Whether a fact decided it, before this catch-up or in it.
    decided: bool,
}

/// What the service holds of the payment, or `None` when no payment has this ID.
async fn read_held(
    tx: &mut Transaction<'static, Postgres>,
    payment: PaymentId,
) -> Result<Option<Held>, CatchUpError> {
    let failed = |source| database(payment, source);
    let Some(stopped) = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM payment_stops s WHERE s.payment_id = p.id)
                  AS "stopped!"
             FROM payments p
            WHERE p.id = $1"#,
        payment.get()
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(failed)?
    else {
        return Ok(None);
    };
    let facts = sqlx::query!(
        "SELECT psp_fact_id, kind FROM payment_facts WHERE payment_id = $1 ORDER BY position",
        payment.get()
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(failed)?
    .into_iter()
    .map(|row| {
        let kind = FactKind::parse(&row.kind).expect("payment_facts_kind_check allows only kinds");
        (row.psp_fact_id, kind)
    })
    .collect();
    let requests = sqlx::query!(
        r#"SELECT r.id, r.kind, r.amount,
                  EXISTS (SELECT 1 FROM payment_request_decisions d WHERE d.request_id = r.id)
                  AS "decided!"
             FROM payment_requests r
            WHERE r.payment_id = $1"#,
        payment.get()
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(failed)?
    .into_iter()
    .map(|row| {
        let request = HeldRequest {
            kind: RequestKind::parse(&row.kind)
                .expect("payment_requests_kind_check allows only kinds"),
            amount: row.amount,
            decided: row.decided,
        };
        (row.id, request)
    })
    .collect();
    Ok(Some(Held {
        stopped,
        facts,
        requests,
    }))
}

/// The first position at which the history and the facts held disagree (ADR-0041): one
/// that both have, with another ID, or else the first that the history lacks of the
/// facts counted before it was read.
fn changed_at(held: &[(String, FactKind)], theirs: &[Fact], counted: usize) -> Option<i32> {
    held.iter()
        .zip(theirs)
        .find(|((id, _), fact)| id.as_str() != fact.id())
        .map(|(_, fact)| fact.position())
        .or_else(|| {
            (theirs.len() < counted).then(|| {
                i32::try_from(theirs.len())
                    .ok()
                    .and_then(|len| len.checked_add(1))
                    .expect("a history holds far fewer facts than an i32 counts")
            })
        })
}

/// What a new fact made.
enum Placed {
    /// A step, with this ID.
    Step(Uuid),
    /// Nothing but the fact: a refusal, or a settlement after a full refund.
    Recorded,
    /// Nothing: no step can take it from the payment's status, or within its limits.
    Nowhere,
}

/// Why the books are there when a new fact posts: they were read before the lock, if
/// any fact after the count posts, and every new fact comes after the count.
const NEEDED: &str = "the books of a new capture or refund, read before the lock";

/// A catch-up applying new facts under the payment's lock.
struct Applying<'a> {
    tx: Transaction<'static, Postgres>,
    payment: PaymentId,
    psp: Psp,
    books: Option<&'a Books>,
    requests: HashMap<Uuid, HeldRequest>,
    /// Whether the PSP settled the payment, in a fact held or applied here.
    settled: bool,
    applied: u32,
}

impl Applying<'_> {
    /// Applies one new fact, with its step if it makes one, and records it; or names the
    /// reason that stops the payment at it, in the order of ADR-0041: a fact that cannot
    /// be placed, a decision that contradicts the requests, then the table of moves.
    async fn place(&mut self, fact: &Fact) -> Result<Option<StopReason>, CatchUpError> {
        let Ok(detail) = fact.detail() else {
            return Ok(Some(StopReason::NoPlace));
        };
        let request = match detail.request() {
            None => None,
            Some(id) => match self.requests.get(&id) {
                Some(asked) if !asked.decided && answers(asked, detail) => Some((id, asked.kind)),
                _ => return Ok(Some(StopReason::Contradiction)),
            },
        };
        let step = match self.step(detail).await? {
            Placed::Step(step) => Some(step),
            Placed::Recorded => None,
            Placed::Nowhere => return Ok(Some(StopReason::NoPlace)),
        };
        self.insert_fact(fact, detail).await?;
        if let Some(step) = step {
            self.insert_link(step, fact.position(), detail).await?;
        }
        if let Some((id, kind)) = request {
            self.insert_decision(id, kind, fact.position(), detail.kind())
                .await?;
            if let Some(asked) = self.requests.get_mut(&id) {
                asked.decided = true;
            }
        }
        self.settled |= detail == FactDetail::Settled;
        self.applied = self
            .applied
            .checked_add(1)
            .expect("a history holds far fewer facts than a u32 counts");
        Ok(None)
    }

    /// Makes a fact's step through the library's command for it (ADR-0031), with IDs of
    /// its own, since a fact's identity is its position (ADR-0039).
    async fn step(&mut self, detail: FactDetail) -> Result<Placed, CatchUpError> {
        let payment = self.payment;
        // A copy of the reference, so that it borrows nothing of `self`.
        let books = self.books;
        let step = TransitionId::now_v7();
        let id = step.get();
        let made = match detail {
            FactDetail::Authorized {
                amount, expires_at, ..
            } => {
                let command = Authorize {
                    step,
                    payment,
                    amount,
                    expires_at,
                };
                refusal(payment, authorize_in(&mut self.tx, &command).await)?.is_none()
            }
            FactDetail::Captured {
                amount,
                captured_at,
                ..
            } => {
                let command = Capture {
                    step,
                    posting: TransactionId::now_v7(),
                    payment,
                    amount,
                    captured_at,
                };
                refusal(
                    payment,
                    capture_in(&mut self.tx, books.expect(NEEDED), &command).await,
                )?
                .is_none()
            }
            FactDetail::Refunded {
                amount,
                refunded_at,
                ..
            } => {
                let command = Refund {
                    step,
                    posting: TransactionId::now_v7(),
                    payment,
                    amount,
                    refunded_at,
                };
                refusal(
                    payment,
                    refund_in(&mut self.tx, books.expect(NEEDED), &command).await,
                )?
                .is_none()
            }
            FactDetail::Declined { .. } => self
                .plain(step, Event::Decline, StepKind::Decline)
                .await?
                .is_none(),
            FactDetail::Voided { .. } => self
                .plain(step, Event::Void, StepKind::Void)
                .await?
                .is_none(),
            FactDetail::Expired => self
                .plain(step, Event::Expire, StepKind::Expire)
                .await?
                .is_none(),
            FactDetail::Settled => match self.plain(step, Event::Settle, StepKind::Settle).await? {
                None => true,
                // A settlement after a full refund is recorded without a step, unless the
                // PSP settled the payment before (ADR-0016).
                Some(PlainRefusal::Illegal(IllegalTransition {
                    status: PaymentStatus::Refunded,
                    ..
                })) if !self.settled => return Ok(Placed::Recorded),
                Some(_) => false,
            },
            // A refusal closes its request without a step (ADR-0037).
            FactDetail::CaptureRefused { .. }
            | FactDetail::VoidRefused { .. }
            | FactDetail::RefundRefused { .. } => return Ok(Placed::Recorded),
        };
        Ok(if made {
            Placed::Step(id)
        } else {
            Placed::Nowhere
        })
    }

    /// The step of a fact that carries nothing, or the refusal of the table of moves.
    async fn plain(
        &mut self,
        step: TransitionId,
        event: Event,
        kind: StepKind,
    ) -> Result<Option<PlainRefusal>, CatchUpError> {
        let command = StepCommand {
            step,
            payment: self.payment,
        };
        refusal(
            self.payment,
            plain_in(&mut self.tx, &command, event, kind).await,
        )
    }

    async fn insert_fact(&mut self, fact: &Fact, detail: FactDetail) -> Result<(), CatchUpError> {
        let payment = self.payment;
        let position = fact.position();
        // The first fact points to itself, so that every column stays NOT NULL (ADR-0037).
        let previous = position
            .checked_sub(1)
            .expect("positions start at 1")
            .max(1);
        sqlx::query!(
            "INSERT INTO payment_facts
                    (payment_id, position, prev_position, psp, psp_fact_id, kind, amount)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            payment.get(),
            position,
            previous,
            self.psp.as_str(),
            fact.id(),
            detail.kind().as_str(),
            detail.amount()
        )
        .execute(&mut *self.tx)
        .await
        .map_err(|source| database(payment, source))?;
        Ok(())
    }

    /// Ties a step to the fact that made it, which agree on kind and amount through both
    /// foreign keys (ADR-0037).
    async fn insert_link(
        &mut self,
        step: Uuid,
        position: i32,
        detail: FactDetail,
    ) -> Result<(), CatchUpError> {
        let payment = self.payment;
        sqlx::query!(
            "INSERT INTO payment_transition_facts (transition_id, payment_id, kind, amount, position)
             VALUES ($1, $2, $3, $4, $5)",
            step,
            payment.get(),
            detail.kind().as_str(),
            detail.amount(),
            position
        )
        .execute(&mut *self.tx)
        .await
        .map_err(|source| database(payment, source))?;
        Ok(())
    }

    /// Records that the fact at `position` decided this request (ADR-0031).
    async fn insert_decision(
        &mut self,
        request: Uuid,
        kind: RequestKind,
        position: i32,
        fact_kind: FactKind,
    ) -> Result<(), CatchUpError> {
        let payment = self.payment;
        sqlx::query!(
            "INSERT INTO payment_request_decisions
                    (request_id, payment_id, request_kind, position, fact_kind)
             VALUES ($1, $2, $3, $4, $5)",
            request,
            payment.get(),
            kind.as_str(),
            position,
            fact_kind.as_str()
        )
        .execute(&mut *self.tx)
        .await
        .map_err(|source| database(payment, source))?;
        Ok(())
    }
}

/// Whether a decision answers its request: their kinds pair as
/// `payment_request_decisions_kinds_check` allows, an authorization is for at most the
/// amount asked (ADR-0016), and a capture or a refund for exactly that amount.
fn answers(request: &HeldRequest, detail: FactDetail) -> bool {
    match (request.kind, detail) {
        (RequestKind::Authorize, FactDetail::Authorized { amount, .. }) => {
            amount.get() <= request.amount
        }
        (RequestKind::Capture, FactDetail::Captured { amount, .. })
        | (RequestKind::Refund, FactDetail::Refunded { amount, .. }) => {
            amount.get() == request.amount
        }
        (RequestKind::Authorize, FactDetail::Declined { .. })
        | (RequestKind::Capture, FactDetail::CaptureRefused { .. })
        | (RequestKind::Void, FactDetail::Voided { .. } | FactDetail::VoidRefused { .. })
        | (RequestKind::Refund, FactDetail::RefundRefused { .. }) => true,
        _ => false,
    }
}

/// Stops the payment for an operator at this position, and commits with the facts
/// applied before it, which agreed (ADR-0041).
async fn stop_at(
    mut tx: Transaction<'static, Postgres>,
    payment: PaymentId,
    stop: Stop,
    applied: u32,
) -> Result<CaughtUp, CatchUpError> {
    sqlx::query!(
        "INSERT INTO payment_stops (payment_id, position, reason) VALUES ($1, $2, $3)",
        payment.get(),
        stop.position,
        stop.reason.as_str()
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| database(payment, source))?;
    commit(tx, payment).await?;
    Ok(CaughtUp::Stopped { applied, stop })
}

/// Commits through the one function of ADR-0014.
async fn commit(
    tx: Transaction<'static, Postgres>,
    payment: PaymentId,
) -> Result<(), CatchUpError> {
    commit_unit::<Infallible>(tx)
        .await
        .map_err(|failed| broken(payment, failed))
}

/// The refusal of a step's command, or none when it applied the step.
fn refusal<R>(
    payment: PaymentId,
    result: Result<Outcome, Failed<R>>,
) -> Result<Option<R>, CatchUpError> {
    match result {
        Ok(Outcome::Applied(_)) => Ok(None),
        Ok(Outcome::AlreadyApplied(_)) => {
            unreachable!("a catch-up's step IDs are new, so none is stored (ADR-0039)")
        }
        Err(Failed::Refused(refused)) => Ok(Some(refused)),
        Err(failed) => Err(broken(payment, failed)),
    }
}

/// A failure that is not a refusal. A catch-up's step IDs are new, and it runs without
/// an idempotency key, so it never meets a stored step or a taken key (ADR-0039).
fn broken<R>(payment: PaymentId, failed: Failed<R>) -> CatchUpError {
    match failed {
        Failed::NotFound => CatchUpError::NotFound { payment },
        Failed::Duplicate(source) | Failed::Invariant(source) => {
            CatchUpError::Invariant { payment, source }
        }
        Failed::NotConfirmed(source) => CatchUpError::NotConfirmed { payment, source },
        Failed::Refused(_) => unreachable!("a refusal is read before, or never made"),
        Failed::Mismatch => {
            unreachable!("a catch-up's step IDs are new, so none is stored (ADR-0039)")
        }
        Failed::KeyTaken => unreachable!("a catch-up runs without an idempotency key"),
    }
}

/// Why the books could not be read.
fn books_failure(payment: PaymentId, failed: Failed<PostingRefusal>) -> CatchUpError {
    match failed {
        Failed::Refused(PostingRefusal::MissingAccount(key)) => {
            CatchUpError::MissingAccount { key }
        }
        Failed::Refused(PostingRefusal::Refused(_)) => {
            unreachable!("reading the books moves nothing")
        }
        failed => broken(payment, failed),
    }
}

/// A database error, as the one classifier reads it (ADR-0015). Under the payment's lock
/// a duplicate key is a writer that skipped the lock, or a PSP that gave two facts one
/// ID: a bug either way.
fn database(payment: PaymentId, source: sqlx::Error) -> CatchUpError {
    match failure_of(&source) {
        Failure::Duplicate | Failure::Invariant => CatchUpError::Invariant { payment, source },
        Failure::NotConfirmed => CatchUpError::NotConfirmed { payment, source },
    }
}
