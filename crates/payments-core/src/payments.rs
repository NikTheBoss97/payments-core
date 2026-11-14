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
//! A command runs on a plain pool for the system paths, or for a merchant through
//! a [`Tenant`](crate::tenancy::Tenant), whose transactions name it (ADR-0025).
//!
//! The lints are the same as in `money`: this module handles amounts.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::accounts::{AccountError, AccountKey, AccountRef, Psp, find_account, open_account};
use crate::catch_up::PspWord;
use crate::db::{CommitError, code_and_constraint, commit};
use crate::idempotency::Decided;
use crate::ids::{AccountId, MerchantId, PaymentId, TransactionId, TransitionId};
use crate::ledger::{Entry, LedgerTransaction};
use crate::lifecycle::{
    self, Event, IllegalTransition, PaymentStatus, Refused, StepKind, Transition, next,
};
use crate::money::{self, BasisPoints, Currency, PositiveAmount};
use crate::posting::{PostInError, post_in};
use crate::requests::{Conflict, Refusal, RequestKind};
use crate::tenancy::{Scope, Tenant};
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
    /// The merchant exists at another fee rate: not a retry of this registration
    /// (ADR-0019). Nothing changed.
    #[error("merchant {merchant} is registered at another fee rate")]
    Mismatch { merchant: MerchantId },
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

/// Registers a merchant at the fee rate that the operator set (ADR-0017,
/// ADR-0024). Calling it again with the same values is safe.
///
/// # Errors
///
/// [`RegisterError::Mismatch`] if the merchant exists at another rate,
/// [`RegisterError::Invariant`] for a bug, and [`RegisterError::NotConfirmed`]
/// for anything else.
pub async fn register_merchant(
    pool: &PgPool,
    id: MerchantId,
    fee_rate: BasisPoints,
) -> Result<(), RegisterError> {
    let failure = |source: sqlx::Error| match failure_of(&source) {
        Failure::Duplicate | Failure::Invariant => RegisterError::Invariant {
            merchant: id,
            source,
        },
        Failure::NotConfirmed => RegisterError::NotConfirmed {
            merchant: id,
            source,
        },
    };
    let fee_bps = i32::from(fee_rate.get());
    let inserted = sqlx::query!(
        "INSERT INTO merchants (id, fee_bps) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
        id.get(),
        fee_bps
    )
    .execute(pool)
    .await
    .map_err(failure)?;
    if inserted.rows_affected() == 1 {
        return Ok(());
    }
    // A merchant's row never changes, so this read cannot go stale.
    let stored = sqlx::query_scalar!("SELECT fee_bps FROM merchants WHERE id = $1", id.get())
        .fetch_one(pool)
        .await
        .map_err(failure)?;
    if stored == fee_bps {
        Ok(())
    } else {
        Err(RegisterError::Mismatch { merchant: id })
    }
}

/// Why a merchant's fee rate could not be read. Reading again is safe.
#[derive(Debug, thiserror::Error)]
#[error("the fee rate of merchant {merchant} could not be read")]
pub struct FeeRateError {
    merchant: MerchantId,
    #[source]
    source: sqlx::Error,
}

