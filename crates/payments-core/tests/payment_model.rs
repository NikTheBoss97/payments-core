// Author: Nicola Antonello

//! Random histories of a merchant's requests and the PSP's decisions, against a small
//! independent model (ADR-0002, ADR-0016, ADR-0020, ADR-0039).
//!
//! The model has no table of moves and no ledger: it keeps what has happened to a
//! payment as facts, with the requests that wait for the PSP, and adds up each account's
//! balance in plain arithmetic. It predicts each answer that the merchant gets, and each
//! payment's status and refundable amount after every event. The PSP's decisions are
//! drawn from those that a PSP could make, the fake makes them, and a catch-up applies
//! each one at once (ADR-0031, ADR-0038). Payments run at the same time, so their
//! postings share the accounts, as in production. The generator is seeded, so a failure
//! repeats.
//!
//! Three merchants ask on the tenant role's pool (ADR-0025), where a merchant left behind
//! on a pooled connection would show as a payment that goes missing or a wrong answer,
//! and the system role applies the PSP's word.

mod common;

use std::error::Error;

use common::payments::{
    balance_of, create_command, eur, fee_rate, unbalanced_currencies, uncaused_postings,
};
use common::psp::{caught_up, send};
use common::{merchant_id, request_id, runtime_pool, tenant_pool, transition_id};
use futures_util::future::join_all;
use mock_psp::MockPsp;
use payments_core::accounts::{AccountRef, Psp};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{PaymentStatus, Refused};
use payments_core::money::{BasisPoints, PositiveAmount};
use payments_core::payments::{
    CreatePayment, Outcome, StepCommand, StepError, cancel, create_payment, find_payment,
    register_merchant,
};
use payments_core::requests::{
    Ask, Conflict, RecordRequest, Refusal, RequestError, RequestKind, RequestOutcome,
    RequestRecorded, record_request,
};
use payments_core::tenancy::Tenant;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// SplitMix64, as in the concurrency tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number from 1 to `n`.
    fn up_to(&mut self, n: i64) -> i64 {
        let n = u64::try_from(n).expect("positive");
        i64::try_from(self.next() % n).expect("below n") + 1
    }

    /// One of the choices, with equal chances.
    fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
        let at = self.next() % u64::try_from(choices.len()).expect("a short list");
        choices[usize::try_from(at).expect("a short list")]
    }
}

fn amount(minor: i64) -> PositiveAmount {
    PositiveAmount::new(minor).expect("a small positive amount")
}

/// What happens next to a payment.
#[derive(Debug, Clone, Copy)]
enum Event {
    /// The merchant asks the PSP for something.
    Ask(Ask),
    /// The merchant cancels the payment.
    Cancel,
    /// The PSP decides the oldest request that it holds.
    Decide,
    /// The PSP settles the capture.
    Settle,
    /// The PSP lets the authorization lapse.
    Expire,
}

/// What the merchant should be told, according to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// The request is recorded, or the cancel applied.
    Taken,
    Illegal,
    AboveLimit,
    /// An authorization was asked for already.
    AuthorizationAsked,
    /// A capture or a void of this kind waits for the PSP.
    Pending(RequestKind),
}

/// A request that the PSP holds and has not decided.
#[derive(Debug, Clone, Copy)]
struct Held {
    /// The `n` of its ID, [`request_id`].
    n: u32,
    ask: Ask,
}

/// What has happened to a payment. Its status and what it may do are derived from
/// these facts alone.
#[derive(Debug, Default)]
struct Facts {
    authorized: Option<i64>,
    declined: bool,
    released: bool,
    captured: Option<i64>,
    settled: bool,
    refunded: i64,
    /// Whether the merchant ever asked for an authorization, decided or not.
    authorization_asked: bool,
    /// The requests that wait for the PSP, oldest first.
    held: Vec<Held>,
}

impl Facts {
    fn undecided(&self) -> bool {
        self.authorized.is_none() && !self.declined && !self.released
    }

    fn holding(&self) -> bool {
        self.authorized.is_some() && self.captured.is_none() && !self.released
    }

    /// What is left to refund, if anything.
    fn left(&self) -> Option<i64> {
        self.captured
            .map(|captured| captured - self.refunded)
            .filter(|left| *left > 0)
    }

    fn status(&self) -> PaymentStatus {
        if self.declined {
            PaymentStatus::Failed
        } else if self.released {
            PaymentStatus::Voided
        } else if self.captured.is_some() {
            match (self.left(), self.settled) {
                (None, _) => PaymentStatus::Refunded,
                (Some(_), true) => PaymentStatus::Settled,
                (Some(_), false) => PaymentStatus::Captured,
            }
        } else if self.authorized.is_some() {
            PaymentStatus::Authorized
        } else {
            PaymentStatus::Created
        }
    }

