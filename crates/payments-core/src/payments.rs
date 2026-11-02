// Author: Nicola Antonello

//! Merchants, and the commands that change a payment (ADR-0017, ADR-0019).
//!
//! A command is plain data that carries every ID, and a retry is the identical
//! value sent again: it gets the stored reply, whatever has happened to the
//! payment since. No error converts from `sqlx::Error` with `#[from]`, so no
//! `?` can skip the classifier, which says what each database error proves.
//!
//! Never wrap a command in a client-side timeout or `select!`: sqlx cannot
//! cancel the server's work, and a dropped `COMMIT` is a lost one (ADR-0014).
//! Waits are bounded on the server (ADR-0018).
//!
//! The lints are the same as in `money`: this module handles amounts.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::accounts::{AccountError, AccountRef, Psp, open_account};
use crate::db::{CommitError, code_and_constraint, commit};
use crate::ids::{AccountId, MerchantId, PaymentId, TransitionId};
use crate::lifecycle::{
    self, Event, IllegalTransition, PaymentStatus, Refused, StepKind, Transition, next,
};
use crate::money::{BasisPoints, Currency, PositiveAmount};
use crate::times::BusinessTime;

/// Where a command landed: its step's `seq`, the statuses it moved between,
/// and the refundable amount after it. A retry gets the same values, read from
/// the stored step (ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub seq: i32,
    pub from: PaymentStatus,
    pub to: PaymentStatus,
    pub refundable: i64,
}

/// What a command did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// This call stored the step.
    Applied(Recorded),
    /// An earlier call stored it, with the same content.
    AlreadyApplied(Recorded),
}

/// Why a merchant could not be registered.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// The database rejected what the types rule out: a bug, never retried
    /// (ADR-0013).
    #[error("the database rejected merchant {merchant} as invalid")]
    Invariant {
        merchant: MerchantId,
        #[source]
        source: sqlx::Error,
    },
    /// Any other failure. Registering converges on the ID, so calling it again
    /// is safe.
    #[error("merchant {merchant} may or may not be registered: register it again")]
    NotConfirmed {
        merchant: MerchantId,
        #[source]
        source: sqlx::Error,
    },
}

/// Registers a merchant (ADR-0017). Calling it again with the same ID is safe.
///
/// # Errors
///
/// [`RegisterError::Invariant`] for a bug, and [`RegisterError::NotConfirmed`]
/// for anything else.
pub async fn register_merchant(pool: &PgPool, id: MerchantId) -> Result<(), RegisterError> {
    sqlx::query!(
        "INSERT INTO merchants (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
        id.get()
    )
    .execute(pool)
    .await
    .map_err(|source| match failure_of(&source) {
        Failure::Duplicate | Failure::Invariant => RegisterError::Invariant {
            merchant: id,
            source,
        },
        Failure::NotConfirmed => RegisterError::NotConfirmed {
            merchant: id,
            source,
        },
    })?;
    Ok(())
}

/// A payment as stored: its terms, which never change, and where it stands,
/// which is its newest step (ADR-0017).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payment {
    pub merchant: MerchantId,
    pub psp: Psp,
    pub currency: Currency,
    /// The requested amount.
    pub amount: PositiveAmount,
    pub fee_rate: BasisPoints,
    pub status: PaymentStatus,
    /// What can still be refunded.
    pub refundable: i64,
}

/// Why a payment could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// Any failure. Reading again is safe.
    #[error("payment {payment} could not be read")]
    Database {
        payment: PaymentId,
        #[source]
        source: sqlx::Error,
    },
}

/// Reads a payment, or `None` when no payment has this ID.
///
/// # Errors
///
/// [`ReadError::Database`] when the payment cannot be read.
pub async fn find_payment(pool: &PgPool, id: PaymentId) -> Result<Option<Payment>, ReadError> {
    // One statement, so the terms and the newest step come from one snapshot.
    let row = sqlx::query!(
        r#"SELECT p.merchant_id, p.psp, p.currency AS "currency!", p.amount, p.fee_bps,
                  newest.to_status, newest.refundable
             FROM payments p
            CROSS JOIN LATERAL (
                  SELECT t.to_status, t.refundable
                    FROM payment_transitions t
                   WHERE t.payment_id = p.id
                   ORDER BY t.seq DESC
                   LIMIT 1
                  ) newest
            WHERE p.id = $1"#,
        id.get()
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| ReadError::Database {
        payment: id,
        source,
    })?;
    Ok(row.map(|row| {
        let fee_bps = u16::try_from(row.fee_bps).expect("payments_fee_bps_check keeps it in range");
        Payment {
            merchant: MerchantId::try_from(row.merchant_id)
                .expect("merchants_id_check allows only UUIDv7 IDs"),
            psp: Psp::parse(&row.psp).expect("payments_psp_check allows only known labels"),
            currency: Currency::parse(&row.currency)
                .expect("the currency_code domain allows only well-formed codes"),
            amount: PositiveAmount::new(row.amount)
                .expect("payments_amount_check keeps it between 1 and the cap"),
            fee_rate: BasisPoints::new(fee_bps).expect("payments_fee_bps_check keeps it in range"),
            status: parse_status(&row.to_status),
            refundable: row.refundable,
        }
    }))
}

