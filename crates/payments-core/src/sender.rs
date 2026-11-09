// Author: Nicola Antonello

//! The sender (ADR-0028, ADR-0032, ADR-0042): the only code that sends a request to the
//! PSP. It sends each request until the PSP holds it. A request that the process which
//! recorded it hands over is sent at once; any other attempt is a retry, which spends a
//! token of the budget that the sender shares with the sweep, and looks the request up
//! first: it sends again only if the PSP does not hold the request.
//!
//! Its steps take the time and a source of random numbers, so a test decides both, and
//! its timing lives in memory. What it knows of each request is in the requests,
//! answers, decisions and stops, so a restart loses only the timing.
//!
//! The lints are the same as in `money`: this module sends amounts.
#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use axum::http::StatusCode;
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::accounts::Psp;
use crate::catch_up::{self, CatchUpError, CaughtUp, SweepError, Swept};
use crate::db::{CommitError, code_and_constraint, commit};
use crate::ids::{PaymentId, RequestId};
use crate::money::Currency;
use crate::psp::{Outgoing, PspClient, ReadError, SendError};
use crate::requests::RequestKind;
use crate::retries::{self, RetryBudget};

/// How long a request may wait for its acknowledgement, from its first attempt in this
/// process, before a step's report flags it for an operator (ADR-0032, ADR-0042).
pub const FLAG_AFTER: Duration = Duration::from_secs(3600);

/// The sender of one PSP: what it knows of the requests that it sends, the budget of
/// retries that it shares with the sweep, and where the sweep's next pass begins.
pub struct Sender {
    client: PspClient,
    budget: RetryBudget,
    /// Requests handed over and not yet seen by a step: each one's first send needs no
    /// lookup (ADR-0042).
    handed_over: HashSet<Uuid>,
    /// The requests that wait for the PSP, by ID.
    pending: BTreeMap<Uuid, Pending>,
    /// The last payment that the sweep caught up.
    place: Option<PaymentId>,
}

/// What the sender knows of one request.
#[derive(Debug, Clone, Copy)]
struct Pending {
    payment: PaymentId,
    kind: RequestKind,
    amount: i64,
    currency: Currency,
    /// Which retry the next attempt is: 0 for the first send of a request handed over,
    /// which needs neither a token nor a lookup.
    retry: u32,
    /// When the next attempt is due.
    due: DateTime<Utc>,
    /// When this process first tried it, for the flag.
    first_tried: Option<DateTime<Utc>>,
}

/// The PSP's answer to a send, as `payment_request_answers` stores it (ADR-0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The PSP holds the request.
    Acknowledged,
    /// The PSP will never take it: a bug of ours, for an operator.
    Malformed,
}

impl Answer {
    /// The label that the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Malformed => "malformed",
        }
    }
}

/// What one attempt at a request came to (ADR-0032, ADR-0042).
#[derive(Debug)]
pub enum Attempt {
    /// The PSP answered the send with a `2xx`, which is recorded: the sender stops.
    Sent,
    /// The lookup found the request at the PSP, which is recorded as acknowledged
    /// without sending it again (ADR-0039).
    Found,
    /// The PSP refused the request as malformed, which is recorded: the sender stops,
    /// and an operator looks (ADR-0037).
    Malformed { status: StatusCode },
    /// The PSP asked to be called later: the next attempt waits at least `retry_after`.
    TryLater { retry_after: Option<u32> },
    /// The send's outcome is unknown: the next attempt looks the request up first.
    Unknown(SendError),
    /// The lookup could not read the history: the next attempt tries again.
    LookupFailed(CatchUpError),
    /// The lookup's catch-up stopped the payment, so the request waits for an operator
    /// with it.
    Stopped,
    /// The PSP's answer could not be recorded: the next attempt's lookup finds what the
    /// PSP holds.
    NotRecorded(AnswerError),
    /// No token was left for this retry, so it waits for a later step.
    NoToken,
}

/// What one step of the sender did (ADR-0042).
#[derive(Debug, Default)]
pub struct Report {
    /// Each attempt, by request, in the order of the attempts.
    pub attempts: Vec<(Uuid, Attempt)>,
    /// What the lookups' catch-ups did, by payment.
    pub caught_up: Vec<(PaymentId, Result<CaughtUp, CatchUpError>)>,
    /// The requests of stopped payments, which wait for an operator.
    pub held: Vec<Uuid>,
    /// The requests still unacknowledged [`FLAG_AFTER`] after their first attempt in
    /// this process, for an operator.
    pub overdue: Vec<Uuid>,
}

