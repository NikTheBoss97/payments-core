// Author: Nicola Antonello

//! The PSP's records (ADR-0029): what it took, what it decided, and what it still has to
//! tell the server.
//!
//! It has two memories. Its records, which make each payment's history, are permanent.
//! Its idempotency cache forgets, as Stripe's may after a day, and then a copy of a
//! request counts as a new one.
//!
//! The fake keeps each payment's state on its own side, and refuses a decision that a
//! real PSP could not make, such as capturing more than was authorized: that is a mistake
//! in the test. One method appends any fact, for the tests of a history that the service
//! cannot place. Every time is an argument, so nothing here reads a clock.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::deliveries::{Attempt, next_attempt};
use crate::wire::{Detail, Fact, History, Notification, Request, RequestKind};

/// How long the idempotency cache keeps a request, as Stripe may prune its keys after a
/// day (ADR-0028).
pub(crate) const CACHE_LIFETIME: TimeDelta = TimeDelta::days(1);

/// How the fake decides the requests that it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// It holds every request until the test decides it. Every fact then appears at a
    /// moment that the test chose.
    Hold,
    /// It approves each request as it arrives, or refuses one that it could not approve:
    /// the binary's rule for local runs.
    ApproveAtOnce,
}

/// What a test decides about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// For the whole amount asked.
    Approve,
    /// An authorization for less than the amount asked (ADR-0016).
    ApproveFor(i64),
    /// Of an authorization.
    Decline,
    /// Of a capture, a void or a refund.
    Refuse,
}

/// A decision that a real PSP could not make: a mistake in the test.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Impossible(String);

/// What the fake answered to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Taken {
    /// It holds the request: a new one, or a copy that the cache knows.
    Received,
    /// The ID came with other content while the cache held it.
    Conflict,
}

/// A payment's state on the PSP's side, which its decisions are checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stage {
    #[default]
    Created,
    Authorized {
        amount: i64,
    },
    Declined,
    Voided,
    Expired,
    Captured {
        amount: i64,
        refunded: i64,
        settled: bool,
    },
}

/// A payment, as the PSP sees it.
#[derive(Debug, Default)]
struct Payment {
    /// Every request taken for it, in order. A copy taken after the cache forgot its ID
    /// is here twice.
    requests: Vec<Request>,
    /// The requests not decided yet, as indexes into `requests`.
    undecided: Vec<usize>,
    facts: Vec<Fact>,
    stage: Stage,
}

/// A notification that the PSP owes the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Its `webhook-id`, which stays the same on every attempt (ADR-0030).
    pub id: String,
    pub notification: Notification,
    /// How many attempts were made to deliver it.
    pub attempts: u32,
    pub state: MessageState,
}

/// Where a notification stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageState {
    /// Owed, with its next attempt due at this time.
    Due(DateTime<Utc>),
    /// The receiver took it, with a `2xx`.
    Delivered,
    /// Every attempt of the schedule failed. The sweep finds what it announced
    /// (ADR-0031).
    GaveUp,
    /// The test dropped it: it is never delivered unless the test delivers it.
    Dropped,
}

/// Everything that the fake remembers.
#[derive(Debug)]
pub(crate) struct Model {
    policy: Policy,
    /// How long an authorization lasts before it lapses.
    authorization_lifetime: TimeDelta,
    payments: HashMap<Uuid, Payment>,
    /// The idempotency cache: each request as first taken, and when.
    cache: HashMap<Uuid, (Request, DateTime<Utc>)>,
    /// Every request taken as new, in the order of arrival. An ID here twice means that
    /// the PSP acted twice on one request, which I9 forbids.
    taken: Vec<Request>,
    /// The notifications, in the order of the facts that they announce.
    messages: Vec<Message>,
}