/// Candidate IDs for a payment's accounts: each becomes an account's ID only if
/// that account's key is new (ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountCandidates {
    pub fee_revenue: AccountId,
    pub merchant_payable: AccountId,
    pub psp_receivable: AccountId,
}

/// The command that creates a payment.
#[derive(Debug)]
pub struct CreatePayment {
    pub id: PaymentId,
    /// The ID of the payment's first step, which identifies this command.
    pub step: TransitionId,
    pub merchant: MerchantId,
    pub psp: Psp,
    pub currency: Currency,
    /// The requested amount.
    pub amount: PositiveAmount,
    /// The fee rate, fixed for the payment's life (ADR-0020).
    pub fee_rate: BasisPoints,
    pub accounts: AccountCandidates,
}

/// Why a payment could not be created. Only [`CreateError::NotConfirmed`] may
/// have stored it.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// No merchant has this ID. Nothing is stored, and no account is opened.
    #[error("merchant {merchant} is not registered")]
    UnknownMerchant { merchant: MerchantId },
    /// No currency has this code. Nothing is stored.
    #[error("currency {currency} is not in the currencies table")]
    UnknownCurrency {
        currency: Currency,
        #[source]
        source: sqlx::Error,
    },
    /// An account could not be opened for a reason that the types and the chart
    /// of accounts rule out, such as a candidate ID that another account has: a
    /// bug, never retried. The payment is not stored.
    #[error("an account of the payment could not be opened")]
    Account {
        #[source]
        source: AccountError,
    },
    /// The command's step ID is stored with other content: a bug, never
    /// retried.
    #[error("step {step} is stored with other content")]
    Mismatch { step: Uuid },
    /// The database caught what the domain types should have prevented: a bug,
    /// never retried.
    #[error("payment {payment}: the database rejected it as invalid")]
    Invariant {
        payment: PaymentId,
        #[source]
        source: sqlx::Error,
    },
    /// The payment may or may not be stored. Send the identical command again
    /// later, never one with new IDs (ADR-0019).
    #[error("payment {payment} may or may not be stored: send the command again")]
    NotConfirmed {
        payment: PaymentId,
        #[source]
        source: sqlx::Error,
    },
}

/// Creates a payment: it checks that the merchant is registered, opens the
/// payment's accounts, each committing on its own, and then stores the payment
/// with its first step in one database transaction. It takes no lock: a
/// concurrent insert of the same payment waits on the primary key (ADR-0019).
///
/// # Errors
///
/// See [`CreateError`].
pub async fn create_payment(
    pool: &PgPool,
    command: &CreatePayment,
) -> Result<Outcome, CreateError> {
    let payment = command.id;
    let fail = |source| create_failure(payment, source);
    // Merchants are append-only, so this answer cannot go stale.
    let registered = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM merchants WHERE id = $1) AS "registered!""#,
        command.merchant.get()
    )
    .fetch_one(pool)
    .await
    .map_err(fail)?;
    if !registered {
        return Err(CreateError::UnknownMerchant {
            merchant: command.merchant,
        });
    }
    for (reference, candidate) in accounts_to_open(command) {
        open_account(pool, candidate, &reference.key(), reference.normal_side())
            .await
            .map_err(|err| match err {
                AccountError::UnknownCurrency { currency, source } => {
                    CreateError::UnknownCurrency { currency, source }
                }
                AccountError::Database { source, .. } => fail(source),
                AccountError::SideMismatch { .. } | AccountError::Invariant { .. } => {
                    CreateError::Account { source: err }
                }
            })?;
    }
    let mut tx = pool.begin().await.map_err(fail)?;
    if let Err(source) = insert_payment(&mut tx, command).await {
        // The database transaction is aborted. Dropping it queues the ROLLBACK;
        // committing it would report success (ADR-0014).
        drop(tx);
        return match failure_of(&source) {
            Failure::Duplicate => stored_create(pool, command, source).await,
            Failure::Invariant => Err(CreateError::Invariant { payment, source }),
            Failure::NotConfirmed => Err(CreateError::NotConfirmed { payment, source }),
        };
    }
    match commit(tx).await {
        Ok(()) => {
            let first = Transition::genesis();
            Ok(Outcome::Applied(Recorded {
                seq: 0,
                from: first.from(),
                to: first.to(),
                refundable: 0,
            }))
        }
        Err(CommitError::Rejected(source)) => Err(CreateError::Invariant { payment, source }),
        Err(CommitError::Unknown(source)) => Err(CreateError::NotConfirmed { payment, source }),
    }
}