    /// What the refunds that wait for the PSP ask, together.
    fn refunds_held(&self) -> i64 {
        self.held
            .iter()
            .map(|held| match held.ask {
                Ask::Refund(minor) => minor.get(),
                _ => 0,
            })
            .sum()
    }

    /// The capture or the void that waits for the PSP, if one does.
    fn capture_or_void_held(&self) -> Option<RequestKind> {
        self.held.iter().find_map(|held| match held.ask {
            Ask::Capture(_) => Some(RequestKind::Capture),
            Ask::Void => Some(RequestKind::Void),
            _ => None,
        })
    }

    /// Records request `n` if the model takes it, and says what it should get: the table
    /// and the amount's limit first, as for a step (ADR-0016), then the requests already
    /// asked for (ADR-0039).
    fn ask(&mut self, ask: Ask, requested: i64, n: u32) -> Answer {
        let (allowed, within_limit) = match ask {
            Ask::Authorize(minor) => (self.undecided(), minor.get() <= requested),
            Ask::Capture(minor) => (
                self.holding(),
                self.authorized.is_some_and(|limit| minor.get() <= limit),
            ),
            Ask::Void => (self.holding(), true),
            Ask::Refund(minor) => (
                self.left().is_some(),
                self.left()
                    .is_some_and(|left| minor.get() <= left - self.refunds_held()),
            ),
        };
        if !allowed {
            return Answer::Illegal;
        }
        if !within_limit {
            return Answer::AboveLimit;
        }
        match ask {
            Ask::Authorize(_) if self.authorization_asked => return Answer::AuthorizationAsked,
            Ask::Capture(_) | Ask::Void => {
                if let Some(kind) = self.capture_or_void_held() {
                    return Answer::Pending(kind);
                }
            }
            _ => {}
        }
        if let Ask::Authorize(_) = ask {
            self.authorization_asked = true;
        }
        self.held.push(Held { n, ask });
        Answer::Taken
    }

    /// Applies the merchant's cancel if the model takes it, and says what it should get:
    /// the table first, then an authorization asked for (ADR-0039).
    fn cancel(&mut self) -> Answer {
        if !self.undecided() {
            return Answer::Illegal;
        }
        if self.authorization_asked {
            return Answer::AuthorizationAsked;
        }
        self.released = true;
        Answer::Taken
    }
}

/// How one payment moved the accounts: debits minus credits.
#[derive(Debug, Default, Clone, Copy)]
struct Moved {
    receivable: i64,
    payable: i64,
    fees: i64,
}

/// How one history went, by the model's account.
struct Ran {
    moved: Moved,
    /// The status that it ended in.
    end: PaymentStatus,
    /// What its merchant was told, in order.
    told: Vec<Answer>,
    /// Whether the PSP settled the payment once it had refunded all of it, which the
    /// catch-up records without a step (ADR-0016).
    settled_refunded: bool,
}

/// An event drawn around the payment's limits, so that they are reached and exact
/// refunds are common, while the moves that end a payment are rare enough for most
/// histories to get far along the lifecycle.
fn draw(rng: &mut Rng, requested: i64) -> Event {
    let near = requested + 2;
    match rng.up_to(64) {
        1..=6 => Event::Ask(Ask::Authorize(amount(rng.up_to(near)))),
        7 => Event::Cancel,
        8..=17 => Event::Ask(Ask::Capture(amount(rng.up_to(near)))),
        18 => Event::Ask(Ask::Void),
        19..=34 => Event::Ask(Ask::Refund(amount(rng.up_to(near)))),
        35..=60 => Event::Decide,
        61..=62 => Event::Settle,
        _ => Event::Expire,
    }
}

/// What the merchant was told, boiled down to what the model predicts.
fn told_about_request(result: Result<RequestOutcome, RequestError>, context: &str) -> Answer {
    match result {
        Ok(RequestOutcome::Recorded(_)) => Answer::Taken,
        Err(RequestError::Refused(Refusal::Refused(Refused::Illegal(_)))) => Answer::Illegal,
        Err(RequestError::Refused(Refusal::Refused(Refused::AboveLimit { .. }))) => {
            Answer::AboveLimit
        }
        Err(RequestError::Refused(Refusal::Conflict(Conflict::AuthorizationRequested))) => {
            Answer::AuthorizationAsked
        }
        Err(RequestError::Refused(Refusal::Conflict(Conflict::Pending(kind)))) => {
            Answer::Pending(kind)
        }
        other => panic!("{context}: {other:?}"),
    }
}

