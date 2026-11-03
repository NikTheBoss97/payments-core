// Author: Nicola Antonello

//! Random histories of real commands against a small independent model
//! (ADR-0002, ADR-0016, ADR-0020).
//!
//! The model has no table of moves and no ledger: it keeps what has happened to a
//! payment as facts, and adds up each account's balance in plain arithmetic. It
//! predicts each command's outcome, and each payment's status and refundable
//! amount after it. Payments run at the same time, so their postings share the
//! accounts, as in production. The generator is seeded, so a failure repeats.

mod common;

use std::error::Error;

use common::payments::{
    authorize_command, balance_of, capture_command, create_command, eur, fee_rate, refund_command,
    unbalanced_currencies, uncaused_postings,
};
use common::{merchant_id, runtime_pool, transaction_id, transition_id};
use futures_util::future::join_all;
use payments_core::accounts::{AccountRef, Psp};
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::{PaymentStatus, Refused};
use payments_core::money::{BasisPoints, PositiveAmount};
use payments_core::payments::{
    Authorize, AuthorizeError, Capture, CaptureError, CreatePayment, Outcome, Refund, RefundError,
    StepCommand, StepError, authorize, cancel, capture, create_payment, decline, expire,
    find_payment, refund, register_merchant, settle, void,
};
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

#[derive(Debug, Clone, Copy)]
enum Command {
    Authorize(i64),
    Decline,
    Cancel,
    Capture(i64),
    Void,
    Expire,
    Settle,
    Refund(i64),
}

/// What a command should do, according to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Applied,
    Illegal,
    AboveLimit,
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

    /// Applies `command` to the facts, and says what it should do.
    fn apply(&mut self, command: Command, requested: i64) -> Answer {
        let (allowed, within_limit) = match command {
            Command::Authorize(minor) => (self.undecided(), minor <= requested),
            Command::Decline | Command::Cancel => (self.undecided(), true),
            Command::Capture(minor) => (
                self.holding(),
                self.authorized.is_some_and(|limit| minor <= limit),
            ),
            Command::Void | Command::Expire => (self.holding(), true),
            Command::Settle => (self.left().is_some() && !self.settled, true),
            Command::Refund(minor) => (
                self.left().is_some(),
                self.left().is_some_and(|limit| minor <= limit),
            ),
        };
        if !allowed {
            return Answer::Illegal;
        }
        if !within_limit {
            return Answer::AboveLimit;
        }
        match command {
            Command::Authorize(minor) => self.authorized = Some(minor),
            Command::Decline => self.declined = true,
            Command::Cancel | Command::Void | Command::Expire => self.released = true,
            Command::Capture(minor) => self.captured = Some(minor),
            Command::Settle => self.settled = true,
            Command::Refund(minor) => self.refunded += minor,
        }
        Answer::Applied
    }
}

/// How one payment moved the accounts: debits minus credits.
#[derive(Debug, Default, Clone, Copy)]
struct Moved {
    receivable: i64,
    payable: i64,
    fees: i64,
}

/// A command drawn around the payment's limits, so that they are reached and exact
/// refunds are common, while the moves that end a payment are rare enough for most
/// histories to get far along the lifecycle.
fn draw(rng: &mut Rng, requested: i64) -> Command {
    let near = requested + 2;
    match rng.up_to(32) {
        1..=6 => Command::Authorize(rng.up_to(near)),
        7 => Command::Decline,
        8 => Command::Cancel,
        9..=14 => Command::Capture(rng.up_to(near)),
        15 => Command::Void,
        16 => Command::Expire,
        17..=19 => Command::Settle,
        _ => Command::Refund(rng.up_to(near)),
    }
}

/// The outcome of a command, boiled down to what the model predicts, with the
/// reply when it applied.
fn answer<E>(
    result: Result<Outcome, E>,
    refusal: impl Fn(&E) -> Option<Answer>,
    context: &str,
) -> (Answer, Option<Outcome>)
where
    E: std::fmt::Debug,
{
    match result {
        Ok(outcome) => (Answer::Applied, Some(outcome)),
        Err(err) => match refusal(&err) {
            Some(answer) => (answer, None),
            None => panic!("{context}: {err:?}"),
        },
    }
}

fn refused(refused: &Refused) -> Answer {
    match refused {
        Refused::Illegal(_) => Answer::Illegal,
        Refused::AboveLimit { .. } => Answer::AboveLimit,
    }
}