/// A payment's accounts, with their candidate IDs, in sorted key order
/// (ADR-0019): their kinds differ, and are listed alphabetically.
fn accounts_to_open(command: &CreatePayment) -> [(AccountRef, AccountId); 3] {
    let CreatePayment {
        merchant,
        psp,
        currency,
        accounts,
        ..
    } = *command;
    [
        (AccountRef::FeeRevenue(currency), accounts.fee_revenue),
        (
            AccountRef::MerchantPayable(merchant, currency),
            accounts.merchant_payable,
        ),
        (
            AccountRef::PspReceivable(psp, currency),
            accounts.psp_receivable,
        ),
    ]
}

/// What a create's database error means, outside the insert that can meet a
/// duplicate key.
fn create_failure(payment: PaymentId, source: sqlx::Error) -> CreateError {
    match failure_of(&source) {
        Failure::Duplicate | Failure::Invariant => CreateError::Invariant { payment, source },
        Failure::NotConfirmed => CreateError::NotConfirmed { payment, source },
    }
}

async fn insert_payment(
    tx: &mut Transaction<'_, Postgres>,
    command: &CreatePayment,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payments (id, merchant_id, psp, currency, amount, fee_bps) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        command.id.get(),
        command.merchant.get(),
        command.psp.as_str(),
        command.currency.as_str(),
        command.amount.get(),
        i32::from(command.fee_rate.get())
    )
    .execute(&mut **tx)
    .await?;
    insert_step(
        tx,
        &NewStep {
            payment: command.id,
            id: &command.step,
            seq: 0,
            prev_seq: 0,
            step: Transition::genesis(),
            amount: 0,
            prev_refundable: 0,
            refundable: 0,
        },
    )
    .await
}

/// A create met a duplicate key, so some ID exists, and the step with the
/// command's ID decides (ADR-0019). What it finds is committed: an insert that
/// meets an uncommitted row with the same key waits for that transaction to
/// end, and reports the duplicate only if it committed.
async fn stored_create(
    pool: &PgPool,
    command: &CreatePayment,
    duplicate: sqlx::Error,
) -> Result<Outcome, CreateError> {
    let payment = command.id;
    let row = sqlx::query!(
        r#"SELECT s.payment_id, s.seq, s.kind, s.from_status, s.to_status, s.refundable,
                  p.merchant_id, p.psp, p.currency AS "currency!", p.amount, p.fee_bps
             FROM payment_transitions s
             JOIN payments p ON p.id = s.payment_id
            WHERE s.id = $1"#,
        command.step.get()
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| create_failure(payment, source))?;
    let Some(row) = row else {
        // Nothing explains the duplicate: another writer broke a rule.
        return Err(CreateError::Invariant {
            payment,
            source: duplicate,
        });
    };
    // A first step's amount is 0, as payment_transitions_money_check requires
    // of its kind, so comparing the kind compares the amount.
    let same = row.payment_id == payment.get()
        && row.kind == StepKind::Create.as_str()
        && row.merchant_id == command.merchant.get()
        && row.psp == command.psp.as_str()
        && row.currency == command.currency.as_str()
        && row.amount == command.amount.get()
        && row.fee_bps == i32::from(command.fee_rate.get());
    if !same {
        return Err(CreateError::Mismatch {
            step: command.step.get(),
        });
    }
    Ok(Outcome::AlreadyApplied(Recorded {
        seq: row.seq,
        from: parse_status(&row.from_status),
        to: parse_status(&row.to_status),
        refundable: row.refundable,
    }))
}

/// The command that records an authorization (ADR-0016).
#[derive(Debug)]
pub struct Authorize {
    /// The step's ID, which identifies this command.
    pub step: TransitionId,
    pub payment: PaymentId,
    /// The authorized amount, at most the requested one.
    pub amount: PositiveAmount,
    /// The deadline that the PSP reported, never enforced by a clock here
    /// (ADR-0016).
    pub expires_at: BusinessTime,
}