/// What the merchant was told about its cancel, boiled down the same way, with the
/// reply when it applied.
fn told_about_cancel(
    result: Result<Outcome, StepError>,
    context: &str,
) -> (Answer, Option<Outcome>) {
    match result {
        Ok(outcome) => (Answer::Taken, Some(outcome)),
        Err(StepError::Illegal(_)) => (Answer::Illegal, None),
        Err(StepError::AuthorizationRequested) => (Answer::AuthorizationAsked, None),
        Err(other) => panic!("{context}: {other:?}"),
    }
}

/// The world that each history runs in: the tenant role's pool, which the merchants
/// ask on, the system role's, which applies the PSP's word, and the fake.
struct World {
    tenant: PgPool,
    system: PgPool,
    fake: MockPsp,
}

impl World {
    /// The PSP decides the oldest request that it holds, with a decision that a PSP could
    /// make, and a catch-up applies it at once. The facts and the money follow.
    async fn decide(
        &self,
        rng: &mut Rng,
        facts: &mut Facts,
        moved: &mut Moved,
        rate: i64,
        payment: PaymentId,
    ) {
        if facts.held.is_empty() {
            return;
        }
        let held = facts.held.remove(0);
        let id = request_id(held.n).get();
        let refused = rng.up_to(6) == 1;
        match held.ask {
            Ask::Authorize(_) if refused => {
                self.fake.decline(id);
                facts.declined = true;
            }
            Ask::Authorize(asked) => {
                // Now and then less than asked (ADR-0016).
                let authorized = if asked.get() > 1 && rng.up_to(3) == 1 {
                    let less = rng.up_to(asked.get() - 1);
                    self.fake.approve_for(id, less);
                    less
                } else {
                    self.fake.approve(id);
                    asked.get()
                };
                facts.authorized = Some(authorized);
            }
            // A capture or a void is only approved while the authorization holds.
            Ask::Capture(captured) if !refused && facts.holding() => {
                self.fake.approve(id);
                facts.captured = Some(captured.get());
                let fee = (captured.get() * rate + 5_000) / 10_000;
                moved.receivable += captured.get();
                moved.payable -= captured.get() - fee;
                moved.fees -= fee;
            }
            Ask::Void if !refused && facts.holding() => {
                self.fake.approve(id);
                facts.released = true;
            }
            Ask::Refund(minor) if !refused => {
                self.fake.approve(id);
                facts.refunded += minor.get();
                moved.payable += minor.get();
                moved.receivable -= minor.get();
            }
            Ask::Capture(_) | Ask::Void | Ask::Refund(_) => self.fake.refuse(id),
        }
        caught_up(&self.system, &self.fake, payment).await;
    }

    /// Runs one payment's random history against the model, and returns how it went
    /// according to the model.
    async fn history(&self, merchant: MerchantId, n: u32) -> Result<Ran, Box<dyn Error>> {
        let me = Tenant::new(&self.tenant, merchant);
        let mut told = Vec::new();
        let mut rng = Rng(0x5EED_0000 + u64::from(n));
        // Mostly small, so that an amount drawn near a limit often meets it exactly, and
        // sometimes large, so that the fee's rounding has more to work on.
        let requested = if rng.up_to(3) == 1 {
            rng.up_to(1_000)
        } else {
            rng.up_to(8)
        };
        let rate = rng.pick(&[0_u16, 1, 250, 2_999, 5_000, 10_000]);
        let create = CreatePayment {
            amount: PositiveAmount::new(requested)?,
            fee_rate: BasisPoints::new(rate)?,
            ..create_command(n, merchant)
        };
        create_payment(me, &create).await?;
        let payment = create.id;
        let mut facts = Facts::default();
        let mut moved = Moved::default();
        let mut settled_refunded = false;
        // Some histories are empty, so that one can end where it starts.
        let rounds = u32::try_from(rng.up_to(60) - 1)?;
        for round in 0..rounds {
            let event = draw(&mut rng, requested);
            // IDs of their own: a request's and a step's are unique across all payments.
            let id = 100_000 + n * 100 + round;
            let context = format!("payment {n}, round {round}, {event:?} at {rate} basis points");
            match event {
                Event::Ask(ask) => {
                    let command = RecordRequest {
                        request: request_id(id),
                        payment,
                        ask,
                    };
                    let got = told_about_request(record_request(me, &command).await, &context);
                    let want = facts.ask(ask, requested, id);
                    assert_eq!(got, want, "{context}");
                    told.push(got);
                    if want == Answer::Taken {
                        // The PSP holds it, as the sender would have sent it.
                        send(&self.fake, &command).await;
                        // Now and then the same request again, as a client whose answer was
                        // lost: the same request, and no effect on anything.
                        if rng.up_to(3) == 1 {
                            assert_eq!(
                                record_request(me, &command).await?,
                                RequestOutcome::AlreadyRecorded(RequestRecorded {
                                    request: command.request.get(),
                                    payment,
                                }),
                                "{context}"
                            );
                        }
                    }
                }
                Event::Cancel => {
                    let command = StepCommand {
                        step: transition_id(id),
                        payment,
                    };
                    let (got, reply) = told_about_cancel(cancel(me, &command).await, &context);
                    let want = facts.cancel();
                    assert_eq!(got, want, "{context}");
                    told.push(got);
                    if want == Answer::Taken && rng.up_to(3) == 1 {
                        let Some(Outcome::Applied(recorded)) = reply else {
                            panic!("{context}: {reply:?}");
                        };
                        assert_eq!(
                            cancel(me, &command).await?,
                            Outcome::AlreadyApplied(recorded),
                            "{context}"
                        );
                    }
                }
                Event::Decide => {
                    self.decide(&mut rng, &mut facts, &mut moved, i64::from(rate), payment)
                        .await;
                }
                // Only a capture that the PSP has not settled can be settled, refunded in
                // full or not (ADR-0016).
                Event::Settle if facts.captured.is_some() && !facts.settled => {
                    self.fake.settle(payment.get());
                    caught_up(&self.system, &self.fake, payment).await;
                    facts.settled = true;
                    settled_refunded |= facts.left().is_none();
                }
                Event::Expire if facts.holding() => {
                    self.fake.expire(payment.get());
                    caught_up(&self.system, &self.fake, payment).await;
                    facts.released = true;
                }
                Event::Settle | Event::Expire => {}
            }
            let stored = find_payment(me, payment)
                .await?
                .ok_or("the payment exists")?;
            assert_eq!(stored.status, facts.status(), "{context}");
            assert_eq!(stored.refundable, facts.left().unwrap_or(0), "{context}");
        }
        Ok(Ran {
            moved,
            end: facts.status(),
            told,
            settled_refunded,
        })
    }
}