/// Sends a command with these IDs. A retry calls this again with the same `n`, so
/// it sends the identical value.
async fn send(
    app: &PgPool,
    command: Command,
    payment: PaymentId,
    n: u32,
    context: &str,
) -> (Answer, Option<Outcome>) {
    let minor = |value| PositiveAmount::new(value).expect("a small positive amount");
    let plain = StepCommand {
        step: transition_id(n),
        payment,
    };
    let step_refusal = |err: &StepError| match err {
        StepError::Illegal(_) => Some(Answer::Illegal),
        _ => None,
    };
    match command {
        Command::Authorize(value) => {
            let command = Authorize {
                step: transition_id(n),
                amount: minor(value),
                ..authorize_command(0, payment)
            };
            let refusal = |err: &AuthorizeError| match err {
                AuthorizeError::Refused(reason) => Some(refused(reason)),
                _ => None,
            };
            answer(authorize(app, &command).await, refusal, context)
        }
        Command::Capture(value) => {
            let command = Capture {
                step: transition_id(n),
                posting: transaction_id(n),
                amount: minor(value),
                ..capture_command(0, payment)
            };
            let refusal = |err: &CaptureError| match err {
                CaptureError::Refused(reason) => Some(refused(reason)),
                _ => None,
            };
            answer(capture(app, &command).await, refusal, context)
        }
        Command::Refund(value) => {
            let command = Refund {
                step: transition_id(n),
                posting: transaction_id(n),
                ..refund_command(0, payment, value)
            };
            let refusal = |err: &RefundError| match err {
                RefundError::Refused(reason) => Some(refused(reason)),
                _ => None,
            };
            answer(refund(app, &command).await, refusal, context)
        }
        Command::Decline => answer(decline(app, &plain).await, step_refusal, context),
        Command::Cancel => answer(cancel(app, &plain).await, step_refusal, context),
        Command::Void => answer(void(app, &plain).await, step_refusal, context),
        Command::Expire => answer(expire(app, &plain).await, step_refusal, context),
        Command::Settle => answer(settle(app, &plain).await, step_refusal, context),
    }
}

/// Runs one payment's random history against the model, and returns how it moved
/// the accounts according to the model, and where it ended.
async fn history(
    app: &PgPool,
    merchant: MerchantId,
    n: u32,
) -> Result<(Moved, PaymentStatus), Box<dyn Error>> {
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
    create_payment(app, &create).await?;
    let payment = create.id;
    let mut facts = Facts::default();
    let mut moved = Moved::default();
    // Some histories are empty, so that one can end where it starts.
    let rounds = u32::try_from(rng.up_to(25) - 1)?;
    for round in 0..rounds {
        let command = draw(&mut rng, requested);
        // IDs of their own: a step's is unique across all payments.
        let id = 100_000 + n * 100 + round;
        let context = format!("payment {n}, round {round}, {command:?} at {rate} basis points");
        let (got, reply) = send(app, command, payment, id, &context).await;
        let want = facts.apply(command, requested);
        assert_eq!(got, want, "{context}");
        if want == Answer::Applied {
            match command {
                Command::Capture(captured) => {
                    let fee = (captured * i64::from(rate) + 5_000) / 10_000;
                    moved.receivable += captured;
                    moved.payable -= captured - fee;
                    moved.fees -= fee;
                }
                Command::Refund(minor) => {
                    moved.payable += minor;
                    moved.receivable -= minor;
                }
                _ => {}
            }
            // Now and then the same command again, as a client whose answer was
            // lost: the original reply, and no effect on anything.
            if rng.up_to(3) == 1 {
                let Some(Outcome::Applied(recorded)) = reply else {
                    panic!("{context}: {reply:?}");
                };
                let (_, again) = send(app, command, payment, id, &context).await;
                assert_eq!(again, Some(Outcome::AlreadyApplied(recorded)), "{context}");
            }
        }
        let stored = find_payment(app, payment)
            .await?
            .ok_or("the payment exists")?;
        assert_eq!(stored.status, facts.status(), "{context}");
        assert_eq!(stored.refundable, facts.left().unwrap_or(0), "{context}");
    }
    Ok((moved, facts.status()))
}

#[sqlx::test]
async fn random_histories_match_the_model(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let merchants = [merchant_id(1), merchant_id(2), merchant_id(3)];
    for merchant in merchants {
        register_merchant(&app, merchant, fee_rate()).await?;
    }
    let payments: Vec<(MerchantId, u32)> = (1..=40)
        .map(|n: u32| {
            let at = usize::try_from(n).expect("a small n") % merchants.len();
            (merchants[at], n)
        })
        .collect();
    let histories = join_all(
        payments
            .iter()
            .map(|&(merchant, n)| history(&app, merchant, n)),
    )
    .await;

    // Each account's balance, from the entries, against the model's arithmetic.
    let mut receivable = 0;
    let mut fees = 0;
    let mut payable = [0; 3];
    let mut ends = Vec::new();
    for (&(merchant, _), result) in payments.iter().zip(histories) {
        let (moved, end) = result?;
        ends.push(end);
        receivable += moved.receivable;
        fees += moved.fees;
        let at = merchants
            .iter()
            .position(|known| *known == merchant)
            .expect("one of the three");
        payable[at] += moved.payable;
    }
    assert_eq!(
        balance_of(&app, AccountRef::PspReceivable(Psp::Mock, eur())).await?,
        receivable
    );
    assert_eq!(balance_of(&app, AccountRef::FeeRevenue(eur())).await?, fees);
    for (merchant, expected) in merchants.iter().zip(payable) {
        assert_eq!(
            balance_of(&app, AccountRef::MerchantPayable(*merchant, eur())).await?,
            expected,
            "{merchant}"
        );
    }
    // Histories that end everywhere have passed through every status, or they
    // would test little. The seeds are fixed, so this holds until the logic changes.
    for status in PaymentStatus::ALL {
        assert!(ends.contains(status), "no history ended {status}: {ends:?}");
    }
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    assert_eq!(uncaused_postings(&pool).await?, Vec::<Uuid>::new());
    app.close().await;
    Ok(())
}