/// Why an [`Authorize`] was not applied. Only [`AuthorizeError::NotConfirmed`]
/// may have stored its step.
#[derive(Debug, thiserror::Error)]
pub enum AuthorizeError {
    /// No payment has this ID.
    #[error("payment {payment} does not exist")]
    NotFound { payment: PaymentId },
    /// The status or the amount refuses it. Nothing is stored, and a retry is
    /// decided again (ADR-0019).
    #[error(transparent)]
    Refused(Refused),
    /// The command's step ID is stored with other content: a bug, never
    /// retried.
    #[error("step {step} is stored with other content")]
    Mismatch { step: Uuid },
    /// The database caught what the domain types should have prevented, or a
    /// writer that skipped the payment's lock got there first (ADR-0018): a bug,
    /// never retried.
    #[error("step {step}: the database rejected it as invalid")]
    Invariant {
        step: Uuid,
        #[source]
        source: sqlx::Error,
    },
    /// The step may or may not be stored. Send the identical command again
    /// later, never one with new IDs (ADR-0019).
    #[error("step {step} may or may not be stored: send the command again")]
    NotConfirmed {
        step: Uuid,
        #[source]
        source: sqlx::Error,
    },
}

impl AuthorizeError {
    fn new(command: &Authorize, failed: Failed<Refused>) -> Self {
        let step = command.step.get();
        match failed {
            Failed::NotFound => Self::NotFound {
                payment: command.payment,
            },
            Failed::Refused(refused) => Self::Refused(refused),
            Failed::Mismatch => Self::Mismatch { step },
            Failed::Duplicate(source) | Failed::Invariant(source) => {
                Self::Invariant { step, source }
            }
            Failed::NotConfirmed(source) => Self::NotConfirmed { step, source },
        }
    }
}

/// Records that the PSP authorized a payment, for at most the requested amount
/// (ADR-0016).
///
/// # Errors
///
/// See [`AuthorizeError`].
pub async fn authorize(pool: &PgPool, command: &Authorize) -> Result<Outcome, AuthorizeError> {
    let plan = Plan {
        id: &command.step,
        asked: Asked {
            payment: command.payment,
            kind: StepKind::Authorize,
            amount: command.amount.get(),
            detail: Detail::Authorization {
                expires_at: command.expires_at,
            },
        },
    };
    apply(pool, &plan, |state| {
        lifecycle::authorize(state.status, command.amount, state.requested)
    })
    .await
    .map_err(|failed| AuthorizeError::new(command, failed))
}

/// A command that moves a payment and carries nothing else: decline, cancel,
/// void, expire or settle.
#[derive(Debug)]
pub struct StepCommand {
    /// The step's ID, which identifies this command.
    pub step: TransitionId,
    pub payment: PaymentId,
}

/// Why a [`StepCommand`] was not applied. Only [`StepError::NotConfirmed`] may
/// have stored its step.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// No payment has this ID.
    #[error("payment {payment} does not exist")]
    NotFound { payment: PaymentId },
    /// The payment's status does not allow the event. Nothing is stored, and a
    /// retry is decided again (ADR-0019).
    #[error(transparent)]
    Illegal(IllegalTransition),
    /// The command's step ID is stored with other content: a bug, never
    /// retried.
    #[error("step {step} is stored with other content")]
    Mismatch { step: Uuid },
    /// The database caught what the domain types should have prevented, or a
    /// writer that skipped the payment's lock got there first (ADR-0018): a bug,
    /// never retried.
    #[error("step {step}: the database rejected it as invalid")]
    Invariant {
        step: Uuid,
        #[source]
        source: sqlx::Error,
    },
    /// The step may or may not be stored. Send the identical command again
    /// later, never one with new IDs (ADR-0019).
    #[error("step {step} may or may not be stored: send the command again")]
    NotConfirmed {
        step: Uuid,
        #[source]
        source: sqlx::Error,
    },
}

impl StepError {
    fn new(command: &StepCommand, failed: Failed<IllegalTransition>) -> Self {
        let step = command.step.get();
        match failed {
            Failed::NotFound => Self::NotFound {
                payment: command.payment,
            },
            Failed::Refused(illegal) => Self::Illegal(illegal),
            Failed::Mismatch => Self::Mismatch { step },
            Failed::Duplicate(source) | Failed::Invariant(source) => {
                Self::Invariant { step, source }
            }
            Failed::NotConfirmed(source) => Self::NotConfirmed { step, source },
        }
    }
}

/// Records that the PSP declined the authorization (ADR-0016).
///
/// # Errors
///
/// See [`StepError`].
pub async fn decline(pool: &PgPool, command: &StepCommand) -> Result<Outcome, StepError> {
    plain_step(pool, command, Event::Decline, StepKind::Decline).await
}