/// The fee rate that the operator set for this merchant, which a new payment copies
/// (ADR-0024), or `None` when no merchant has its ID.
///
/// # Errors
///
/// [`FeeRateError`] when the rate cannot be read.
pub async fn merchant_fee_rate(tenant: Tenant<'_>) -> Result<Option<BasisPoints>, FeeRateError> {
    let merchant = tenant.merchant();
    let failure = |source| FeeRateError { merchant, source };
    let mut tx = Scope::from(tenant).begin().await.map_err(failure)?;
    // The merchant is in the query as well as in the policy (ADR-0025).
    let fee_bps = sqlx::query_scalar!(
        "SELECT fee_bps FROM merchants WHERE id = $1",
        merchant.get()
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(failure)?;
    Ok(fee_bps.map(|fee_bps| {
        let fee_bps = u16::try_from(fee_bps).expect("merchants_fee_bps_check keeps it in range");
        BasisPoints::new(fee_bps).expect("merchants_fee_bps_check keeps it in range")
    }))
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
    /// Its authorization, once it has one.
    pub authorization: Option<Authorization>,
}

/// A payment's authorization: the amount authorized, which may be less than the amount
/// asked for, and when it lapses (ADR-0016, ADR-0033).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Authorization {
    pub amount: PositiveAmount,
    pub expires_at: BusinessTime,
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

/// Reads a payment, or `None` when no payment has this ID, or when another merchant
/// owns it and the scope is a merchant's (ADR-0025).
///
/// # Errors
///
/// [`ReadError::Database`] when the payment cannot be read.
pub async fn find_payment<'a>(
    scope: impl Into<Scope<'a>>,
    id: PaymentId,
) -> Result<Option<Payment>, ReadError> {
    let failure = |source| ReadError::Database {
        payment: id,
        source,
    };
    let mut tx = scope.into().begin().await.map_err(failure)?;
    // One statement, so the terms, the newest step and the authorization come from one
    // snapshot, and a status never shows without its authorization. A payment has at most
    // one authorization (ADR-0016).
    let row = sqlx::query!(
        r#"SELECT p.merchant_id, p.psp, p.currency AS "currency!", p.amount, p.fee_bps,
                  newest.to_status, newest.refundable,
                  a.amount AS "authorized?", a.expires_at AS "expires_at?"
             FROM payments p
            CROSS JOIN LATERAL (
                  SELECT t.to_status, t.refundable
                    FROM payment_transitions t
                   WHERE t.payment_id = p.id
                   ORDER BY t.seq DESC
                   LIMIT 1
                  ) newest
             LEFT JOIN payment_authorizations a ON a.payment_id = p.id
            WHERE p.id = $1"#,
        id.get()
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(failure)?;
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
            authorization: row.authorized.map(|amount| Authorization {
                amount: PositiveAmount::new(amount)
                    .expect("an authorization is at least 1 and at most the request"),
                expires_at: stored_time(
                    row.expires_at
                        .expect("payment_authorizations.expires_at is NOT NULL"),
                ),
            }),
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
    /// The idempotency key already holds an answer, so this attempt rolled back
    /// whole. Look the key up for the answer (ADR-0026).
    #[error("the idempotency key already holds an answer")]
    KeyTaken,
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
pub async fn create_payment<'a>(
    scope: impl Into<Scope<'a>>,
    command: &CreatePayment,
) -> Result<Outcome, CreateError> {
    let scope = scope.into();
    let payment = command.id;
    let fail = |source| create_failure(payment, source);
    // Merchants are append-only, so this answer cannot go stale.
    let mut tx = scope.begin().await.map_err(fail)?;
    let registered = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM merchants WHERE id = $1) AS "registered!""#,
        command.merchant.get()
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(fail)?;
    drop(tx);
    if !registered {
        return Err(CreateError::UnknownMerchant {
            merchant: command.merchant,
        });
    }
    for (reference, candidate) in accounts_to_open(command) {
        open_account(
            scope.pool(),
            candidate,
            &reference.key(),
            reference.normal_side(),
        )
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
    let mut tx = scope.begin().await.map_err(fail)?;
    if let Err(source) = insert_payment(&mut tx, command).await {
        // The database transaction is aborted. Dropping it queues the ROLLBACK;
        // committing it would report success (ADR-0014).
        drop(tx);
        return match failure_of(&source) {
            Failure::Duplicate => stored_create(scope, command, source).await,
            Failure::Invariant => Err(CreateError::Invariant { payment, source }),
            Failure::NotConfirmed => Err(CreateError::NotConfirmed { payment, source }),
        };
    }
    let first = Transition::genesis();
    let recorded = Recorded {
        seq: 0,
        from: first.from(),
        to: first.to(),
        refundable: 0,
    };
    if let Err(failure) = store_sealed(&mut tx, scope, Decided::Applied(&recorded)).await {
        return Err(match failure {
            SealFailure::KeyTaken => CreateError::KeyTaken,
            SealFailure::Invariant(source) => CreateError::Invariant { payment, source },
            SealFailure::NotConfirmed(source) => CreateError::NotConfirmed { payment, source },
        });
    }
    match commit(tx).await {
        Ok(()) => Ok(Outcome::Applied(recorded)),
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
    scope: Scope<'_>,
    command: &CreatePayment,
    duplicate: sqlx::Error,
) -> Result<Outcome, CreateError> {
    let payment = command.id;
    let mut tx = scope
        .begin()
        .await
        .map_err(|source| create_failure(payment, source))?;
    let row = sqlx::query!(
        r#"SELECT s.payment_id, s.seq, s.kind, s.from_status, s.to_status, s.refundable,
                  p.merchant_id, p.psp, p.currency AS "currency!", p.amount, p.fee_bps
             FROM payment_transitions s
             JOIN payments p ON p.id = s.payment_id
            WHERE s.id = $1"#,
        command.step.get()
    )
    .fetch_optional(&mut *tx)
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

/// The command that records an authorization (ADR-0016), which only the catch-up
/// gives, from a fact of the PSP's (ADR-0048).
#[derive(Debug)]
pub(crate) struct Authorize {
    /// The step's ID, which identifies this command.
    pub step: TransitionId,
    pub payment: PaymentId,
    /// The authorized amount, at most the requested one.
    pub amount: PositiveAmount,
    /// The deadline that the PSP reported, never enforced by a clock here
    /// (ADR-0016).
    pub expires_at: BusinessTime,
}

/// An [`Authorize`]'s steps 3 to 6, in a transaction that the caller has begun and
/// locked: the catch-up applies the PSP's facts this way (ADR-0039), and only it
/// makes the [`PspWord`] that this takes (ADR-0048).
pub(crate) async fn authorize_in(
    tx: &mut Transaction<'_, Postgres>,
    command: &Authorize,
    _: &PspWord,
) -> Result<Outcome, Failed<Refused>> {
    apply_in(tx, &authorize_plan(command), |state| {
        authorize_move(state, command)
    })
    .await
}

/// An [`Authorize`] as its unit of work sees it.
fn authorize_plan(command: &Authorize) -> Plan<'_> {
    Plan {
        id: &command.step,
        asked: Asked {
            payment: command.payment,
            kind: StepKind::Authorize,
            amount: command.amount.get(),
            detail: Detail::Authorization {
                expires_at: command.expires_at,
            },
        },
        posting: None,
    }
}

/// The move of an [`Authorize`]: at most the requested amount (ADR-0016).
fn authorize_move(state: &State, command: &Authorize) -> Result<Transition, Refused> {
    lifecycle::authorize(state.status, command.amount, state.requested)
}

/// The command that records a capture, and posts it (ADR-0016, ADR-0020), which only
/// the catch-up gives, from a fact of the PSP's (ADR-0048).
#[derive(Debug)]
pub(crate) struct Capture {
    /// The step's ID, which identifies this command.
    pub step: TransitionId,
    /// The ID of the ledger transaction that this capture posts.
    pub posting: TransactionId,
    pub payment: PaymentId,
    /// The captured amount, at most the authorized one. The PSP releases the
    /// rest of the hold.
    pub amount: PositiveAmount,
    /// When the capture took effect, as the PSP reported it (ADR-0022).
    pub captured_at: BusinessTime,
}

/// A [`Capture`]'s steps 3 to 6, in a transaction that the caller has begun and locked,
/// with the payment's [`Books`] read before the lock (ADR-0039): the step, the posting
/// and the record commit together. Only the catch-up makes the [`PspWord`] that this
/// takes (ADR-0048).
pub(crate) async fn capture_in(
    tx: &mut Transaction<'_, Postgres>,
    books: &Books,
    command: &Capture,
    _: &PspWord,
) -> Result<Outcome, Failed<PostingRefusal>> {
    let (posting, asked) = capture_parts(books, command);
    let plan = Plan {
        id: &command.step,
        asked,
        posting: Some(&posting),
    };
    apply_in(tx, &plan, |state| capture_move(state, command)).await
}

/// What a payment's postings need, read outside its transaction, and so before its
/// lock: its terms never change, and its accounts exist before it does (ADR-0019,
/// ADR-0020).
pub(crate) struct Books {
    currency: Currency,
    fee_rate: BasisPoints,
    receivable: AccountId,
    payable: AccountId,
    fees: AccountId,
}

/// Reads a payment's [`Books`].
pub(crate) async fn read_books(
    scope: Scope<'_>,
    payment: PaymentId,
) -> Result<Books, Failed<PostingRefusal>> {
    let pool = scope.pool();
    let terms = read_terms(scope, payment).await?;
    let receivable =
        posting_account(pool, AccountRef::PspReceivable(terms.psp, terms.currency)).await?;
    let payable = posting_account(
        pool,
        AccountRef::MerchantPayable(terms.merchant, terms.currency),
    )
    .await?;
    let fees = posting_account(pool, AccountRef::FeeRevenue(terms.currency)).await?;
    Ok(Books {
        currency: terms.currency,
        fee_rate: terms.fee_rate,
        receivable,
        payable,
        fees,
    })
}

/// A [`Capture`]'s posting, and what a retry of it compares.
fn capture_parts(books: &Books, command: &Capture) -> (LedgerTransaction, Asked) {
    let fee = money::fee(command.amount, books.fee_rate);
    let posting = capture_posting(
        same_posting_id(&command.posting),
        books.currency,
        CaptureAccounts {
            receivable: books.receivable,
            payable: books.payable,
            fees: books.fees,
        },
        command.amount,
        fee,
    );
    let asked = Asked {
        payment: command.payment,
        kind: StepKind::Capture,
        amount: command.amount.get(),
        detail: Detail::Capture {
            fee,
            captured_at: command.captured_at,
            posting: command.posting.get(),
        },
    };
    (posting, asked)
}

/// The move of a [`Capture`]: at most the authorized amount (ADR-0016).
fn capture_move(state: &State, command: &Capture) -> Result<Transition, PostingRefusal> {
    // Before an authorization the status refuses the capture, so this limit is never
    // read.
    let limit = state.authorized.unwrap_or(state.requested);
    lifecycle::capture(state.status, command.amount, limit).map_err(|_| PostingRefusal::Refused)
}

/// The accounts of a capture's posting.
struct CaptureAccounts {
    receivable: AccountId,
    payable: AccountId,
    fees: AccountId,
}

/// ADR-0020's lines for a capture of C with a fee of F: the PSP's receivable
/// +C, the merchant's payable -(C - F) and the platform's fees -F. A line whose
/// amount would be zero is left out.
fn capture_posting(
    id: TransactionId,
    currency: Currency,
    accounts: CaptureAccounts,
    captured: PositiveAmount,
    fee: i64,
) -> LedgerTransaction {
    let merchant_share = captured
        .get()
        .checked_sub(fee)
        .expect("a fee is at most the captured amount");
    let entries = [
        Some(Entry {
            account: accounts.receivable,
            amount: captured.debit(),
        }),
        credit_line(accounts.payable, merchant_share),
        credit_line(accounts.fees, fee),
    ]
    .into_iter()
    .flatten()
    .collect();
    LedgerTransaction::new(id, currency, entries)
        .expect("the credits share out the debit, so the lines balance, and there are two or more")
}

/// A credit of `minor`, or no line at all when it is zero: a ledger entry is
/// never zero (ADR-0020).
fn credit_line(account: AccountId, minor: i64) -> Option<Entry> {
    (minor != 0).then(|| Entry {
        account,
        amount: PositiveAmount::new(minor)
            .expect("a share of a captured amount is between 1 and the cap")
            .credit(),
    })
}

/// The command that records a refund, and posts it (ADR-0016, ADR-0020), which only
/// the catch-up gives, from a fact of the PSP's (ADR-0048).
#[derive(Debug)]
pub(crate) struct Refund {
    /// The step's ID, which identifies this command.
    pub step: TransitionId,
    /// The ID of the ledger transaction that this refund posts.
    pub posting: TransactionId,
    pub payment: PaymentId,
    /// The refunded amount. Below what can still be refunded it is partial, at
    /// that amount it is final, and above it the command is refused.
    pub amount: PositiveAmount,
    /// When the refund took effect, as the PSP reported it (ADR-0022).
    pub refunded_at: BusinessTime,
}

/// A [`Refund`]'s steps 3 to 6, in a transaction that the caller has begun and locked,
/// with the payment's [`Books`] read before the lock (ADR-0039): the merchant pays,
/// the platform keeps its fee (ADR-0020), and the step, the posting and the record
/// commit together. Only the catch-up makes the [`PspWord`] that this takes
/// (ADR-0048).
pub(crate) async fn refund_in(
    tx: &mut Transaction<'_, Postgres>,
    books: &Books,
    command: &Refund,
    _: &PspWord,
) -> Result<Outcome, Failed<PostingRefusal>> {
    let (posting, asked) = refund_parts(books.currency, books.payable, books.receivable, command);
    let plan = Plan {
        id: &command.step,
        asked,
        posting: Some(&posting),
    };
    apply_in(tx, &plan, |state| refund_move(state, command)).await
}

/// A [`Refund`]'s posting, and what a retry of it compares.
fn refund_parts(
    currency: Currency,
    payable: AccountId,
    receivable: AccountId,
    command: &Refund,
) -> (LedgerTransaction, Asked) {
    let posting = refund_posting(
        same_posting_id(&command.posting),
        currency,
        payable,
        receivable,
        command.amount,
    );
    let asked = Asked {
        payment: command.payment,
        kind: StepKind::Refund,
        amount: command.amount.get(),
        detail: Detail::Refund {
            refunded_at: command.refunded_at,
            posting: command.posting.get(),
        },
    };
    (posting, asked)
}

/// The move of a [`Refund`]: partial or final, within what can still be refunded
/// (ADR-0016).
fn refund_move(state: &State, command: &Refund) -> Result<Transition, PostingRefusal> {
    lifecycle::refund(state.status, command.amount, state.refundable)
        .map_err(|_| PostingRefusal::Refused)
}

/// ADR-0020's lines for a refund of R: the merchant's payable +R and the PSP's
/// receivable -R. They depend on R alone, because the platform keeps its fee.
fn refund_posting(
    id: TransactionId,
    currency: Currency,
    payable: AccountId,
    receivable: AccountId,
    refunded: PositiveAmount,
) -> LedgerTransaction {
    let entries = vec![
        Entry {
            account: payable,
            amount: refunded.debit(),
        },
        Entry {
            account: receivable,
            amount: refunded.credit(),
        },
    ];
    LedgerTransaction::new(id, currency, entries)
        .expect("a debit and a credit of one amount balance")
}

/// A second value of a command's posting ID. A retry must post under the same
/// one, and a command is only borrowed, so this is written out (ADR-0005).
fn same_posting_id(id: &TransactionId) -> TransactionId {
    TransactionId::try_from(id.get()).expect("a transaction ID stays valid")
}

/// What stops a command that posts, besides a [`Failed`] that any command can
/// meet.
pub(crate) enum PostingRefusal {
    /// The table of moves or the amount's limit, which stops the payment in a
    /// catch-up whatever the reason (ADR-0041).
    Refused,
    MissingAccount(AccountKey),
}

/// A payment's terms, or why they could not be read.
async fn read_terms<R>(scope: Scope<'_>, id: PaymentId) -> Result<Payment, Failed<R>> {
    match find_payment(scope, id).await {
        Ok(Some(payment)) => Ok(payment),
        Ok(None) => Err(Failed::NotFound),
        Err(ReadError::Database { source, .. }) => Err(Failed::database(source)),
    }
}

/// The ID of an account of a posting. It only looks the account up: creating a
/// payment opened it (ADR-0020), and a posting never creates accounts (ADR-0005).
async fn posting_account(
    pool: &PgPool,
    reference: AccountRef,
) -> Result<AccountId, Failed<PostingRefusal>> {
    let key = reference.key();
    match find_account(pool, &key).await {
        Ok(Some(account)) => Ok(account.id),
        Ok(None) => Err(Failed::Refused(PostingRefusal::MissingAccount(key))),
        Err(AccountError::Database { source, .. }) => Err(Failed::database(source)),
        Err(other) => unreachable!("finding an account fails only as Database: {other}"),
    }
}

/// A command that moves a payment and carries nothing else: the merchant's cancel,
/// and in a catch-up a decline, a void, a lapse or a settlement.
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
    /// Only a cancellation: an authorization of the payment was requested, so the
    /// PSP may hold one, which only a void releases (ADR-0039). Nothing is stored,
    /// and a retry is decided again (ADR-0019).
    #[error("an authorization of this payment was requested already")]
    AuthorizationRequested,
    /// The command's step ID is stored with other content: a bug, never
    /// retried.
    #[error("step {step} is stored with other content")]
    Mismatch { step: Uuid },
    /// The idempotency key already holds an answer, so this attempt rolled back
    /// whole. Look the key up for the answer (ADR-0026).
    #[error("the idempotency key already holds an answer")]
    KeyTaken,
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
    fn new(command: &StepCommand, failed: Failed<PlainRefusal>) -> Self {
        let step = command.step.get();
        match failed {
            Failed::NotFound => Self::NotFound {
                payment: command.payment,
            },
            Failed::Refused(PlainRefusal::Illegal(illegal)) => Self::Illegal(illegal),
            Failed::Refused(PlainRefusal::AuthorizationRequested) => Self::AuthorizationRequested,
            Failed::Mismatch => Self::Mismatch { step },
            Failed::KeyTaken => Self::KeyTaken,
            Failed::Duplicate(source) | Failed::Invariant(source) => {
                Self::Invariant { step, source }
            }
            Failed::NotConfirmed(source) => Self::NotConfirmed { step, source },
        }
    }
}

/// Cancels a payment before its authorization (ADR-0016), and before any request
/// to authorize it, since the PSP may then hold one (ADR-0039).
///
/// # Errors
///
/// See [`StepError`].
pub async fn cancel<'a>(
    scope: impl Into<Scope<'a>>,
    command: &StepCommand,
) -> Result<Outcome, StepError> {
    apply(
        scope.into(),
        &plain_plan(command, StepKind::Cancel),
        |state| plain_move(state, Event::Cancel),
    )
    .await
    .map_err(|failed| StepError::new(command, failed))
}

