// Author: Nicola Antonello

//! Fixtures shared by the payment tests: rows as raw SQL writes them,
//! bypassing the domain types, the way a buggy code path or a manual session
//! could write them (ADR-0017).

use std::error::Error;

use payments_core::accounts::{AccountRef, Psp, find_account};
use payments_core::balances::balance;
use payments_core::db::commit;
use payments_core::ids::{MerchantId, PaymentId};
use payments_core::lifecycle::PaymentStatus;
use payments_core::money::{BasisPoints, Currency, PositiveAmount};
use payments_core::payments::{AccountCandidates, CreatePayment, find_payment, lock_key};
use payments_core::times::BusinessTime;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::{Uuid, uuid};

use super::{account_id, payment_id, runtime_connection, runtime_transaction, transition_id};

/// A valid UUID that is not version 7.
pub const VERSION_4: Uuid = uuid!("019b76da-a800-4000-8000-000000000001");

pub fn eur() -> Currency {
    Currency::parse("EUR").expect("valid code")
}

/// The fee rate of the test merchants and their payments: 2.5%.
pub fn fee_rate() -> BasisPoints {
    BasisPoints::new(250).expect("valid rate")
}

/// The command that creates payment `n` for this merchant: 100.00 EUR through
/// the mock PSP, at a fee rate of 2.5%. Its IDs are fixed, so calling this again
/// builds the identical command, which is what a retry sends (ADR-0019).
pub fn create_command(n: u32, merchant: MerchantId) -> CreatePayment {
    let candidate = |offset| {
        let base = n.checked_mul(3).expect("a small n");
        account_id(base.checked_add(offset).expect("a small n"))
    };
    CreatePayment {
        id: payment_id(n),
        step: transition_id(n),
        merchant,
        psp: Psp::Mock,
        currency: eur(),
        amount: PositiveAmount::new(REQUESTED).expect("valid amount"),
        fee_rate: fee_rate(),
        accounts: AccountCandidates {
            fee_revenue: candidate(0),
            merchant_payable: candidate(1),
            psp_receivable: candidate(2),
        },
    }
}

/// When the test authorizations expire (ADR-0022).
pub fn deadline() -> BusinessTime {
    let time = "2026-01-08T12:00:00.123456Z".parse().expect("a valid time");
    BusinessTime::new(time).expect("a time that can be stored")
}

/// When the test captures took effect.
pub fn capture_time() -> BusinessTime {
    let time = "2026-01-05T09:30:00.654321Z".parse().expect("a valid time");
    BusinessTime::new(time).expect("a time that can be stored")
}

/// An account's balance, debits minus credits. An account that does not exist yet
/// has moved nothing.
pub async fn balance_of(app: &PgPool, reference: AccountRef) -> Result<i64, Box<dyn Error>> {
    let Some(account) = find_account(app, &reference.key()).await? else {
        return Ok(0);
    };
    let read = balance(app, account.id).await?;
    Ok(read.ok_or("the account exists")?.debits_minus_credits)
}

/// The trial balance: every currency whose entries do not sum to zero across the
/// whole ledger. Invariant I1 says there are none (ADR-0002).
pub async fn unbalanced_currencies(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT currency AS "currency!" FROM ledger_entries
            GROUP BY currency HAVING sum(amount) <> 0"#
    )
    .fetch_all(pool)
    .await
}

/// The ledger transactions that no business record references: the detection on
/// the ledger's side of ADR-0021. After a payment flow there are none.
pub async fn uncaused_postings(pool: &PgPool) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar!(
        "SELECT t.id FROM ledger_transactions t \
          WHERE NOT EXISTS (SELECT 1 FROM payment_captures c \
                             WHERE c.ledger_transaction_id = t.id) \
            AND NOT EXISTS (SELECT 1 FROM payment_refunds r \
                             WHERE r.ledger_transaction_id = t.id)"
    )
    .fetch_all(pool)
    .await
}

/// Registers a new merchant and returns its ID.
pub async fn register_merchant(conn: &mut PgConnection) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!("INSERT INTO merchants (id, fee_bps) VALUES ($1, 250)", id)
        .execute(conn)
        .await?;
    Ok(id)
}

/// A payment as the runtime role writes it: every column except
/// `recorded_at`, which only the database sets (ADR-0012).
#[derive(Debug, Clone, Copy)]
pub struct NewPayment<'a> {
    pub id: Uuid,
    pub merchant_id: Uuid,
    pub psp: &'a str,
    pub currency: &'a str,
    pub amount: i64,
    pub fee_bps: i32,
}