/// Cancels a payment before its authorization (ADR-0016).
///
/// # Errors
///
/// See [`StepError`].
pub async fn cancel(pool: &PgPool, command: &StepCommand) -> Result<Outcome, StepError> {
    plain_step(pool, command, Event::Cancel, StepKind::Cancel).await
}

/// Records that an authorization was voided (ADR-0016).
///
/// # Errors
///
/// See [`StepError`].
pub async fn void(pool: &PgPool, command: &StepCommand) -> Result<Outcome, StepError> {
    plain_step(pool, command, Event::Void, StepKind::Void).await
}

/// Records that the PSP reported an authorization as expired, a call that only
/// the PSP can make (ADR-0016).
///
/// # Errors
///
/// See [`StepError`].
pub async fn expire(pool: &PgPool, command: &StepCommand) -> Result<Outcome, StepError> {
    plain_step(pool, command, Event::Expire, StepKind::Expire).await
}

/// Records that the PSP reported a capture as settled (ADR-0016).
///
/// # Errors
///
/// See [`StepError`].
pub async fn settle(pool: &PgPool, command: &StepCommand) -> Result<Outcome, StepError> {
    plain_step(pool, command, Event::Settle, StepKind::Settle).await
}

async fn plain_step(
    pool: &PgPool,
    command: &StepCommand,
    event: Event,
    kind: StepKind,
) -> Result<Outcome, StepError> {
    let plan = Plan {
        id: &command.step,
        asked: Asked {
            payment: command.payment,
            kind,
            amount: 0,
            detail: Detail::None,
        },
    };
    apply(pool, &plan, |state| next(state.status, event))
        .await
        .map_err(|failed| StepError::new(command, failed))
}

/// The key of a payment's advisory lock (ADR-0018): the low 64 bits of its ID,
/// read as a big-endian `i64`. RFC 9562's variant bits are the top bits of that
/// half, so every payment's key is negative, apart from other uses of advisory
/// locks, which take non-negative keys.
#[must_use]
pub fn lock_key(id: PaymentId) -> i64 {
    let (_, low) = id.get().as_u64_pair();
    i64::from_be_bytes(low.to_be_bytes())
}

/// What a command asks for, in the terms that a retry compares. It leaves out
/// where the command landed: `seq`, the statuses and the refundable amount
/// (ADR-0019).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Asked {
    payment: PaymentId,
    kind: StepKind,
    amount: i64,
    detail: Detail,
}

/// What a step's record holds that a retry compares.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Detail {
    /// This kind of step has no record.
    None,
    Authorization {
        expires_at: BusinessTime,
    },
}

/// A command as its unit of work sees it.
pub(crate) struct Plan<'a> {
    id: &'a TransitionId,
    asked: Asked,
}

/// A stored step: what its command asked for, and the reply a retry gets.
struct Stored {
    asked: Asked,
    recorded: Recorded,
}

/// What a command's one read under the lock found.
pub(crate) struct State {
    /// The newest step's `seq`, status and refundable amount.
    seq: i32,
    status: PaymentStatus,
    refundable: i64,
    /// The payment's requested amount.
    requested: PositiveAmount,
    /// The step with the command's ID, on any payment.
    stored: Option<Stored>,
}

/// A command's failure, before its public function states it in its own enum.
/// `R` is what that command's rules can refuse.
pub(crate) enum Failed<R> {
    NotFound,
    Refused(R),
    Mismatch,
    /// Some ID already exists, and the command has yet to look its step up.
    /// Reaching the caller, it is a duplicate that nothing explains: a bug.
    Duplicate(sqlx::Error),
    Invariant(sqlx::Error),
    NotConfirmed(sqlx::Error),
}

impl<R> Failed<R> {
    fn database(source: sqlx::Error) -> Self {
        match failure_of(&source) {
            Failure::Duplicate => Self::Duplicate(source),
            Failure::Invariant => Self::Invariant(source),
            Failure::NotConfirmed => Self::NotConfirmed(source),
        }
    }
}

