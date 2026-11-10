// Author: Nicola Antonello

//! The sender's task (ADR-0043, ADR-0044): the work that the server does in the
//! background. One task owns the sender, takes a step every second and a pass of the
//! sweep every minute, and logs what changed.
//!
//! A step's report lists what has not changed too, such as a request that a stop holds,
//! in every step while it lasts. The task logs each attempt and each catch-up that did
//! something, and a request once when it becomes held or overdue, so that the log holds
//! what happened, once. Which lines to write is a pure function of a step's or a pass's
//! result and of what the task has already logged.

use std::collections::HashSet;
use std::convert::Infallible;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tokio::time::{self, MissedTickBehavior};
use tracing::Level;
use uuid::Uuid;

use crate::api::problem::chain;
use crate::catch_up::{CatchUpError, CaughtUp, SweepError, Swept};
use crate::ids::PaymentId;
use crate::sender::{Attempt, Report, SearchError, Sender, os_random};

/// How often the task works (ADR-0043).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// Between two steps of the sender.
    pub step: Duration,
    /// Between two passes of the sweep.
    pub pass: Duration,
}

impl Cadence {
    /// A step every second, and a pass of the sweep every minute (ADR-0043).
    pub const STANDARD: Self = Self {
        step: Duration::from_secs(1),
        pass: Duration::from_secs(60),
    };
}

/// Runs the sender for as long as the process lives (ADR-0043, ADR-0044): a step every
/// `cadence.step` and a pass of the sweep every `cadence.pass`, the first of each at
/// once, and logs what changed. It never returns, so a task that ends has panicked,
/// which the server reads as a crash.
pub async fn run(pool: PgPool, mut sender: Sender, cadence: Cadence) -> Infallible {
    // A late step delays the next one by a whole period, rather than running the missed
    // ones in a burst (ADR-0044).
    let mut steps = time::interval(cadence.step);
    steps.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut passes = time::interval(cadence.pass);
    passes.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watch = Watch::default();
    loop {
        // The select! only picks the next event. The work runs after it, outside the
        // select!, so no step or pass is ever dropped half done (ADR-0043).
        let event = tokio::select! {
            _ = steps.tick() => Event::Step,
            _ = passes.tick() => Event::Pass,
        };
        let now = Utc::now();
        let lines = match event {
            Event::Step => watch.step(&sender.send_due(&pool, now, &mut os_random).await),
            Event::Pass => pass(&sender.sweep(&pool, now).await),
        };
        for line in lines {
            if line.level == Level::WARN {
                tracing::warn!("{}", line.text);
            } else {
                tracing::info!("{}", line.text);
            }
        }
    }
}

/// The task's next piece of work.
#[derive(Debug, Clone, Copy)]
enum Event {
    Step,
    Pass,
}

/// One line of the task's log, and its level: a warning is something that failed, or
/// that waits for an operator.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    level: Level,
    text: String,
}

impl Line {
    fn info(text: String) -> Self {
        Self {
            level: Level::INFO,
            text,
        }
    }

    fn warn(text: String) -> Self {
        Self {
            level: Level::WARN,
            text,
        }
    }
}

/// The requests that the task has logged as held by a stop, or as overdue, so that each
/// is logged once while it stays so (ADR-0044).
#[derive(Debug, Default)]
struct Watch {
    held: HashSet<Uuid>,
    overdue: HashSet<Uuid>,
}

impl Watch {
    /// The lines for one step of the sender: its attempts and catch-ups that did
    /// something, then the requests newly held or newly overdue. A step that could not
    /// list its requests leaves the sets as they were.
    fn step(&mut self, step: &Result<Report, SearchError>) -> Vec<Line> {
        let report = match step {
            Ok(report) => report,
            Err(failed) => {
                return vec![Line::warn(format!(
                    "the sender's step could not list its requests: {}",
                    chain(failed)
                ))];
            }
        };
        let mut lines: Vec<Line> = report
            .attempts
            .iter()
            .filter_map(|(request, attempt)| attempt_line(*request, attempt))
            .collect();
        lines.extend(
            report
                .caught_up
                .iter()
                .filter_map(|(payment, result)| catch_up_line(*payment, result)),
        );
        lines.extend(
            newly(&mut self.held, &report.held)
                .into_iter()
                .map(|request| {
                    Line::warn(format!(
                        "request {request} waits with its stopped payment for an operator"
                    ))
                }),
        );
        lines.extend(
            newly(&mut self.overdue, &report.overdue)
                .into_iter()
                .map(|request| {
                    Line::warn(format!(
                        "request {request} has no acknowledgement an hour after its first attempt"
                    ))
                }),
        );
        lines
    }
}