/// A step of the PSP's that carries nothing, in a transaction that the caller has
/// begun and locked (ADR-0039): a decline, a void, a lapse or a settlement. Only the
/// catch-up makes the [`PspWord`] that this takes (ADR-0048).
pub(crate) async fn plain_in(
    tx: &mut Transaction<'_, Postgres>,
    command: &StepCommand,
    event: Event,
    kind: StepKind,
    _: &PspWord,
) -> Result<Outcome, Failed<PlainRefusal>> {
    apply_in(tx, &plain_plan(command, kind), |state| {
        plain_move(state, event)
    })
    .await
}

/// A step that carries nothing, as its unit of work sees it.
fn plain_plan(command: &StepCommand, kind: StepKind) -> Plan<'_> {
    Plan {
        id: &command.step,
        asked: Asked {
            payment: command.payment,
            kind,
            amount: 0,
            detail: Detail::None,
        },
        posting: None,
    }
}

/// Why a step that carries nothing is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlainRefusal {
    /// The table of moves.
    Illegal(IllegalTransition),
    /// A request to authorize the payment, which only a cancellation meets.
    AuthorizationRequested,
}

impl From<IllegalTransition> for PlainRefusal {
    fn from(illegal: IllegalTransition) -> Self {
        Self::Illegal(illegal)
    }
}

