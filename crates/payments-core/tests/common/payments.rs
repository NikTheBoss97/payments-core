// Author: Nicola Antonello

//! Fixtures shared by the payment tests: rows as raw SQL writes them,
//! bypassing the domain types, the way a buggy code path or a manual session
//! could write them (ADR-0017).

use payments_core::accounts::Psp;
use sqlx::{PgConnection, Postgres, Transaction};
use uuid::{Uuid, uuid};

/// A valid UUID that is not version 7.
pub const VERSION_4: Uuid = uuid!("019b76da-a800-4000-8000-000000000001");

/// Registers a new merchant and returns its ID.
pub async fn register_merchant(conn: &mut PgConnection) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!("INSERT INTO merchants (id) VALUES ($1)", id)
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