impl Model {
    pub(crate) fn new(policy: Policy, authorization_lifetime: TimeDelta) -> Self {
        Self {
            policy,
            authorization_lifetime,
            payments: HashMap::new(),
            cache: HashMap::new(),
            taken: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Takes a request, which the caller has checked is well formed.
    pub(crate) fn take(&mut self, request: Request, now: DateTime<Utc>) -> Taken {
        if let Some((cached, _)) = self.cache.get(&request.id) {
            return if *cached == request {
                Taken::Received
            } else {
                Taken::Conflict
            };
        }
        self.cache.insert(request.id, (request.clone(), now));
        self.taken.push(request.clone());
        let payment = self.payments.entry(request.payment_id).or_default();
        payment.undecided.push(payment.requests.len());
        payment.requests.push(request.clone());
        if self.policy == Policy::ApproveAtOnce {
            let refusal = match request.kind {
                RequestKind::Authorize => Decision::Decline,
                RequestKind::Capture | RequestKind::Refund | RequestKind::Void => Decision::Refuse,
            };
            // A request that it can neither approve nor refuse, such as a second
            // authorization, stays held.
            if self.decide(request.id, Decision::Approve, now).is_err() {
                let _held = self.decide(request.id, refusal, now);
            }
        }
        Taken::Received
    }

    /// How many times this request ID was taken as a new request.
    pub(crate) fn times_taken(&self, request_id: Uuid) -> usize {
        self.taken
            .iter()
            .filter(|request| request.id == request_id)
            .count()
    }

    /// Every request taken as new, in the order of arrival.
    pub(crate) fn requests(&self) -> &[Request] {
        &self.taken
    }

    /// Forgets every key in the idempotency cache, as when the cache prunes them.
    pub(crate) fn forget_keys(&mut self) {
        self.cache.clear();
    }

    /// Forgets the keys taken more than a day before `now`.
    pub(crate) fn prune_keys(&mut self, now: DateTime<Utc>) {
        self.cache
            .retain(|_, (_, taken_at)| now - *taken_at < CACHE_LIFETIME);
    }

    /// Decides the first undecided request with this ID.
    ///
    /// # Errors
    ///
    /// A decision that a real PSP could not make: see [`Impossible`].
    pub(crate) fn decide(
        &mut self,
        request_id: Uuid,
        decision: Decision,
        now: DateTime<Utc>,
    ) -> Result<(), Impossible> {
        let lifetime = self.authorization_lifetime;
        let (payment_id, slot) = self
            .payments
            .iter()
            .find_map(|(id, payment)| {
                payment
                    .undecided
                    .iter()
                    .position(|&index| payment.requests[index].id == request_id)
                    .map(|slot| (*id, slot))
            })
            .ok_or_else(|| Impossible(format!("no undecided request {request_id}")))?;
        let payment = self
            .payments
            .get_mut(&payment_id)
            .expect("the payment was just found");
        let request = payment.requests[payment.undecided[slot]].clone();
        let (stage, detail) = outcome(payment.stage, &request, decision, now, lifetime)?;
        payment.undecided.remove(slot);
        payment.stage = stage;
        self.append(payment_id, detail, now);
        Ok(())
    }

    /// Settles a captured payment, which only the PSP decides.
    ///
    /// # Errors
    ///
    /// A payment that is not captured, or is settled already.
    pub(crate) fn settle(
        &mut self,
        payment_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), Impossible> {
        let payment = self.payment_mut(payment_id)?;
        let Stage::Captured {
            amount,
            refunded,
            settled: false,
        } = payment.stage
        else {
            return Err(Impossible(format!(
                "cannot settle payment {payment_id}: it is {:?}",
                payment.stage
            )));
        };
        payment.stage = Stage::Captured {
            amount,
            refunded,
            settled: true,
        };
        self.append(payment_id, Detail::Settled, now);
        Ok(())
    }

    /// Lets an authorization lapse, which only the PSP decides.
    ///
    /// # Errors
    ///
    /// A payment that is not authorized.
    pub(crate) fn expire(
        &mut self,
        payment_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), Impossible> {
        let payment = self.payment_mut(payment_id)?;
        if !matches!(payment.stage, Stage::Authorized { .. }) {
            return Err(Impossible(format!(
                "cannot expire payment {payment_id}: it is {:?}",
                payment.stage
            )));
        }
        payment.stage = Stage::Expired;
        self.append(payment_id, Detail::Expired, now);
        Ok(())
    }

    /// Appends a fact and the notification that announces it. The fact changes nothing
    /// that the fake checks, so a test can write a history that no PSP would.
    pub(crate) fn append(&mut self, payment_id: Uuid, detail: Detail, now: DateTime<Utc>) {
        let payment = self.payments.entry(payment_id).or_default();
        let position = u32::try_from(payment.facts.len() + 1)
            .expect("a history shorter than four billion facts");
        payment.facts.push(Fact {
            id: format!("fct_{}", Uuid::now_v7().simple()),
            position,
            detail,
        });
        self.notify(payment_id, now);
    }

    /// Queues a notification that this payment changed, and returns its ID.
    pub(crate) fn notify(&mut self, payment_id: Uuid, now: DateTime<Utc>) -> String {
        let id = format!("msg_{}", Uuid::now_v7().simple());
        self.messages.push(Message {
            id: id.clone(),
            notification: Notification::payment_updated(payment_id, now),
            attempts: 0,
            state: MessageState::Due(now),
        });
        id
    }

    /// The notifications, in the order of the facts that they announce.
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The notification with this ID.
    pub(crate) fn message(&self, message_id: &str) -> Option<&Message> {
        self.messages
            .iter()
            .find(|message| message.id == message_id)
    }

    /// The notifications due by `now`, in order.
    pub(crate) fn due(&self, now: DateTime<Utc>) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| matches!(message.state, MessageState::Due(due) if due <= now))
            .map(|message| message.id.clone())
            .collect()
    }