/// A command's unit of work, from `BEGIN` to `COMMIT` (ADR-0019).
async fn apply<R>(
    pool: &PgPool,
    plan: &Plan<'_>,
    decide: impl FnOnce(&State) -> Result<Transition, R>,
) -> Result<Outcome, Failed<R>> {
    let mut tx = begin_locked(pool, plan.asked.payment).await?;
    match apply_in(&mut tx, plan, decide).await {
        Ok(Outcome::Applied(recorded)) => match commit(tx).await {
            Ok(()) => Ok(Outcome::Applied(recorded)),
            Err(CommitError::Rejected(source)) => Err(Failed::Invariant(source)),
            Err(CommitError::Unknown(source)) => Err(Failed::NotConfirmed(source)),
        },
        // A retry wrote nothing, so dropping the transaction ends it.
        Ok(replayed) => Ok(replayed),
        Err(Failed::Duplicate(duplicate)) => {
            // The failure aborted the transaction, which released the lock at
            // once (ADR-0018), and the lookup takes it again. Dropping the
            // transaction first gives its connection back: a command that asks
            // the pool for a connection while it holds one can wait for itself.
            drop(tx);
            look_up_again(pool, plan, duplicate).await
        }
        // A rejection wrote nothing, and any other failure aborted the
        // transaction: drop it, because committing it would report success
        // (ADR-0014).
        Err(failed) => Err(failed),
    }
}

/// Begins a database transaction whose first statement takes the payment's lock
/// (ADR-0018). A session-level lock would outlive the transaction, so this is
/// the only lock function that the library calls. `lock_timeout` bounds the
/// wait, on the server.
async fn begin_locked<R>(
    pool: &PgPool,
    payment: PaymentId,
) -> Result<Transaction<'static, Postgres>, Failed<R>> {
    let mut tx = pool.begin().await.map_err(Failed::database)?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", lock_key(payment))
        .execute(&mut *tx)
        .await
        .map_err(Failed::database)?;
    Ok(tx)
}

/// Steps 3 to 6 of a command (ADR-0019), inside a database transaction that the
/// caller has begun and locked, so that later phases can add to the same unit of
/// work. It never begins or commits. After an error, drop the transaction.
pub(crate) async fn apply_in<R>(
    tx: &mut Transaction<'_, Postgres>,
    plan: &Plan<'_>,
    decide: impl FnOnce(&State) -> Result<Transition, R>,
) -> Result<Outcome, Failed<R>> {
    let state = read_state(tx, plan).await?;
    // Identity first: a retry that waited behind its own first attempt finds
    // that attempt's step, and checked against the state it would be refused.
    if let Some(stored) = state.stored {
        return answer(stored, plan).map(Outcome::AlreadyApplied);
    }
    let step = decide(&state).map_err(Failed::Refused)?;
    let seq = state
        .seq
        .checked_add(1)
        .expect("a payment has far fewer steps than an integer holds");
    insert_step(
        tx,
        &NewStep {
            payment: plan.asked.payment,
            id: plan.id,
            seq,
            prev_seq: state.seq,
            step,
            amount: plan.asked.amount,
            prev_refundable: state.refundable,
            refundable: state.refundable,
        },
    )
    .await
    .map_err(Failed::database)?;
    insert_record(tx, plan, &state)
        .await
        .map_err(Failed::database)?;
    Ok(Outcome::Applied(Recorded {
        seq,
        from: step.from(),
        to: step.to(),
        refundable: state.refundable,
    }))
}

/// A write met a duplicate key, so some ID exists. Under the lock again, the
/// step with the command's ID decides, and none means that another writer broke
/// a rule (ADR-0019).
async fn look_up_again<R>(
    pool: &PgPool,
    plan: &Plan<'_>,
    duplicate: sqlx::Error,
) -> Result<Outcome, Failed<R>> {
    let mut tx = begin_locked(pool, plan.asked.payment).await?;
    match read_state(&mut tx, plan).await?.stored {
        Some(stored) => answer(stored, plan).map(Outcome::AlreadyApplied),
        None => Err(Failed::Invariant(duplicate)),
    }
}

/// The stored reply when the stored step is this command's.
fn answer<R>(stored: Stored, plan: &Plan<'_>) -> Result<Recorded, Failed<R>> {
    if stored.asked == plan.asked {
        Ok(stored.recorded)
    } else {
        Err(Failed::Mismatch)
    }
}