/// The move of a step that carries nothing: the table of moves first, as for every
/// step. A cancellation is a step of our own, which the PSP never hears of, so once an
/// authorization is requested it is refused: the PSP may hold one, which only a void
/// releases (ADR-0039).
fn plain_move(state: &State, event: Event) -> Result<Transition, PlainRefusal> {
    let step = next(state.status, event)?;
    if event == Event::Cancel && state.authorization_requested {
        return Err(PlainRefusal::AuthorizationRequested);
    }
    Ok(step)
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
    Capture {
        fee: i64,
        captured_at: BusinessTime,
        /// The link to the ledger (ADR-0011).
        posting: Uuid,
    },
    Refund {
        refunded_at: BusinessTime,
        /// The link to the ledger (ADR-0011).
        posting: Uuid,
    },
}

/// A command as its unit of work sees it.
pub(crate) struct Plan<'a> {
    id: &'a TransitionId,
    asked: Asked,
    /// What the step posts, written after it and before its record.
    posting: Option<&'a LedgerTransaction>,
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
    /// The payment's authorized amount, once it has one.
    authorized: Option<PositiveAmount>,
    /// Whether the payment has a request to authorize it, decided or not (ADR-0039).
    authorization_requested: bool,
    /// The step with the command's ID, on any payment.
    stored: Option<Stored>,
}