    /// Records an attempt made at `at`. A delivered notification stays delivered, and a
    /// failure moves only one that is still due to its next attempt, or ends it.
    pub(crate) fn record(
        &mut self,
        message_id: &str,
        attempt: Attempt,
        at: DateTime<Utc>,
        jitter: u16,
    ) {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)
        else {
            return;
        };
        message.attempts += 1;
        if attempt.delivered() {
            message.state = MessageState::Delivered;
        } else if let MessageState::Due(_) = message.state {
            let retry_after = match attempt {
                Attempt::Answered { retry_after, .. } => retry_after,
                Attempt::NoAnswer => None,
            };
            message.state = match next_attempt(message.attempts, at, retry_after, jitter) {
                Some(due) => MessageState::Due(due),
                None => MessageState::GaveUp,
            };
        }
    }

    /// Drops a notification that was not delivered.
    pub(crate) fn drop_message(&mut self, message_id: &str) {
        if let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)
            && message.state != MessageState::Delivered
        {
            message.state = MessageState::Dropped;
        }
    }

    /// A payment's history, if the PSP holds anything for it.
    pub(crate) fn history(&self, payment_id: Uuid) -> Option<History> {
        let payment = self.payments.get(&payment_id)?;
        Some(History {
            payment_id,
            facts: payment.facts.clone(),
            requests: payment.requests.clone(),
        })
    }

    fn payment_mut(&mut self, payment_id: Uuid) -> Result<&mut Payment, Impossible> {
        self.payments
            .get_mut(&payment_id)
            .ok_or_else(|| Impossible(format!("no payment {payment_id}")))
    }
}