/// The one read under the lock (ADR-0019). It must come in a later statement
/// than the lock: at READ COMMITTED each statement takes a new snapshot, so this
/// one sees every step that the lock's previous holder committed (ADR-0018).
async fn read_state<R>(
    tx: &mut Transaction<'_, Postgres>,
    plan: &Plan<'_>,
) -> Result<State, Failed<R>> {
    let row = sqlx::query!(
        r#"SELECT newest.seq, newest.to_status, newest.refundable,
                  p.amount AS requested,
                  stored.payment_id AS "stored_payment?",
                  stored.seq AS "stored_seq?",
                  stored.kind AS "stored_kind?",
                  stored.from_status AS "stored_from?",
                  stored.to_status AS "stored_to?",
                  stored.amount AS "stored_amount?",
                  stored.refundable AS "stored_refundable?",
                  stored_authorization.expires_at AS "expires_at?"
             FROM payments p
            CROSS JOIN LATERAL (
                  SELECT t.seq, t.to_status, t.refundable
                    FROM payment_transitions t
                   WHERE t.payment_id = p.id
                   ORDER BY t.seq DESC
                   LIMIT 1
                  ) newest
             LEFT JOIN payment_transitions stored ON stored.id = $2
             LEFT JOIN payment_authorizations stored_authorization
                    ON stored_authorization.transition_id = stored.id
            WHERE p.id = $1"#,
        plan.asked.payment.get(),
        plan.id.get()
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(Failed::database)?
    .ok_or(Failed::NotFound)?;
    // The join finds a whole row or none, and every column is NOT NULL.
    let stored = match (
        row.stored_payment,
        row.stored_seq,
        row.stored_kind,
        row.stored_from,
        row.stored_to,
        row.stored_amount,
        row.stored_refundable,
    ) {
        (
            Some(payment),
            Some(seq),
            Some(kind),
            Some(from),
            Some(to),
            Some(amount),
            Some(refundable),
        ) => {
            let kind = StepKind::parse(&kind)
                .expect("payment_transitions_edge_check allows only known kinds");
            Some(Stored {
                asked: Asked {
                    payment: PaymentId::try_from(payment)
                        .expect("payments_id_check allows only UUIDv7 IDs"),
                    kind,
                    amount,
                    detail: stored_detail(kind, row.expires_at),
                },
                recorded: Recorded {
                    seq,
                    from: parse_status(&from),
                    to: parse_status(&to),
                    refundable,
                },
            })
        }
        _ => None,
    };
    Ok(State {
        seq: row.seq,
        status: parse_status(&row.to_status),
        refundable: row.refundable,
        requested: PositiveAmount::new(row.requested)
            .expect("payments_amount_check keeps it between 1 and the cap"),
        stored,
    })
}

/// What a stored step's record holds. Its kind says which record it has, and a
/// committed step of a kind that carries data has its record (LG004).
fn stored_detail(kind: StepKind, expires_at: Option<DateTime<Utc>>) -> Detail {
    match kind {
        StepKind::Authorize => Detail::Authorization {
            expires_at: stored_time(expires_at.expect("LG004 keeps an authorization's record")),
        },
        StepKind::Create
        | StepKind::Decline
        | StepKind::Cancel
        | StepKind::Capture
        | StepKind::Void
        | StepKind::Expire
        | StepKind::Settle
        | StepKind::Refund => Detail::None,
    }
}

fn stored_time(time: DateTime<Utc>) -> BusinessTime {
    BusinessTime::new(time).expect("a time read from the database can be stored")
}

/// One row of a payment's history. Its move can come only from a [`Transition`],
/// which only the table of moves can make (ADR-0016).
struct NewStep<'a> {
    payment: PaymentId,
    id: &'a TransitionId,
    seq: i32,
    prev_seq: i32,
    step: Transition,
    amount: i64,
    prev_refundable: i64,
    refundable: i64,
}

/// Writes a step. This is the only function that does.
async fn insert_step(tx: &mut Transaction<'_, Postgres>, row: &NewStep<'_>) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_transitions \
             (payment_id, seq, prev_seq, id, kind, from_status, to_status, \
              amount, prev_refundable, refundable) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        row.payment.get(),
        row.seq,
        row.prev_seq,
        row.id.get(),
        row.step.kind().as_str(),
        row.step.from().as_str(),
        row.step.to().as_str(),
        row.amount,
        row.prev_refundable,
        row.refundable
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Writes the record of a step whose kind carries data, after its step, whose row
/// the record's foreign key refers to (ADR-0017).
async fn insert_record(
    tx: &mut Transaction<'_, Postgres>,
    plan: &Plan<'_>,
    state: &State,
) -> sqlx::Result<()> {
    match plan.asked.detail {
        Detail::None => Ok(()),
        Detail::Authorization { expires_at } => {
            sqlx::query!(
                "INSERT INTO payment_authorizations \
                     (transition_id, payment_id, amount, payment_amount, expires_at) \
                 VALUES ($1, $2, $3, $4, $5)",
                plan.id.get(),
                plan.asked.payment.get(),
                plan.asked.amount,
                state.requested.get(),
                expires_at.get()
            )
            .execute(&mut **tx)
            .await?;
            Ok(())
        }
    }
}

fn parse_status(label: &str) -> PaymentStatus {
    PaymentStatus::parse(label).expect("payment_transitions_edge_check allows only known statuses")
}