/// The requests in `now` that `logged` does not hold yet, in their order. `logged` then
/// holds `now` alone, so a request that leaves and comes back is new again.
fn newly(logged: &mut HashSet<Uuid>, now: &[Uuid]) -> Vec<Uuid> {
    let new = now
        .iter()
        .copied()
        .filter(|request| !logged.contains(request))
        .collect();
    *logged = now.iter().copied().collect();
    new
}

/// The lines for one pass of the sweep: its catch-ups that did something.
fn pass(pass: &Result<Swept, SweepError>) -> Vec<Line> {
    match pass {
        Ok(swept) => swept
            .iter()
            .filter_map(|(payment, result)| catch_up_line(*payment, result))
            .collect(),
        Err(failed) => vec![Line::warn(format!(
            "the sweep's pass could not list its payments: {}",
            chain(failed)
        ))],
    }
}

/// The line for an attempt at a request, unless it found no token, which is no event.
fn attempt_line(request: Uuid, attempt: &Attempt) -> Option<Line> {
    let line = match attempt {
        Attempt::Sent => Line::info(format!("request {request}: sent, and the PSP holds it")),
        Attempt::Found => Line::info(format!(
            "request {request}: the PSP holds it already, so it was not sent again"
        )),
        Attempt::TryLater {
            retry_after: Some(seconds),
        } => Line::info(format!(
            "request {request}: the PSP asked to be called again in {seconds} s"
        )),
        Attempt::TryLater { retry_after: None } => Line::info(format!(
            "request {request}: the PSP asked to be called again later"
        )),
        Attempt::Malformed { status } => Line::warn(format!(
            "request {request}: the PSP refused it as malformed ({status}), so it waits for an operator"
        )),
        Attempt::Unknown(failed) => Line::warn(format!(
            "request {request}: the send's outcome is unknown, so a lookup comes first: {}",
            chain(failed)
        )),
        Attempt::LookupFailed(failed) => Line::warn(format!(
            "request {request}: the lookup failed: {}",
            chain(failed)
        )),
        Attempt::Stopped => Line::warn(format!(
            "request {request}: its payment stopped, so it waits for an operator"
        )),
        Attempt::NotRecorded(failed) => Line::warn(format!(
            "request {request}: the PSP's answer could not be recorded: {}",
            chain(failed)
        )),
        Attempt::NoToken => return None,
    };
    Some(line)
}

