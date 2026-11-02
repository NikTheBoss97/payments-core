// Author: Nicola Antonello

//! Merchants, and the commands that change a payment (ADR-0017, ADR-0019).
//!
//! A command is plain data that carries every ID, and a retry is the identical
//! value sent again: it gets the stored reply, whatever has happened to the
//! payment since. No error converts from `sqlx::Error` with `#[from]`, so no
//! `?` can skip the classifier, which says what each database error proves.
//!
//! The lints are the same as in `money`: this module handles amounts.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::accounts::{AccountError, AccountRef, Psp, open_account};
use crate::db::{CommitError, code_and_constraint, commit};
use crate::ids::{AccountId, MerchantId, PaymentId, TransitionId};
use crate::lifecycle::{PaymentStatus, StepKind, Transition};
use crate::money::{BasisPoints, Currency, PositiveAmount};

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
/// Never wrap it in a client-side timeout or `select!` (ADR-0014).
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