/// The stage and the fact that a decision makes, or why a PSP could not make it.
fn outcome(
    stage: Stage,
    request: &Request,
    decision: Decision,
    now: DateTime<Utc>,
    lifetime: TimeDelta,
) -> Result<(Stage, Detail), Impossible> {
    let request_id = request.id;
    let refused = || {
        Impossible(format!(
            "cannot {decision:?} {request:?}: the payment is {stage:?}"
        ))
    };
    match (request.kind, decision) {
        (RequestKind::Authorize, Decision::Approve | Decision::ApproveFor(_)) => {
            let amount = match decision {
                Decision::ApproveFor(less) if (1..=request.amount).contains(&less) => less,
                Decision::ApproveFor(_) => return Err(refused()),
                _ => request.amount,
            };
            if stage != Stage::Created {
                return Err(refused());
            }
            let expires_at = now + lifetime;
            Ok((
                Stage::Authorized { amount },
                Detail::Authorized {
                    request_id,
                    amount,
                    expires_at,
                },
            ))
        }
        (RequestKind::Authorize, Decision::Decline) if stage == Stage::Created => {
            Ok((Stage::Declined, Detail::Declined { request_id }))
        }
        (RequestKind::Capture, Decision::Approve) => match stage {
            Stage::Authorized { amount } if request.amount <= amount => Ok((
                Stage::Captured {
                    amount: request.amount,
                    refunded: 0,
                    settled: false,
                },
                Detail::Captured {
                    request_id,
                    amount: request.amount,
                    captured_at: now,
                },
            )),
            _ => Err(refused()),
        },
        (RequestKind::Void, Decision::Approve) => match stage {
            Stage::Authorized { .. } => Ok((Stage::Voided, Detail::Voided { request_id })),
            _ => Err(refused()),
        },
        (RequestKind::Refund, Decision::Approve) => match stage {
            Stage::Captured {
                amount,
                refunded,
                settled,
            } if request.amount <= amount - refunded => Ok((
                Stage::Captured {
                    amount,
                    refunded: refunded + request.amount,
                    settled,
                },
                Detail::Refunded {
                    request_id,
                    amount: request.amount,
                    refunded_at: now,
                },
            )),
            _ => Err(refused()),
        },
        (RequestKind::Capture, Decision::Refuse) => {
            Ok((stage, Detail::CaptureRefused { request_id }))
        }
        (RequestKind::Void, Decision::Refuse) => Ok((stage, Detail::VoidRefused { request_id })),
        (RequestKind::Refund, Decision::Refuse) => {
            Ok((stage, Detail::RefundRefused { request_id }))
        }
        _ => Err(refused()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYMENT: Uuid = Uuid::from_u128(1);

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp_micros(1_767_225_600_000_000).expect("a time in range")
    }

    fn request(kind: RequestKind, amount: i64) -> Request {
        Request {
            id: Uuid::now_v7(),
            payment_id: PAYMENT,
            kind,
            amount,
            currency: "EUR".to_owned(),
        }
    }

    fn holding() -> Model {
        Model::new(Policy::Hold, TimeDelta::days(7))
    }

    /// Takes a request and decides it.
    fn decided(model: &mut Model, kind: RequestKind, amount: i64, decision: Decision) -> Request {
        let request = request(kind, amount);
        assert_eq!(model.take(request.clone(), now()), Taken::Received);
        model
            .decide(request.id, decision, now())
            .expect("a decision that the PSP can make");
        request
    }

    fn details(model: &Model) -> Vec<Detail> {
        model
            .history(PAYMENT)
            .expect("a payment that the PSP holds")
            .facts
            .into_iter()
            .map(|fact| fact.detail)
            .collect()
    }

    #[test]
    fn a_copy_is_the_same_request_until_the_cache_forgets_it() {
        let mut model = holding();
        let first = request(RequestKind::Authorize, 100);
        assert_eq!(model.take(first.clone(), now()), Taken::Received);
        assert_eq!(model.take(first.clone(), now()), Taken::Received);
        assert_eq!(model.times_taken(first.id), 1);
        let other = Request {
            amount: 99,
            ..first.clone()
        };
        assert_eq!(model.take(other, now()), Taken::Conflict);

        // Forgotten, a copy is a new request: the PSP would act on it twice.
        model.forget_keys();
        assert_eq!(model.take(first.clone(), now()), Taken::Received);
        assert_eq!(model.times_taken(first.id), 2);
        assert_eq!(model.requests(), [first.clone(), first.clone()]);
        let history = model.history(PAYMENT).expect("held");
        assert_eq!(history.requests, [first.clone(), first]);
    }

    #[test]
    fn the_cache_forgets_a_key_after_a_day() {
        let mut model = holding();
        let old = request(RequestKind::Authorize, 100);
        model.take(old.clone(), now());
        let later = now() + CACHE_LIFETIME - TimeDelta::microseconds(1);
        model.prune_keys(later);
        model.take(old.clone(), later);
        assert_eq!(model.times_taken(old.id), 1);
        model.prune_keys(now() + CACHE_LIFETIME);
        model.take(old.clone(), later);
        assert_eq!(model.times_taken(old.id), 2);
    }

    #[test]
    fn a_held_request_makes_no_fact_until_it_is_decided() {
        let mut model = holding();
        let authorize = request(RequestKind::Authorize, 100);
        model.take(authorize.clone(), now());
        assert_eq!(details(&model), []);
        assert!(model.messages().is_empty());
        model
            .decide(authorize.id, Decision::Approve, now())
            .expect("possible");
        assert_eq!(details(&model).len(), 1);
        assert_eq!(model.messages().len(), 1);
    }

    #[test]
    fn each_decision_writes_its_fact_at_the_next_position() {
        let mut model = holding();
        let authorize = decided(
            &mut model,
            RequestKind::Authorize,
            100,
            Decision::ApproveFor(80),
        );
        let refused = decided(&mut model, RequestKind::Capture, 90, Decision::Refuse);
        let capture = decided(&mut model, RequestKind::Capture, 80, Decision::Approve);
        let refund = decided(&mut model, RequestKind::Refund, 30, Decision::Approve);
        let too_late = decided(&mut model, RequestKind::Refund, 60, Decision::Refuse);
        model.settle(PAYMENT, now()).expect("captured");
        let expires_at = now() + TimeDelta::days(7);
        assert_eq!(
            details(&model),
            [
                Detail::Authorized {
                    request_id: authorize.id,
                    amount: 80,
                    expires_at,
                },
                Detail::CaptureRefused {
                    request_id: refused.id,
                },
                Detail::Captured {
                    request_id: capture.id,
                    amount: 80,
                    captured_at: now(),
                },
                Detail::Refunded {
                    request_id: refund.id,
                    amount: 30,
                    refunded_at: now(),
                },
                Detail::RefundRefused {
                    request_id: too_late.id,
                },
                Detail::Settled,
            ]
        );
        let history = model.history(PAYMENT).expect("held");
        let positions: Vec<u32> = history.facts.iter().map(|fact| fact.position).collect();
        assert_eq!(positions, [1, 2, 3, 4, 5, 6]);
        assert_eq!(model.messages().len(), 6);
    }

    #[test]
    fn a_settlement_and_a_full_refund_happen_in_either_order() {
        // The pair that ADR-0031 reads the history for: both are legal from a capture.
        for settle_first in [true, false] {
            let mut model = holding();
            decided(&mut model, RequestKind::Authorize, 100, Decision::Approve);
            decided(&mut model, RequestKind::Capture, 100, Decision::Approve);
            if settle_first {
                model.settle(PAYMENT, now()).expect("captured");
            }
            decided(&mut model, RequestKind::Refund, 100, Decision::Approve);
            if !settle_first {
                model
                    .settle(PAYMENT, now())
                    .expect("captured, though refunded in full");
            }
            assert_eq!(details(&model).len(), 4);
        }
    }

    #[test]
    fn a_void_and_an_expiry_end_an_authorization() {
        let mut model = holding();
        decided(&mut model, RequestKind::Authorize, 100, Decision::Approve);
        decided(&mut model, RequestKind::Void, 0, Decision::Approve);
        assert!(model.expire(PAYMENT, now()).is_err());

        let other = Uuid::from_u128(2);
        let authorize = Request {
            payment_id: other,
            ..request(RequestKind::Authorize, 100)
        };
        model.take(authorize.clone(), now());
        model
            .decide(authorize.id, Decision::Approve, now())
            .expect("possible");
        model.expire(other, now()).expect("authorized");
        let history = model.history(other).expect("held");
        assert_eq!(
            history.facts.last().map(|fact| &fact.detail),
            Some(&Detail::Expired)
        );
    }

    #[test]
    fn a_decision_that_no_psp_could_make_is_refused() {
        // Each starts from a payment authorized for 100.
        let cases: [(RequestKind, i64, Decision); 8] = [
            (RequestKind::Capture, 101, Decision::Approve),
            (RequestKind::Refund, 1, Decision::Approve),
            (RequestKind::Capture, 1, Decision::Decline),
            (RequestKind::Authorize, 100, Decision::Approve),
            (RequestKind::Authorize, 100, Decision::Decline),
            (RequestKind::Authorize, 100, Decision::Refuse),
            (RequestKind::Void, 0, Decision::Decline),
            (RequestKind::Refund, 1, Decision::ApproveFor(1)),
        ];
        for (kind, amount, decision) in cases {
            let mut model = holding();
            decided(&mut model, RequestKind::Authorize, 100, Decision::Approve);
            let request = request(kind, amount);
            model.take(request.clone(), now());
            assert!(
                model.decide(request.id, decision, now()).is_err(),
                "{decision:?} of {kind:?} for {amount}"
            );
            assert_eq!(
                details(&model).len(),
                1,
                "{decision:?} of {kind:?} wrote a fact"
            );
        }

        // An authorization for more than asked, or for nothing, a request decided twice
        // or never taken, a settlement before a capture.
        let mut model = holding();
        let authorize = request(RequestKind::Authorize, 100);
        model.take(authorize.clone(), now());
        for less in [0, 101] {
            assert!(
                model
                    .decide(authorize.id, Decision::ApproveFor(less), now())
                    .is_err()
            );
        }
        assert!(model.settle(PAYMENT, now()).is_err());
        model
            .decide(authorize.id, Decision::Approve, now())
            .expect("possible");
        assert!(
            model
                .decide(authorize.id, Decision::Approve, now())
                .is_err()
        );
        assert!(
            model
                .decide(Uuid::now_v7(), Decision::Approve, now())
                .is_err()
        );
        assert!(model.settle(Uuid::now_v7(), now()).is_err());
    }

    #[test]
    fn the_binarys_rule_approves_what_it_can_refuses_what_it_can_and_holds_the_rest() {
        let mut model = Model::new(Policy::ApproveAtOnce, TimeDelta::days(7));
        let capture = request(RequestKind::Capture, 100);
        model.take(capture.clone(), now());
        let authorize = request(RequestKind::Authorize, 100);
        model.take(authorize.clone(), now());
        // A second authorization can be neither approved nor declined.
        let second = request(RequestKind::Authorize, 100);
        model.take(second.clone(), now());
        assert_eq!(
            details(&model),
            [
                Detail::CaptureRefused {
                    request_id: capture.id,
                },
                Detail::Authorized {
                    request_id: authorize.id,
                    amount: 100,
                    expires_at: now() + TimeDelta::days(7),
                },
            ]
        );
        assert_eq!(model.requests(), [capture, authorize, second]);
    }

    #[test]
    fn an_appended_fact_bypasses_the_checks_and_is_announced() {
        let mut model = holding();
        model.append(PAYMENT, Detail::Settled, now());
        assert_eq!(details(&model), [Detail::Settled]);
        assert_eq!(model.messages().len(), 1);
        let again = model.notify(PAYMENT, now());
        assert_eq!(model.messages().len(), 2);
        assert_ne!(model.messages()[0].id, again);
    }
}