/// Why a step could not list the requests to send. It sent nothing, and the next step
/// lists them again.
#[derive(Debug, thiserror::Error)]
#[error("the requests that wait for the PSP could not be listed")]
pub struct SearchError {
    #[source]
    source: sqlx::Error,
}

/// Why the PSP's answer could not be recorded.
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    /// The insert failed, and wrote nothing.
    #[error("the answer could not be written")]
    Database(#[source] sqlx::Error),
    /// The `COMMIT` failed, so the answer may or may not be recorded (ADR-0014).
    #[error("the answer may or may not be recorded")]
    NotConfirmed(#[source] CommitError),
}

impl Sender {
    /// The sender of the PSP that `client` reads, with the budget of ADR-0042, full at
    /// `now`.
    #[must_use]
    pub fn new(client: PspClient, now: DateTime<Utc>) -> Self {
        Self::with_budget(client, RetryBudget::standard(now))
    }

    /// The sender of the PSP that `client` reads, with this budget.
    #[must_use]
    pub fn with_budget(client: PspClient, budget: RetryBudget) -> Self {
        Self {
            client,
            budget,
            handed_over: HashSet::new(),
            pending: BTreeMap::new(),
            place: None,
        }
    }

    /// Takes a request that this process has just recorded and committed: the next step
    /// sends it at once, without a lookup, since no earlier send can have happened
    /// (ADR-0042). One that the sender knows already keeps its state.
    pub fn hand_over(&mut self, request: &RequestId) {
        if !self.pending.contains_key(&request.get()) {
            self.handed_over.insert(request.get());
        }
    }

    /// One step (ADR-0042): searches for the requests that wait for the PSP, then makes
    /// each attempt that is due at `now`, one at a time, the oldest first. `random`
    /// draws the jitter of each delay.
    ///
    /// # Errors
    ///
    /// [`SearchError`] when the requests cannot be listed.
    pub async fn send_due(
        &mut self,
        pool: &PgPool,
        now: DateTime<Utc>,
        random: &mut impl FnMut() -> u64,
    ) -> Result<Report, SearchError> {
        let waiting = search(pool, self.client.psp()).await?;
        let mut report = Report::default();
        self.take_in(waiting, now, random, &mut report);
        let mut due: Vec<(DateTime<Utc>, Uuid)> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(&request, pending)| (pending.due, request))
            .collect();
        due.sort_unstable();
        for (_, request) in due {
            let attempt = self.attempt(pool, request, now, random, &mut report).await;
            report.attempts.push((request, attempt));
        }
        report.overdue = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending
                    .first_tried
                    .is_some_and(|first| waited(first, now) >= FLAG_AFTER)
            })
            .map(|(&request, _)| request)
            .collect();
        Ok(report)
    }

    /// One pass of the sweep, under the budget that it shares with the sender, starting
    /// after the last payment that the pass before it caught up (ADR-0042).
    ///
    /// # Errors
    ///
    /// [`SweepError`] when the payments that are due cannot be listed.
    pub async fn sweep(&mut self, pool: &PgPool, now: DateTime<Utc>) -> Result<Swept, SweepError> {
        let swept = catch_up::sweep(pool, &self.client, now, &mut self.budget, self.place).await?;
        if let Some(&(last, _)) = swept.last() {
            self.place = Some(last);
        }
        Ok(swept)
    }

    /// Brings what the sender knows up to date with the search. A request that has
    /// gained an answer, a decision or a stop leaves it, and a stopped payment's request
    /// is held. A new one is due at once if it was handed over, and otherwise after a
    /// jittered delay, as the first retry, since it may have been sent before a crash
    /// (ADR-0042).
    fn take_in(
        &mut self,
        waiting: Vec<Waiting>,
        now: DateTime<Utc>,
        random: &mut impl FnMut() -> u64,
        report: &mut Report,
    ) {
        let handed_over = std::mem::take(&mut self.handed_over);
        let mut pending = BTreeMap::new();
        for found in waiting {
            if found.stopped {
                report.held.push(found.request);
                continue;
            }
            let known = self.pending.remove(&found.request);
            let fresh = handed_over.contains(&found.request);
            let (retry, due) = match known {
                Some(known) => (known.retry, known.due),
                None if fresh => (0, now),
                None => (1, after(now, retries::delay(1, None, random()))),
            };
            pending.insert(
                found.request,
                Pending {
                    payment: found.payment,
                    kind: found.kind,
                    amount: found.amount,
                    currency: found.currency,
                    retry,
                    due,
                    first_tried: known.and_then(|known| known.first_tried),
                },
            );
        }
        self.pending = pending;
    }

    /// One attempt at a request that is due. A retry spends a token, and looks the
    /// request up first through a catch-up: found, it is recorded as acknowledged; a
    /// payment that the catch-up stopped holds it. Otherwise the request is sent, and the
    /// PSP's answer recorded or the next attempt scheduled (ADR-0028, ADR-0032).
    async fn attempt(
        &mut self,
        pool: &PgPool,
        request: Uuid,
        now: DateTime<Utc>,
        random: &mut impl FnMut() -> u64,
        report: &mut Report,
    ) -> Attempt {
        let Some(&pending) = self.pending.get(&request) else {
            unreachable!("an attempt is made only at a request that the sender holds");
        };
        let id =
            RequestId::try_from(request).expect("payment_requests_id_check allows only UUIDv7 IDs");
        if pending.retry > 0 && !self.budget.take(now) {
            return Attempt::NoToken;
        }
        if let Some(pending) = self.pending.get_mut(&request) {
            pending.first_tried.get_or_insert(now);
        }
        if pending.retry > 0 {
            let read = match catch_up::read(pool, &self.client, pending.payment).await {
                Ok(read) => read,
                Err(failed) => {
                    self.later(request, now, asked_later(&failed), random);
                    return Attempt::LookupFailed(failed);
                }
            };
            let held = read.history().holds(&id);
            let caught = catch_up::apply(pool, read).await;
            let stopped = matches!(
                caught,
                Ok(CaughtUp::Stopped { .. } | CaughtUp::AlreadyStopped)
            );
            report.caught_up.push((pending.payment, caught));
            if held {
                return self
                    .record(
                        pool,
                        request,
                        Answer::Acknowledged,
                        Attempt::Found,
                        now,
                        random,
                    )
                    .await;
            }
            if stopped {
                self.pending.remove(&request);
                return Attempt::Stopped;
            }
        }
        let outgoing = Outgoing {
            request: id,
            payment: pending.payment,
            kind: pending.kind,
            amount: pending.amount,
            currency: pending.currency,
        };
        match self.client.send(&outgoing).await {
            Ok(()) => {
                self.record(
                    pool,
                    request,
                    Answer::Acknowledged,
                    Attempt::Sent,
                    now,
                    random,
                )
                .await
            }
            Err(SendError::Malformed { status }) => {
                let attempt = Attempt::Malformed { status };
                self.record(pool, request, Answer::Malformed, attempt, now, random)
                    .await
            }
            Err(SendError::TryLater { retry_after }) => {
                self.later(request, now, retry_after, random);
                Attempt::TryLater { retry_after }
            }
            Err(unknown) => {
                self.later(request, now, None, random);
                Attempt::Unknown(unknown)
            }
        }
    }

    /// Records the PSP's answer, and the request leaves the sender. An answer that
    /// cannot be recorded leaves it with the sender, whose next lookup finds what the PSP
    /// holds.
    async fn record(
        &mut self,
        pool: &PgPool,
        request: Uuid,
        answer: Answer,
        attempt: Attempt,
        now: DateTime<Utc>,
        random: &mut impl FnMut() -> u64,
    ) -> Attempt {
        let Some(&pending) = self.pending.get(&request) else {
            unreachable!("an answer is recorded only for a request that the sender holds");
        };
        match record_answer(pool, request, pending.payment, answer).await {
            Ok(()) => {
                self.pending.remove(&request);
                attempt
            }
            Err(failed) => {
                self.later(request, now, None, random);
                Attempt::NotRecorded(failed)
            }
        }
    }

    /// Schedules the next attempt at a request: the next retry, after its jittered delay
    /// and never before the PSP's `Retry-After` (ADR-0032).
    fn later(
        &mut self,
        request: Uuid,
        now: DateTime<Utc>,
        retry_after: Option<u32>,
        random: &mut impl FnMut() -> u64,
    ) {
        if let Some(pending) = self.pending.get_mut(&request) {
            pending.retry = pending.retry.saturating_add(1);
            pending.due = after(now, retries::delay(pending.retry, retry_after, random()));
        }
    }
}