impl<'a> NewPayment<'a> {
    /// A valid payment with a fresh ID. Tests change one field to break one
    /// rule.
    pub fn valid(merchant_id: Uuid) -> Self {
        Self {
            id: Uuid::now_v7(),
            merchant_id,
            psp: Psp::Mock.as_str(),
            currency: "EUR",
            amount: 10_000,
            fee_bps: 250,
        }
    }

    pub async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payments (id, merchant_id, psp, currency, amount, fee_bps) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            self.id,
            self.merchant_id,
            self.psp,
            self.currency,
            self.amount,
            self.fee_bps
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Inserts the payment and returns the error, failing the test if the
    /// insert succeeds.
    pub async fn insert_rejected(&self, conn: &mut PgConnection) -> sqlx::Error {
        self.insert(conn)
            .await
            .err()
            .unwrap_or_else(|| panic!("{self:?} was accepted"))
    }
}

/// SQLSTATE of a payment without its first step (ADR-0013).
pub const NO_GENESIS: &str = "LG005";

/// One row of a payment's history as the runtime role writes it.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub payment_id: Uuid,
    pub seq: i32,
    pub prev_seq: i32,
    pub id: Uuid,
    pub kind: &'static str,
    pub from_status: &'static str,
    pub to_status: &'static str,
    pub amount: i64,
    pub prev_refundable: i64,
    pub refundable: i64,
}

impl Step {
    /// A payment's first step, which points to itself.
    pub fn first(payment_id: Uuid) -> Self {
        Self {
            payment_id,
            seq: 0,
            prev_seq: 0,
            id: Uuid::now_v7(),
            kind: "create",
            from_status: "created",
            to_status: "created",
            amount: 0,
            prev_refundable: 0,
            refundable: 0,
        }
    }

    /// The step after this one, starting from its status and refundable
    /// amount.
    pub fn then(
        &self,
        kind: &'static str,
        to_status: &'static str,
        amount: i64,
        refundable: i64,
    ) -> Self {
        Self {
            payment_id: self.payment_id,
            seq: self.seq.checked_add(1).expect("a short history"),
            prev_seq: self.seq,
            id: Uuid::now_v7(),
            kind,
            from_status: self.to_status,
            to_status,
            amount,
            prev_refundable: self.refundable,
            refundable,
        }
    }

    pub async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_transitions \
                 (payment_id, seq, prev_seq, id, kind, from_status, to_status, \
                  amount, prev_refundable, refundable) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            self.payment_id,
            self.seq,
            self.prev_seq,
            self.id,
            self.kind,
            self.from_status,
            self.to_status,
            self.amount,
            self.prev_refundable,
            self.refundable
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Inserts the step and returns the error, failing the test if the insert
    /// succeeds.
    pub async fn insert_rejected(&self, conn: &mut PgConnection) -> sqlx::Error {
        self.insert(conn)
            .await
            .err()
            .unwrap_or_else(|| panic!("{self:?} was accepted"))
    }