/// What a failed statement means (ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// Some ID already exists, and the step with the command's ID decides.
    Duplicate,
    /// The database caught what the domain types should have prevented, or ran
    /// out of shared memory for locks (ADR-0018): a bug, never retried.
    Invariant,
    /// The command may or may not have been applied.
    NotConfirmed,
}

/// The one classifier (ADR-0019), a pure function of the SQLSTATE: no rule needs
/// the constraint's name, because whichever unique key PostgreSQL reports, the
/// command looks its step up. A `COMMIT` is read by `db::commit` instead.
fn classify(code: Option<&str>) -> Failure {
    match code {
        Some("23505") => Failure::Duplicate,
        Some("53200") => Failure::Invariant,
        Some(code) if code.starts_with("23") || code.starts_with("LG") => Failure::Invariant,
        _ => Failure::NotConfirmed,
    }
}

fn failure_of(err: &sqlx::Error) -> Failure {
    let (code, _) = code_and_constraint(err);
    classify(code.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use uuid::uuid;

    #[test]
    fn a_failed_statement_is_classified_by_its_code() {
        use Failure::{Duplicate, Invariant, NotConfirmed};
        for (code, expected) in [
            // Whichever unique key PostgreSQL reports (ADR-0019).
            (Some("23505"), Duplicate),
            // Every other integrity violation, and every code of ours.
            (Some("23502"), Invariant),
            (Some("23503"), Invariant),
            (Some("23514"), Invariant),
            (Some("LG001"), Invariant),
            (Some("LG004"), Invariant),
            (Some("LG006"), Invariant),
            // Out of shared memory: a batch beyond the lock table (ADR-0018).
            (Some("53200"), Invariant),
            // A lock timeout, a deadlock, a cancel, a terminate, a lost
            // connection, a missing privilege, or no SQLSTATE at all.
            (Some("55P03"), NotConfirmed),
            (Some("40P01"), NotConfirmed),
            (Some("57014"), NotConfirmed),
            (Some("57P01"), NotConfirmed),
            (Some("08006"), NotConfirmed),
            (Some("42501"), NotConfirmed),
            (None, NotConfirmed),
        ] {
            assert_eq!(classify(code), expected, "{code:?}");
        }
    }

    proptest! {
        #[test]
        fn codes_outside_those_rules_are_not_confirmed(code in "[0-9A-Z]{5}") {
            prop_assume!(
                !code.starts_with("23") && !code.starts_with("LG") && code != "53200"
            );
            prop_assert_eq!(classify(Some(&code)), Failure::NotConfirmed);
        }
    }

    #[test]
    fn a_payments_lock_key_is_the_low_half_of_its_id() {
        // Written out, and read as a big-endian i64 by hand: 0x8123456789abcdef
        // is 2^64 - 0x7edcba9876543211 (ADR-0018).
        let id = uuid!("019b76da-a800-7000-8123-456789abcdef");
        let id = PaymentId::try_from(id).expect("a UUIDv7");
        assert_eq!(lock_key(id), -0x7EDC_BA98_7654_3211);
    }

    proptest! {
        #[test]
        fn every_payments_lock_key_is_negative(bits in any::<u128>()) {
            // Any ID that converts has RFC 9562's variant bits, the top bits of
            // the low half, so other uses of advisory locks keep clear of these
            // keys by taking non-negative ones (ADR-0018).
            let version_7 = (bits & !(0xf << 76) & !(0b11 << 62)) | (0x7 << 76) | (0b10 << 62);
            let id = PaymentId::try_from(Uuid::from_u128(version_7)).expect("a UUIDv7");
            prop_assert!(lock_key(id) < 0);
        }
    }

    #[test]
    fn a_payments_accounts_open_in_sorted_key_order() {
        let currency = Currency::parse("EUR").expect("valid code");
        let command = CreatePayment {
            id: PaymentId::now_v7(),
            step: TransitionId::now_v7(),
            merchant: MerchantId::now_v7(),
            psp: Psp::Mock,
            currency,
            amount: PositiveAmount::new(1).expect("valid amount"),
            fee_rate: BasisPoints::new(0).expect("valid rate"),
            accounts: AccountCandidates {
                fee_revenue: AccountId::now_v7(),
                merchant_payable: AccountId::now_v7(),
                psp_receivable: AccountId::now_v7(),
            },
        };
        let keys = accounts_to_open(&command).map(|(reference, _)| reference.key());
        assert!(
            keys.windows(2)
                .all(|pair| pair[0].kind.as_str() < pair[1].kind.as_str()),
            "{keys:?}"
        );
    }
}