/// A request with neither an answer nor a decision, as the search finds it.
struct Waiting {
    request: Uuid,
    payment: PaymentId,
    kind: RequestKind,
    amount: i64,
    currency: Currency,
    stopped: bool,
}

/// The requests of this PSP that have neither an answer nor a decision: those that wait
/// for the PSP, and those that a stop holds (ADR-0042). The IDs order them for a stable
/// report, never for correctness (ADR-0005).
async fn search(pool: &PgPool, psp: Psp) -> Result<Vec<Waiting>, SearchError> {
    let rows = sqlx::query!(
        r#"SELECT r.id, r.payment_id, r.kind, r.amount, p.currency,
                  EXISTS (SELECT 1 FROM payment_stops s WHERE s.payment_id = p.id)
                      AS "stopped!"
             FROM payment_requests r
             JOIN payments p ON p.id = r.payment_id
            WHERE p.psp = $1
              AND NOT EXISTS (SELECT 1 FROM payment_request_answers a
                               WHERE a.request_id = r.id)
              AND NOT EXISTS (SELECT 1 FROM payment_request_decisions d
                               WHERE d.request_id = r.id)
            ORDER BY r.id"#,
        psp.as_str()
    )
    .fetch_all(pool)
    .await
    .map_err(|source| SearchError { source })?;
    Ok(rows
        .into_iter()
        .map(|row| Waiting {
            request: row.id,
            payment: PaymentId::try_from(row.payment_id)
                .expect("payments_id_check allows only UUIDv7 IDs"),
            kind: RequestKind::parse(&row.kind)
                .expect("payment_requests_kind_check allows only known kinds"),
            amount: row.amount,
            currency: Currency::parse(&row.currency)
                .expect("payments_currency_fkey names a currency of three capital letters"),
            stopped: row.stopped,
        })
        .collect())
}