/// The line for a catch-up, unless it applied nothing.
fn catch_up_line(payment: PaymentId, result: &Result<CaughtUp, CatchUpError>) -> Option<Line> {
    match result {
        Ok(CaughtUp::Current { applied: 0 } | CaughtUp::AlreadyStopped) => None,
        Ok(CaughtUp::Current { applied: 1 }) => Some(Line::info(format!(
            "payment {payment}: 1 new fact of the PSP's applied"
        ))),
        Ok(CaughtUp::Current { applied }) => Some(Line::info(format!(
            "payment {payment}: {applied} new facts of the PSP's applied"
        ))),
        Ok(CaughtUp::Stopped { stop, .. }) => Some(Line::warn(format!(
            "payment {payment} stopped at position {} of the PSP's history: {}",
            stop.position,
            stop.reason.as_str()
        ))),
        Err(failed) => Some(Line::warn(format!(
            "catching payment {payment} up failed: {}",
            chain(failed)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::StatusCode;
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;
    use sqlx::types::chrono::{DateTime, Utc};

    use super::*;
    use crate::accounts::Psp;
    use crate::catch_up::{Stop, StopReason};
    use crate::psp::{PspClient, ReadError, SendError};
    use crate::sender::{AnswerError, Sender};

    const R1: Uuid = Uuid::from_u128(0x0196_a5b4_c3d2_7e8f_9a0b_1c2d_3e4f_0001);
    const R2: Uuid = Uuid::from_u128(0x0196_a5b4_c3d2_7e8f_9a0b_1c2d_3e4f_0002);

    fn payment(n: u128) -> PaymentId {
        PaymentId::try_from(Uuid::from_u128(
            0x0196_a5b4_c3d2_7e8f_9a0b_1c2d_3e4f_1000 + n,
        ))
        .expect("a UUIDv7")
    }

    fn info(text: &str) -> Line {
        Line {
            level: Level::INFO,
            text: text.to_owned(),
        }
    }

    fn warn(text: &str) -> Line {
        Line {
            level: Level::WARN,
            text: text.to_owned(),
        }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    fn report(attempts: Vec<(Uuid, Attempt)>) -> Result<Report, SearchError> {
        Ok(Report {
            attempts,
            ..Report::default()
        })
    }

    fn held(requests: &[Uuid]) -> Result<Report, SearchError> {
        Ok(Report {
            held: requests.to_vec(),
            ..Report::default()
        })
    }

    fn overdue(requests: &[Uuid]) -> Result<Report, SearchError> {
        Ok(Report {
            overdue: requests.to_vec(),
            ..Report::default()
        })
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_767_225_600, 0).expect("a time in range")
    }

    /// A pool that is closed, so that every query on it fails at once, and a sender of a
    /// PSP that nothing serves: the real errors of a step and a pass.
    async fn failing() -> (PgPool, Sender) {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:9/nothing")
            .expect("a URL that parses");
        pool.close().await;
        let client = PspClient::new(
            Psp::Mock,
            "http://127.0.0.1:9",
            "unused",
            Duration::from_secs(1),
        )
        .expect("a local address");
        (pool, Sender::new(client, now()))
    }

    #[test]
    fn the_cadence_is_a_step_a_second_and_a_pass_a_minute() {
        assert_eq!(
            Cadence::STANDARD,
            Cadence {
                step: Duration::from_secs(1),
                pass: Duration::from_secs(60),
            }
        );
    }

    #[test]
    fn a_step_with_nothing_in_it_logs_nothing() {
        assert_eq!(Watch::default().step(&report(Vec::new())), []);
    }

    #[test]
    fn each_attempt_that_did_something_is_logged_once_and_no_token_is_not() {
        let mut watch = Watch::default();
        let lines = watch.step(&report(vec![
            (R1, Attempt::Sent),
            (R2, Attempt::Found),
            (
                R1,
                Attempt::TryLater {
                    retry_after: Some(30),
                },
            ),
            (R2, Attempt::TryLater { retry_after: None }),
            (R1, Attempt::NoToken),
        ]));
        assert_eq!(
            lines,
            [
                info(&format!("request {R1}: sent, and the PSP holds it")),
                info(&format!(
                    "request {R2}: the PSP holds it already, so it was not sent again"
                )),
                info(&format!(
                    "request {R1}: the PSP asked to be called again in 30 s"
                )),
                info(&format!(
                    "request {R2}: the PSP asked to be called again later"
                )),
            ]
        );
    }

    #[test]
    fn what_failed_or_waits_for_an_operator_is_a_warning_with_its_causes() {
        let mut watch = Watch::default();
        let lines = watch.step(&report(vec![
            (
                R1,
                Attempt::Malformed {
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                },
            ),
            (R2, Attempt::Unknown(SendError::TimedOut)),
            (
                R1,
                Attempt::LookupFailed(CatchUpError::Psp {
                    payment: payment(1),
                    source: ReadError::TimedOut,
                }),
            ),
            (R2, Attempt::Stopped),
            (
                R1,
                Attempt::NotRecorded(AnswerError::Database(sqlx::Error::PoolTimedOut)),
            ),
        ]));
        assert!(
            lines.iter().all(|line| line.level == Level::WARN),
            "{lines:?}"
        );
        assert_eq!(
            texts(&lines),
            [
                format!(
                    "request {R1}: the PSP refused it as malformed (422 Unprocessable Entity), so it waits for an operator"
                ),
                format!(
                    "request {R2}: the send's outcome is unknown, so a lookup comes first: the PSP did not answer in time"
                ),
                format!(
                    "request {R1}: the lookup failed: the PSP's history of payment {} could not be read: the PSP did not answer in time",
                    payment(1)
                ),
                format!("request {R2}: its payment stopped, so it waits for an operator"),
                format!(
                    "request {R1}: the PSP's answer could not be recorded: the answer could not be written: pool timed out while waiting for an open connection"
                ),
            ]
        );
    }

    #[test]
    fn a_catch_up_is_logged_when_it_applied_stopped_or_failed() {
        let stop = Stop {
            position: 3,
            reason: StopReason::NoPlace,
        };
        let caught_up = vec![
            (payment(1), Ok(CaughtUp::Current { applied: 0 })),
            (payment(2), Ok(CaughtUp::Current { applied: 1 })),
            (payment(3), Ok(CaughtUp::Current { applied: 2 })),
            (payment(4), Ok(CaughtUp::Stopped { applied: 1, stop })),
            (payment(5), Ok(CaughtUp::AlreadyStopped)),
            (
                payment(6),
                Err(CatchUpError::NotFound {
                    payment: payment(6),
                }),
            ),
        ];
        let expected = [
            info(&format!(
                "payment {}: 1 new fact of the PSP's applied",
                payment(2)
            )),
            info(&format!(
                "payment {}: 2 new facts of the PSP's applied",
                payment(3)
            )),
            warn(&format!(
                "payment {} stopped at position 3 of the PSP's history: no_place",
                payment(4)
            )),
            warn(&format!(
                "catching payment {} up failed: payment {} does not exist",
                payment(6),
                payment(6)
            )),
        ];
        // The lookups' catch-ups in a step, and the sweep's in a pass, alike.
        let step = Ok(Report {
            caught_up: caught_up
                .iter()
                .map(|(payment, result)| (*payment, copy(result)))
                .collect(),
            ..Report::default()
        });
        assert_eq!(Watch::default().step(&step), expected);
        let swept: Swept = caught_up;
        assert_eq!(pass(&Ok(swept)), expected);
    }

    /// The same result, for the two halves of a test.
    fn copy(result: &Result<CaughtUp, CatchUpError>) -> Result<CaughtUp, CatchUpError> {
        match result {
            Ok(caught_up) => Ok(*caught_up),
            Err(CatchUpError::NotFound { payment }) => {
                Err(CatchUpError::NotFound { payment: *payment })
            }
            Err(other) => panic!("not one of this test's results: {other:?}"),
        }
    }

    #[test]
    fn a_held_request_is_logged_once_while_it_stays_held_and_again_if_it_comes_back() {
        let mut watch = Watch::default();
        let line = warn(&format!(
            "request {R1} waits with its stopped payment for an operator"
        ));
        assert_eq!(watch.step(&held(&[R1])), std::slice::from_ref(&line));
        assert_eq!(watch.step(&held(&[R1])), []);
        assert_eq!(
            watch.step(&held(&[R1, R2])),
            [warn(&format!(
                "request {R2} waits with its stopped payment for an operator"
            ))]
        );
        assert_eq!(watch.step(&held(&[R2])), []);
        assert_eq!(watch.step(&held(&[R1, R2])), [line]);
    }

    #[test]
    fn an_overdue_request_is_logged_once_while_it_stays_overdue_and_again_if_it_comes_back() {
        let mut watch = Watch::default();
        let line = warn(&format!(
            "request {R1} has no acknowledgement an hour after its first attempt"
        ));
        assert_eq!(watch.step(&overdue(&[R1])), std::slice::from_ref(&line));
        assert_eq!(watch.step(&overdue(&[R1])), []);
        assert_eq!(watch.step(&overdue(&[])), []);
        assert_eq!(watch.step(&overdue(&[R1])), [line]);
    }

    #[test]
    fn held_and_overdue_are_watched_apart() {
        // One request, held and overdue in the same step: each is news once.
        let both = || {
            Ok(Report {
                held: vec![R1],
                overdue: vec![R1],
                ..Report::default()
            })
        };
        let mut watch = Watch::default();
        assert_eq!(
            watch.step(&both()),
            [
                warn(&format!(
                    "request {R1} waits with its stopped payment for an operator"
                )),
                warn(&format!(
                    "request {R1} has no acknowledgement an hour after its first attempt"
                )),
            ]
        );
        assert_eq!(watch.step(&both()), []);
    }

    #[tokio::test]
    async fn a_step_that_cannot_list_its_requests_is_a_warning_each_time_and_forgets_nothing() {
        let (pool, mut sender) = failing().await;
        let mut watch = Watch::default();
        assert_eq!(watch.step(&held(&[R1])).len(), 1);
        for _ in 0..2 {
            let step = sender.send_due(&pool, now(), &mut || 0).await;
            assert_eq!(
                watch.step(&step),
                [warn(
                    "the sender's step could not list its requests: the requests that wait for the PSP could not be listed: attempted to acquire a connection on a closed pool"
                )]
            );
        }
        // The failed steps did not clear what was logged as held.
        assert_eq!(watch.step(&held(&[R1])), []);
    }

    #[tokio::test]
    async fn a_pass_that_cannot_list_its_payments_is_a_warning() {
        let (pool, mut sender) = failing().await;
        assert_eq!(
            pass(&sender.sweep(&pool, now()).await),
            [warn(
                "the sweep's pass could not list its payments: the payments that wait for the PSP could not be listed: attempted to acquire a connection on a closed pool"
            )]
        );
    }
}