    /// [`Self::insert_rejected`] in a savepoint, so that the rejection does not
    /// end the surrounding transaction.
    pub async fn rejected_in(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> sqlx::Result<sqlx::Error> {
        savepoint(tx).await?;
        let err = self.insert_rejected(tx).await;
        back_to_savepoint(tx).await?;
        Ok(err)
    }
}

/// Registers a merchant, then writes a payment with its first step.
pub async fn payment_with_first_step(conn: &mut PgConnection) -> sqlx::Result<Step> {
    let merchant = register_merchant(conn).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(conn).await?;
    let first = Step::first(payment.id);
    first.insert(conn).await?;
    Ok(first)
}

/// The requested amount of [`NewPayment::valid`].
pub const REQUESTED: i64 = 10_000;
/// What the test payments are authorized for, and then captured.
pub const AUTHORIZED: i64 = 8_000;
/// The fee on that capture, at the 250 basis points of [`NewPayment::valid`].
pub const FEE: i64 = 200;
/// SQLSTATE of a step without its record (ADR-0013).
pub const NO_RECORD: &str = "LG004";
/// The trigger that checks each step at COMMIT.
pub const STEP_COMMIT_CHECK: &str = "payment_transitions_commit_check";

/// Sets a savepoint that [`back_to_savepoint`] returns to, so that a rejected
/// statement does not end the surrounding transaction.
pub async fn savepoint(tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<()> {
    sqlx::query!("SAVEPOINT rejected")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn back_to_savepoint(tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<()> {
    sqlx::query!("ROLLBACK TO SAVEPOINT rejected")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A merchant and the EUR accounts that its payments move money between.
#[derive(Debug, Clone, Copy)]
pub struct Books {
    pub merchant: Uuid,
    pub receivable: Uuid,
    pub payable: Uuid,
    pub fees: Uuid,
}

impl Books {
    /// ADR-0020's lines for a capture, a zero line left out.
    pub fn capture_lines(&self, captured: i64, fee: i64) -> Vec<(Uuid, i64)> {
        [
            (self.receivable, captured),
            (self.payable, fee - captured),
            (self.fees, -fee),
        ]
        .into_iter()
        .filter(|line| line.1 != 0)
        .collect()
    }

    /// ADR-0020's lines for a refund.
    pub fn refund_lines(&self, refund: i64) -> Vec<(Uuid, i64)> {
        vec![(self.payable, refund), (self.receivable, -refund)]
    }
}

pub async fn open_account(
    conn: &mut PgConnection,
    kind: &str,
    party: &str,
    side: &str,
) -> sqlx::Result<Uuid> {
    open_account_in(conn, "EUR", kind, party, side).await
}

pub async fn open_account_in(
    conn: &mut PgConnection,
    currency: &str,
    kind: &str,
    party: &str,
    side: &str,
) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency, normal_side) \
         VALUES ($1, $2, $3, $4, $5)",
        id,
        kind,
        party,
        currency,
        side
    )
    .execute(conn)
    .await?;
    Ok(id)
}

pub async fn open_books(conn: &mut PgConnection) -> sqlx::Result<Books> {
    let merchant = register_merchant(conn).await?;
    Ok(Books {
        merchant,
        receivable: open_account(conn, "psp_receivable", Psp::Mock.as_str(), "debit").await?,
        payable: open_account(
            conn,
            "merchant_payable",
            &merchant.hyphenated().to_string(),
            "credit",
        )
        .await?,
        fees: open_account(conn, "fee_revenue", "platform", "credit").await?,
    })
}

/// Writes an EUR posting with these lines and returns its ID.
pub async fn write_posting(conn: &mut PgConnection, lines: &[(Uuid, i64)]) -> sqlx::Result<Uuid> {
    write_posting_in(conn, "EUR", lines).await
}

pub async fn write_posting_in(
    conn: &mut PgConnection,
    currency: &str,
    lines: &[(Uuid, i64)],
) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    let entry_count = i32::try_from(lines.len()).expect("a few lines");
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
        id,
        entry_count,
        currency
    )
    .execute(&mut *conn)
    .await?;
    for (line_no, (account, amount)) in (1..).zip(lines) {
        sqlx::query!(
            "INSERT INTO ledger_entries (transaction_id, line_no, entry_count, account_id, currency, amount) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            id,
            line_no,
            entry_count,
            account,
            currency,
            amount
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

pub async fn insert_authorization(
    conn: &mut PgConnection,
    step: &Step,
    payment_amount: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_authorizations \
             (transition_id, payment_id, amount, payment_amount, expires_at) \
         VALUES ($1, $2, $3, $4, transaction_timestamp())",
        step.id,
        step.payment_id,
        step.amount,
        payment_amount
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn insert_capture(
    conn: &mut PgConnection,
    step: &Step,
    authorized: i64,
    fee: i64,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_captures \
             (transition_id, payment_id, amount, authorized_amount, fee, captured_at, \
              ledger_transaction_id) \
         VALUES ($1, $2, $3, $4, $5, transaction_timestamp(), $6)",
        step.id,
        step.payment_id,
        step.amount,
        authorized,
        fee,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn insert_refund(
    conn: &mut PgConnection,
    step: &Step,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_refunds \
             (transition_id, payment_id, amount, refunded_at, ledger_transaction_id) \
         VALUES ($1, $2, $3, transaction_timestamp(), $4)",
        step.id,
        step.payment_id,
        step.amount,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Writes a payment up to its authorization, with its record.
pub async fn authorized_payment(conn: &mut PgConnection, books: &Books) -> sqlx::Result<Step> {
    let payment = NewPayment::valid(books.merchant);
    payment.insert(conn).await?;
    let first = Step::first(payment.id);
    first.insert(conn).await?;
    let authorized = first.then("authorize", "authorized", AUTHORIZED, 0);
    authorized.insert(conn).await?;
    insert_authorization(conn, &authorized, REQUESTED).await?;
    Ok(authorized)
}

/// Writes a payment up to its capture, with its records and posting.
pub async fn captured_payment(conn: &mut PgConnection, books: &Books) -> sqlx::Result<Step> {
    let authorized = authorized_payment(conn, books).await?;
    let captured = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    captured.insert(conn).await?;
    let posting = write_posting(conn, &books.capture_lines(AUTHORIZED, FEE)).await?;
    insert_capture(conn, &captured, AUTHORIZED, FEE, posting).await?;
    Ok(captured)
}

/// Writes a refund after `previous`, with its record and posting.
pub async fn refund_step(
    conn: &mut PgConnection,
    books: &Books,
    previous: &Step,
    to_status: &'static str,
    amount: i64,
) -> sqlx::Result<Step> {
    let refund = previous.then("refund", to_status, amount, previous.refundable - amount);
    refund.insert(conn).await?;
    let posting = write_posting(conn, &books.refund_lines(amount)).await?;
    insert_refund(conn, &refund, posting).await?;
    Ok(refund)
}

/// Writes a payment that stands at `status`, and returns its newest step. Commit
/// the database transaction that holds `conn` before any command reads it.
pub async fn payment_at(
    conn: &mut PgConnection,
    books: &Books,
    status: PaymentStatus,
) -> sqlx::Result<Step> {
    // Each arm writes its payment's steps; `step` writes one more after them.
    let step = match status {
        PaymentStatus::Authorized => return authorized_payment(conn, books).await,
        PaymentStatus::Captured => return captured_payment(conn, books).await,
        PaymentStatus::Refunded => {
            let captured = captured_payment(conn, books).await?;
            return refund_step(conn, books, &captured, "refunded", AUTHORIZED).await;
        }
        PaymentStatus::Settled => {
            let captured = captured_payment(conn, books).await?;
            captured.then("settle", "settled", 0, AUTHORIZED)
        }
        PaymentStatus::Created | PaymentStatus::Failed | PaymentStatus::Voided => {
            let payment = NewPayment::valid(books.merchant);
            payment.insert(conn).await?;
            let first = Step::first(payment.id);
            match status {
                PaymentStatus::Failed => {
                    first.insert(conn).await?;
                    first.then("decline", "failed", 0, 0)
                }
                PaymentStatus::Voided => {
                    first.insert(conn).await?;
                    first.then("cancel", "voided", 0, 0)
                }
                _ => first,
            }
        }
    };
    step.insert(conn).await?;
    Ok(step)
}

/// A merchant and its accounts.
pub async fn books(pool: &PgPool) -> Result<Books, Box<dyn Error>> {
    let mut conn = runtime_connection(pool).await?;
    Ok(open_books(&mut conn).await?)
}

/// Commits a payment that stands at `status`, and returns its newest step.
pub async fn committed_at(
    pool: &PgPool,
    books: &Books,
    status: PaymentStatus,
) -> Result<Step, Box<dyn Error>> {
    let mut tx = runtime_transaction(pool).await?;
    let step = payment_at(&mut tx, books, status).await?;
    commit(tx).await?;
    Ok(step)
}

/// How many requests to the PSP a payment has recorded.
pub async fn requests_of(pool: &PgPool, payment: PaymentId) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM payment_requests WHERE payment_id = $1"#,
        payment.get()
    )
    .fetch_one(pool)
    .await
}

/// How many steps a payment has.
pub async fn steps(pool: &PgPool, payment: PaymentId) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM payment_transitions WHERE payment_id = $1"#,
        payment.get()
    )
    .fetch_one(pool)
    .await
}

pub async fn status_of(app: &PgPool, payment: PaymentId) -> Result<PaymentStatus, Box<dyn Error>> {
    let payment = find_payment(app, payment).await?;
    Ok(payment.ok_or("the payment exists")?.status)
}

/// Takes a payment's lock, as every command does first (ADR-0018).
pub async fn take_lock(tx: &mut Transaction<'_, Postgres>, payment: PaymentId) -> sqlx::Result<()> {
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", lock_key(payment))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