/// Records the PSP's answer to a request, once (ADR-0037): one insert as the system
/// role, committed through the one function of ADR-0014. An answer that a copy of the
/// sender recorded first is not an error.
async fn record_answer(
    pool: &PgPool,
    request: Uuid,
    payment: PaymentId,
    answer: Answer,
) -> Result<(), AnswerError> {
    let mut tx = pool.begin().await.map_err(AnswerError::Database)?;
    let inserted = sqlx::query!(
        "INSERT INTO payment_request_answers (request_id, payment_id, answer) \
         VALUES ($1, $2, $3)",
        request,
        payment.get(),
        answer.as_str()
    )
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => commit(tx).await.map_err(AnswerError::NotConfirmed),
        Err(failed) => {
            // The failed insert aborted the transaction: drop it (ADR-0014).
            drop(tx);
            let (code, constraint) = code_and_constraint(&failed);
            if recorded_already(code.as_deref(), constraint) {
                Ok(())
            } else {
                Err(AnswerError::Database(failed))
            }
        }
    }
}

/// How long the PSP asked to wait, when a lookup failed because it asked.
fn asked_later(failed: &CatchUpError) -> Option<u32> {
    match failed {
        CatchUpError::Psp {
            source: ReadError::TryLater { retry_after },
            ..
        } => *retry_after,
        _ => None,
    }
}

/// `delay` after `now`.
fn after(now: DateTime<Utc>, delay: Duration) -> DateTime<Utc> {
    let micros = i64::try_from(delay.as_micros())
        .expect("a delay of at most u32 seconds is an i64 of microseconds");
    now.timestamp_micros()
        .checked_add(micros)
        .and_then(DateTime::from_timestamp_micros)
        .expect("a time at most u32 seconds after now is in range")
}

/// How long since `first`, or nothing for a time after `now`.
fn waited(first: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    now.timestamp_micros()
        .checked_sub(first.timestamp_micros())
        .and_then(|micros| u64::try_from(micros).ok())
        .map_or(Duration::ZERO, Duration::from_micros)
}

/// A random number from the operating system, for the jitter in production (ADR-0042).
#[must_use]
pub fn os_random() -> u64 {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("the jitter needs the operating system's random bytes");
    u64::from_le_bytes(bytes)
}

/// Whether an insert's error says that the request's answer is recorded already: the
/// pair of ADR-0013, read without the message.
fn recorded_already(code: Option<&str>, constraint: Option<&str>) -> bool {
    matches!(
        (code, constraint),
        (Some("23505"), Some("payment_request_answers_pkey"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_answers_key_says_that_an_answer_is_recorded_already() {
        for (code, constraint, already) in [
            (Some("23505"), Some("payment_request_answers_pkey"), true),
            // The request is unknown: a bug, not a copy.
            (
                Some("23503"),
                Some("payment_request_answers_request_fkey"),
                false,
            ),
            (Some("23505"), Some("payment_requests_pkey"), false),
            (Some("23505"), None, false),
            (
                Some("23514"),
                Some("payment_request_answers_answer_check"),
                false,
            ),
            (None, None, false),
        ] {
            assert_eq!(
                recorded_already(code, constraint),
                already,
                "{code:?} {constraint:?}"
            );
        }
    }

    #[test]
    fn each_answer_has_the_label_of_its_check() {
        assert_eq!(Answer::Acknowledged.as_str(), "acknowledged");
        assert_eq!(Answer::Malformed.as_str(), "malformed");
    }
}