/// A command's failure, before its public function states it in its own enum.
/// `R` is what that command's rules can refuse.
pub(crate) enum Failed<R> {
    NotFound,
    Refused(R),
    Mismatch,
    /// The idempotency key holds an answer already (ADR-0026).
    KeyTaken,
    /// Some ID already exists, and the command has yet to look its step up.
    /// Reaching the caller, it is a duplicate that nothing explains: a bug.
    Duplicate(sqlx::Error),
    Invariant(sqlx::Error),
    NotConfirmed(sqlx::Error),
}

impl<R> Failed<R> {
    pub(crate) fn database(source: sqlx::Error) -> Self {
        match failure_of(&source) {
            Failure::Duplicate => Self::Duplicate(source),
            Failure::Invariant => Self::Invariant(source),
            Failure::NotConfirmed => Self::NotConfirmed(source),
        }
    }
}

/// A command's unit of work, from `BEGIN` to `COMMIT` (ADR-0019).
async fn apply<R: FinalRefusal>(
    scope: Scope<'_>,
    plan: &Plan<'_>,
    decide: impl FnOnce(&State) -> Result<Transition, R>,
) -> Result<Outcome, Failed<R>> {
    let mut tx = begin_locked(scope, plan.asked.payment).await?;
    match apply_in(&mut tx, plan, decide).await {
        Ok(Outcome::Applied(recorded)) => {
            store_sealed(&mut tx, scope, Decided::Applied(&recorded)).await?;
            commit_unit(tx).await?;
            Ok(Outcome::Applied(recorded))
        }
        // A retry wrote nothing, so dropping the transaction ends it.
        Ok(replayed) => Ok(replayed),
        // A rejection wrote nothing. Under a key a final one is stored, so that a late
        // duplicate cannot turn it into an effect (ADR-0026).
        Err(Failed::Refused(refusal)) => {
            if scope.seal().is_some()
                && let Some(refused) = refusal.final_refusal()
            {
                store_sealed(&mut tx, scope, Decided::Refused(refused)).await?;
                commit_unit(tx).await?;
            }
            Err(Failed::Refused(refusal))
        }
        Err(Failed::Duplicate(duplicate)) => {
            // The failure aborted the transaction, which released the lock at
            // once (ADR-0018), and the lookup takes it again. Dropping the
            // transaction first gives its connection back: a command that asks
            // the pool for a connection while it holds one can wait for itself.
            drop(tx);
            look_up_again(scope, plan, duplicate).await
        }
        // Any other failure aborted the transaction: drop it, because committing it
        // would report success (ADR-0014).
        Err(failed) => Err(failed),
    }
}