#[sqlx::test]
async fn random_histories_of_three_merchants_match_the_model(pool: PgPool) -> TestResult {
    let merchants = [merchant_id(1), merchant_id(2), merchant_id(3)];
    let system = runtime_pool(&pool).await?;
    for merchant in merchants {
        register_merchant(&system, merchant, fee_rate()).await?;
    }
    let world = World {
        tenant: tenant_pool(&pool).await?,
        system,
        fake: MockPsp::start().await?,
    };
    // Enough histories for the rarest of the cases checked at the end to come up.
    let payments: Vec<(MerchantId, u32)> = (1..=80)
        .map(|n: u32| {
            let at = usize::try_from(n).expect("a small n") % merchants.len();
            (merchants[at], n)
        })
        .collect();
    let histories = join_all(
        payments
            .iter()
            .map(|&(merchant, n)| world.history(merchant, n)),
    )
    .await;

    // Each account's balance, from the entries, against the model's arithmetic.
    let mut receivable = 0;
    let mut fees = 0;
    let mut payable = [0; 3];
    let mut ends = Vec::new();
    let mut told = Vec::new();
    let mut settled_refunded = false;
    for (&(merchant, _), result) in payments.iter().zip(histories) {
        let Ran {
            moved,
            end,
            told: answers,
            settled_refunded: settled,
        } = result?;
        ends.push(end);
        told.extend(answers);
        settled_refunded |= settled;
        receivable += moved.receivable;
        fees += moved.fees;
        let at = merchants
            .iter()
            .position(|known| *known == merchant)
            .expect("one of the three");
        payable[at] += moved.payable;
    }
    assert_eq!(
        balance_of(&pool, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        receivable
    );
    assert_eq!(
        balance_of(&pool, AccountRef::FeeRevenue(eur())).await?,
        fees
    );
    for (merchant, expected) in merchants.iter().zip(payable) {
        assert_eq!(
            balance_of(&pool, AccountRef::MerchantPayable(*merchant, eur())).await?,
            expected,
            "{merchant}"
        );
    }
    // Histories that end everywhere have passed through every status, or they
    // would test little. The seeds are fixed, so this holds until the logic changes.
    for status in PaymentStatus::ALL {
        assert!(ends.contains(status), "no history ended {status}: {ends:?}");
    }
    // So have the requests and the cancels met every rule that refuses them.
    for answer in [
        Answer::Taken,
        Answer::Illegal,
        Answer::AboveLimit,
        Answer::AuthorizationAsked,
        Answer::Pending(RequestKind::Capture),
        Answer::Pending(RequestKind::Void),
    ] {
        assert!(told.contains(&answer), "no merchant was told {answer:?}");
    }
    // And the PSP has settled a payment that it had refunded in full, which the catch-up
    // records without a step, so that no status shows it (ADR-0016).
    assert!(
        settled_refunded,
        "no history settled a payment refunded in full"
    );
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    world.tenant.close().await;
    world.system.close().await;
    Ok(())
}