/// Commits a command's unit of work, through the one function of ADR-0014.
pub(crate) async fn commit_unit<R>(tx: Transaction<'_, Postgres>) -> Result<(), Failed<R>> {
    commit(tx).await.map_err(|err| match err {
        CommitError::Rejected(source) => Failed::Invariant(source),
        CommitError::Unknown(source) => Failed::NotConfirmed(source),
    })
}

/// Why a sealed command's answer could not be stored. The transaction is aborted:
/// drop it, which takes the effect with it.
pub(crate) enum SealFailure {
    /// The key has an answer: another attempt committed first (ADR-0026).
    KeyTaken,
    Invariant(sqlx::Error),
    NotConfirmed(sqlx::Error),
}

impl<R> From<SealFailure> for Failed<R> {
    fn from(failure: SealFailure) -> Self {
        match failure {
            SealFailure::KeyTaken => Self::KeyTaken,
            SealFailure::Invariant(source) => Self::Invariant(source),
            SealFailure::NotConfirmed(source) => Self::NotConfirmed(source),
        }
    }
}

/// Stores a sealed command's final answer in its unit of work, and does nothing for
/// a command without a seal. A duplicate here can only be the key, whichever attempt
/// got there first, because this is its own statement (ADR-0026).
pub(crate) async fn store_sealed(
    tx: &mut Transaction<'_, Postgres>,
    scope: Scope<'_>,
    decided: Decided<'_>,
) -> Result<(), SealFailure> {
    let Some((merchant, seal)) = scope.seal() else {
        return Ok(());
    };
    seal.store_in(tx, merchant, decided)
        .await
        .map_err(|source| match failure_of(&source) {
            Failure::Duplicate => SealFailure::KeyTaken,
            Failure::Invariant => SealFailure::Invariant(source),
            Failure::NotConfirmed => SealFailure::NotConfirmed(source),
        })
}

/// What of a command's refusal is final, and so stored under its key: what the table
/// of moves says, or a request already recorded.
trait FinalRefusal {
    fn final_refusal(&self) -> Option<Refusal>;
}

impl FinalRefusal for PlainRefusal {
    fn final_refusal(&self) -> Option<Refusal> {
        Some(match self {
            Self::Illegal(illegal) => (*illegal).into(),
            Self::AuthorizationRequested => Conflict::AuthorizationRequested.into(),
        })
    }
}

/// Begins a database transaction whose first statement takes the payment's lock
/// (ADR-0018). A session-level lock would outlive the transaction, so this is
/// the only lock function that the library calls. `lock_timeout` bounds the
/// wait, on the server. Its second statement names the merchant (ADR-0025).
pub(crate) async fn begin_locked<R>(
    scope: Scope<'_>,
    payment: PaymentId,
) -> Result<Transaction<'static, Postgres>, Failed<R>> {
    let mut tx = scope.pool().begin().await.map_err(Failed::database)?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", lock_key(payment))
        .execute(&mut *tx)
        .await
        .map_err(Failed::database)?;
    scope
        .name_merchant(&mut tx)
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
    let refundable = step.refundable_after(plan.asked.amount, state.refundable);
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
            refundable,
        },
    )
    .await
    .map_err(Failed::database)?;
    if let Some(posting) = plan.posting {
        post_in(tx, posting).await.map_err(posting_failure)?;
    }
    insert_record(tx, plan, &state)
        .await
        .map_err(Failed::database)?;
    Ok(Outcome::Applied(Recorded {
        seq,
        from: step.from(),
        to: step.to(),
        refundable,
    }))
}

/// What a failed posting means to a command: `post_in` reads it as the ledger
/// sees it. An ID that exists is a duplicate key like any other, and an account
/// outside the currency is a bug here, because the command found it by a key in
/// that currency.
fn posting_failure<R>(err: PostInError) -> Failed<R> {
    match err {
        PostInError::Exists { source, .. } => Failed::Duplicate(source),
        PostInError::AccountNotInCurrency { source, .. }
        | PostInError::Invariant { source, .. } => Failed::Invariant(source),
        PostInError::Database { source, .. } => Failed::database(source),
    }
}

/// A write met a duplicate key, so some ID exists. Under the lock again, the
/// step with the command's ID decides, and none means that another writer broke
/// a rule (ADR-0019).
async fn look_up_again<R>(
    scope: Scope<'_>,
    plan: &Plan<'_>,
    duplicate: sqlx::Error,
) -> Result<Outcome, Failed<R>> {
    let mut tx = begin_locked(scope, plan.asked.payment).await?;
    match read_state(&mut tx, plan).await?.stored {
        Some(stored) => answer(stored, plan).map(Outcome::AlreadyApplied),
        None => Err(Failed::Invariant(duplicate)),
    }
}

/// The stored reply when the stored step is this command's: the same step and the
/// same record (ADR-0019). Only a cancel is retried here, which posts nothing: the
/// commands that post are the catch-up's, whose step IDs are new (ADR-0039), so no
/// posting is compared (ADR-0048).
fn answer<R>(stored: Stored, plan: &Plan<'_>) -> Result<Recorded, Failed<R>> {
    if stored.asked != plan.asked {
        return Err(Failed::Mismatch);
    }
    Ok(stored.recorded)
}

/// The one read under the lock (ADR-0019). It must come in a later statement
/// than the lock: at READ COMMITTED each statement takes a new snapshot, so this
/// one sees every step and request that the lock's previous holder committed
/// (ADR-0018).
async fn read_state<R>(
    tx: &mut Transaction<'_, Postgres>,
    plan: &Plan<'_>,
) -> Result<State, Failed<R>> {
    let row = sqlx::query!(
        r#"SELECT newest.seq, newest.to_status, newest.refundable,
                  p.amount AS requested,
                  authorized.amount AS "authorized?",
                  EXISTS (SELECT 1 FROM payment_requests r
                           WHERE r.payment_id = p.id AND r.kind = $3)
                      AS "authorization_requested!",
                  stored.payment_id AS "stored_payment?",
                  stored.seq AS "stored_seq?",
                  stored.kind AS "stored_kind?",
                  stored.from_status AS "stored_from?",
                  stored.to_status AS "stored_to?",
                  stored.amount AS "stored_amount?",
                  stored.refundable AS "stored_refundable?",
                  stored_authorization.expires_at AS "expires_at?",
                  stored_capture.fee AS "fee?",
                  stored_capture.captured_at AS "captured_at?",
                  stored_capture.ledger_transaction_id AS "capture_posting?",
                  stored_refund.refunded_at AS "refunded_at?",
                  stored_refund.ledger_transaction_id AS "refund_posting?"
             FROM payments p
            CROSS JOIN LATERAL (
                  SELECT t.seq, t.to_status, t.refundable
                    FROM payment_transitions t
                   WHERE t.payment_id = p.id
                   ORDER BY t.seq DESC
                   LIMIT 1
                  ) newest
             LEFT JOIN payment_authorizations authorized ON authorized.payment_id = p.id
             LEFT JOIN payment_transitions stored ON stored.id = $2
             LEFT JOIN payment_authorizations stored_authorization
                    ON stored_authorization.transition_id = stored.id
             LEFT JOIN payment_captures stored_capture
                    ON stored_capture.transition_id = stored.id
             LEFT JOIN payment_refunds stored_refund
                    ON stored_refund.transition_id = stored.id
            WHERE p.id = $1"#,
        plan.asked.payment.get(),
        plan.id.get(),
        RequestKind::Authorize.as_str()
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
                    detail: match kind {
                        StepKind::Authorize => Detail::Authorization {
                            expires_at: stored_time(row.expires_at.expect(HAS_RECORD)),
                        },
                        StepKind::Capture => Detail::Capture {
                            fee: row.fee.expect(HAS_RECORD),
                            captured_at: stored_time(row.captured_at.expect(HAS_RECORD)),
                            posting: row.capture_posting.expect(HAS_RECORD),
                        },
                        StepKind::Refund => Detail::Refund {
                            refunded_at: stored_time(row.refunded_at.expect(HAS_RECORD)),
                            posting: row.refund_posting.expect(HAS_RECORD),
                        },
                        StepKind::Create
                        | StepKind::Decline
                        | StepKind::Cancel
                        | StepKind::Void
                        | StepKind::Expire
                        | StepKind::Settle => Detail::None,
                    },
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
        authorized: row.authorized.map(|amount| {
            PositiveAmount::new(amount)
                .expect("an authorization is at least 1 and at most the request")
        }),
        authorization_requested: row.authorization_requested,
        stored,
    })
}

/// Why a stored step's kind tells which record it has: a committed step of a
/// kind that carries data has its record (LG004).
const HAS_RECORD: &str = "LG004 keeps the record of a step that carries data";

pub(crate) fn stored_time(time: DateTime<Utc>) -> BusinessTime {
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
        Detail::Capture {
            fee,
            captured_at,
            posting,
        } => {
            let authorized = state
                .authorized
                .expect("the status allows a capture only after an authorization");
            sqlx::query!(
                "INSERT INTO payment_captures \
                     (transition_id, payment_id, amount, authorized_amount, fee, captured_at, \
                      ledger_transaction_id) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                plan.id.get(),
                plan.asked.payment.get(),
                plan.asked.amount,
                authorized.get(),
                fee,
                captured_at.get(),
                posting
            )
            .execute(&mut **tx)
            .await?;
            Ok(())
        }
        Detail::Refund {
            refunded_at,
            posting,
        } => {
            sqlx::query!(
                "INSERT INTO payment_refunds \
                     (transition_id, payment_id, amount, refunded_at, ledger_transaction_id) \
                 VALUES ($1, $2, $3, $4, $5)",
                plan.id.get(),
                plan.asked.payment.get(),
                plan.asked.amount,
                refunded_at.get(),
                posting
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
pub(crate) enum Failure {
    /// Some ID already exists, and the step with the command's ID decides.
    Duplicate,
    /// The database caught what the domain types should have prevented, refused
    /// the statement (ADR-0025), or ran out of shared memory for locks (ADR-0018):
    /// a bug, never retried.
    Invariant,
    /// The command may or may not have been applied.
    NotConfirmed,
}

/// The one classifier (ADR-0019), a pure function of the SQLSTATE: no rule needs
/// the constraint's name, because whichever unique key PostgreSQL reports, the
/// command looks its step up. A `COMMIT` is read by `db::commit` instead.
pub(crate) fn classify(code: Option<&str>) -> Failure {
    match code {
        Some("23505") => Failure::Duplicate,
        Some("53200") => Failure::Invariant,
        Some(code)
            if code.starts_with("23") || code.starts_with("42") || code.starts_with("LG") =>
        {
            Failure::Invariant
        }
        _ => Failure::NotConfirmed,
    }
}

pub(crate) fn failure_of(err: &sqlx::Error) -> Failure {
    let (code, _) = code_and_constraint(err);
    classify(code.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use uuid::uuid;

    #[test]
    fn only_a_cancellation_meets_a_requested_authorization() {
        let amount = |minor| PositiveAmount::new(minor).expect("a positive amount");
        for &status in PaymentStatus::ALL {
            for event in [
                Event::Decline,
                Event::Cancel,
                Event::Void,
                Event::Expire,
                Event::Settle,
            ] {
                let state = |authorization_requested| State {
                    seq: 1,
                    status,
                    refundable: 0,
                    requested: amount(100),
                    authorized: None,
                    authorization_requested,
                    stored: None,
                };
                let table = next(status, event).map_err(PlainRefusal::from);
                assert_eq!(
                    plain_move(&state(false), event),
                    table,
                    "{status} {event:?}"
                );
                // A decline in particular is the PSP's answer to that request.
                let expected = match table {
                    Ok(_) if event == Event::Cancel => Err(PlainRefusal::AuthorizationRequested),
                    other => other,
                };
                assert_eq!(
                    plain_move(&state(true), event),
                    expected,
                    "{status} {event:?}, once an authorization is requested"
                );
            }
        }
    }

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
            // A refused statement is permanent: a missing privilege or a row-level
            // security policy (ADR-0025), a table or a column that is not there.
            (Some("42501"), Invariant),
            (Some("42P01"), Invariant),
            (Some("42703"), Invariant),
            // A lock timeout, a deadlock, a cancel, a terminate, a lost
            // connection, or no SQLSTATE at all.
            (Some("55P03"), NotConfirmed),
            (Some("40P01"), NotConfirmed),
            (Some("57014"), NotConfirmed),
            (Some("57P01"), NotConfirmed),
            (Some("08006"), NotConfirmed),
            (None, NotConfirmed),
        ] {
            assert_eq!(classify(code), expected, "{code:?}");
        }
    }

    proptest! {
        #[test]
        fn codes_outside_those_rules_are_not_confirmed(code in "[0-9A-Z]{5}") {
            prop_assume!(
                !code.starts_with("23")
                    && !code.starts_with("42")
                    && !code.starts_with("LG")
                    && code != "53200"
            );
            prop_assert_eq!(classify(Some(&code)), Failure::NotConfirmed);
        }
    }

    fn capture_accounts() -> CaptureAccounts {
        CaptureAccounts {
            receivable: AccountId::now_v7(),
            payable: AccountId::now_v7(),
            fees: AccountId::now_v7(),
        }
    }

    /// The lines of a capture's posting, as (which account, amount).
    fn capture_lines(captured: i64, fee: i64) -> Vec<(&'static str, i64)> {
        let accounts = capture_accounts();
        let (receivable, payable, fees) = (accounts.receivable, accounts.payable, accounts.fees);
        let posting = capture_posting(
            TransactionId::now_v7(),
            Currency::parse("EUR").expect("valid code"),
            accounts,
            PositiveAmount::new(captured).expect("valid amount"),
            fee,
        );
        posting
            .entries()
            .iter()
            .map(|entry| {
                let name = if entry.account == receivable {
                    "receivable"
                } else if entry.account == payable {
                    "payable"
                } else {
                    assert_eq!(entry.account, fees);
                    "fees"
                };
                (name, entry.amount.get())
            })
            .collect()
    }

    #[test]
    fn a_captures_posting_has_the_lines_of_adr_0020() {
        assert_eq!(
            capture_lines(8_000, 200),
            [("receivable", 8_000), ("payable", -7_800), ("fees", -200)]
        );
        // A fee of 0, or of the whole capture, leaves its line out.
        assert_eq!(
            capture_lines(8_000, 0),
            [("receivable", 8_000), ("payable", -8_000)]
        );
        assert_eq!(
            capture_lines(8_000, 8_000),
            [("receivable", 8_000), ("fees", -8_000)]
        );
    }

    #[test]
    fn a_refunds_posting_has_the_lines_of_adr_0020() {
        let (payable, receivable) = (AccountId::now_v7(), AccountId::now_v7());
        let posting = refund_posting(
            TransactionId::now_v7(),
            Currency::parse("EUR").expect("valid code"),
            payable,
            receivable,
            PositiveAmount::new(3_000).expect("valid amount"),
        );
        let lines: Vec<_> = posting
            .entries()
            .iter()
            .map(|entry| (entry.account, entry.amount.get()))
            .collect();
        assert_eq!(lines, [(payable, 3_000), (receivable, -3_000)]);
    }

    proptest! {
        #[test]
        fn a_captures_posting_can_always_be_built(
            captured in 1_i64..=money::MAX_ABS,
            rate in 0_u16..=10_000,
        ) {
            // LedgerTransaction::new checks the balance, and fee() the share, so
            // building one is the property.
            let amount = PositiveAmount::new(captured).expect("valid amount");
            let rate = BasisPoints::new(rate).expect("valid rate");
            let lines = capture_lines(captured, money::fee(amount, rate));
            prop_assert!((2..=3).contains(&lines.len()));
            prop_assert_eq!(lines[0], ("receivable", captured));
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
