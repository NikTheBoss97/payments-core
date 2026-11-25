// Author: Nicola Antonello

//! Rows that reconciliation's schema tests build on (ADR-0072), written with raw SQL: a
//! movement of ours, that is a capture or a refund that the PSP's history decided, with the
//! merchant's request and the fact's decision on it; the accounts that a batch posts to; a
//! posting; and a batch's own rows.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::payments::{NewPayment, Step, register_merchant};
use super::{runtime_transaction, tenant_transaction};

/// The position of a settled capture's `settled` fact (see [`settled_capture`]).
pub const SETTLED_POSITION: i32 = 2;

/// A capture or a refund of ours: a fact of the PSP's history, with the request that it
/// decided.
#[derive(Debug, Clone)]
pub struct Movement {
    pub merchant: Uuid,
    pub payment: Uuid,
    pub currency: &'static str,
    pub position: i32,
    pub kind: &'static str,
    pub fact_id: String,
    pub request: Uuid,
    pub amount: i64,
}

/// A new merchant's payment in `currency`, committed with its first step.
async fn payment_in(pool: &PgPool, currency: &'static str) -> sqlx::Result<(Uuid, Uuid)> {
    let mut tx = runtime_transaction(pool).await?;
    let merchant = register_merchant(&mut tx).await?;
    let payment = NewPayment {
        currency,
        ..NewPayment::valid(merchant)
    };
    payment.insert(&mut tx).await?;
    Step::first(payment.id).insert(&mut tx).await?;
    tx.commit().await?;
    Ok((merchant, payment.id))
}

/// The merchant's request of `kind` for `amount`, committed as the tenant role, which
/// alone inserts requests (ADR-0028).
async fn request(
    pool: &PgPool,
    merchant: Uuid,
    payment: Uuid,
    kind: &str,
    amount: i64,
) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    let mut tx = tenant_transaction(pool, merchant).await?;
    sqlx::query!(
        "INSERT INTO payment_requests (id, payment_id, kind, amount) VALUES ($1, $2, $3, $4)",
        id,
        payment,
        kind,
        amount
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// The PSP's fact at `position` of `payment`, with the event that announces it, which the
/// fact needs at COMMIT (ADR-0049). Returns the fact's PSP ID.
async fn fact(
    conn: &mut PgConnection,
    merchant: Uuid,
    payment: Uuid,
    position: i32,
    kind: &str,
    amount: i64,
) -> sqlx::Result<String> {
    let fact_id = format!("fct_{}", Uuid::now_v7().simple());
    let prev_position = position
        .checked_sub(1)
        .expect("a fact's position is at least 1")
        .max(1);
    sqlx::query!(
        "INSERT INTO payment_facts \
             (payment_id, position, prev_position, psp, psp_fact_id, kind, amount) \
         VALUES ($1, $2, $3, 'mock', $4, $5, $6)",
        payment,
        position,
        prev_position,
        fact_id,
        kind,
        amount
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query!(
        "INSERT INTO payment_events (id, merchant_id, payment_id, position) \
         VALUES ($1, $2, $3, $4)",
        Uuid::now_v7(),
        merchant,
        payment,
        position
    )
    .execute(&mut *conn)
    .await?;
    Ok(fact_id)
}

/// The decision of `request` by the fact at `position`, of the request's own kind.
async fn decide(
    conn: &mut PgConnection,
    request: Uuid,
    payment: Uuid,
    kind: &str,
    position: i32,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO payment_request_decisions \
             (request_id, payment_id, request_kind, position, fact_kind) \
         VALUES ($1, $2, $3, $4, $3)",
        request,
        payment,
        kind,
        position
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A capture of `amount` in `currency` that the PSP's history settled: an unsettled capture
/// (see [`unsettled_capture`]) and its `settled` fact at [`SETTLED_POSITION`].
pub async fn settled_capture(
    pool: &PgPool,
    currency: &'static str,
    amount: i64,
) -> sqlx::Result<Movement> {
    let capture = unsettled_capture(pool, currency, amount).await?;
    let mut tx = runtime_transaction(pool).await?;
    fact(
        &mut tx,
        capture.merchant,
        capture.payment,
        SETTLED_POSITION,
        "settle",
        0,
    )
    .await?;
    tx.commit().await?;
    Ok(capture)
}

/// A capture of `amount` in `currency` that the PSP's history has not settled yet: the
/// capture's fact at position 1, with the decision on the merchant's request.
pub async fn unsettled_capture(
    pool: &PgPool,
    currency: &'static str,
    amount: i64,
) -> sqlx::Result<Movement> {
    let (merchant, payment) = payment_in(pool, currency).await?;
    let request = request(pool, merchant, payment, "capture", amount).await?;
    let mut tx = runtime_transaction(pool).await?;
    let fact_id = fact(&mut tx, merchant, payment, 1, "capture", amount).await?;
    decide(&mut tx, request, payment, "capture", 1).await?;
    tx.commit().await?;
    Ok(Movement {
        merchant,
        payment,
        currency,
        position: 1,
        kind: "capture",
        fact_id,
        request,
        amount,
    })
}

/// A movement of `kind` for `amount` on the payment of `on`, decided by its fact at
/// `position`, after the facts already there: a refund, or a second capture, since the
/// facts do not judge the order of kinds (ADR-0037).
pub async fn later_movement(
    pool: &PgPool,
    on: &Movement,
    kind: &'static str,
    position: i32,
    amount: i64,
) -> sqlx::Result<Movement> {
    let request = request(pool, on.merchant, on.payment, kind, amount).await?;
    let mut tx = runtime_transaction(pool).await?;
    let fact_id = fact(&mut tx, on.merchant, on.payment, position, kind, amount).await?;
    decide(&mut tx, request, on.payment, kind, position).await?;
    tx.commit().await?;
    Ok(Movement {
        kind,
        position,
        fact_id,
        request,
        amount,
        ..on.clone()
    })
}

/// The accounts that reconciliation posts to for the mock in `currency`: a batch's, and an
/// outcome's claims and write-offs.
#[derive(Debug, Clone, Copy)]
pub struct Accounts {
    pub payouts: Uuid,
    pub fees: Uuid,
    pub receivable: Uuid,
    pub suspense: Uuid,
    pub claims: Uuid,
    pub writeoffs: Uuid,
}

/// Opens the accounts that reconciliation posts to for the mock in `currency`, each in a
/// statement of its own and committed before any posting refers to it (ADR-0005), and
/// returns them. Write-offs belong to the platform, the rest to the PSP (ADR-0069).
pub async fn accounts(pool: &PgPool, currency: &str) -> sqlx::Result<Accounts> {
    let mut tx = runtime_transaction(pool).await?;
    let mut ids = Vec::new();
    for (kind, party) in [
        ("psp_payouts", "mock"),
        ("psp_fees", "mock"),
        ("psp_receivable", "mock"),
        ("settlement_suspense", "mock"),
        ("psp_claims", "mock"),
        ("settlement_writeoffs", "platform"),
    ] {
        sqlx::query!(
            "INSERT INTO accounts (id, kind, party, currency, normal_side) \
             SELECT $1, k.kind, $4, $2, k.normal_side FROM account_kinds k WHERE k.kind = $3 \
             ON CONFLICT (kind, party, currency) DO NOTHING",
            Uuid::now_v7(),
            currency,
            kind,
            party
        )
        .execute(&mut *tx)
        .await?;
        ids.push(
            sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE kind = $1 AND party = $3 AND currency = $2",
                kind,
                currency,
                party
            )
            .fetch_one(&mut *tx)
            .await?,
        );
    }
    tx.commit().await?;
    Ok(Accounts {
        payouts: ids[0],
        fees: ids[1],
        receivable: ids[2],
        suspense: ids[3],
        claims: ids[4],
        writeoffs: ids[5],
    })
}

/// A posting of these entries in `currency`, in the transaction of `conn`. Returns its
/// ledger transaction.
pub async fn post(
    conn: &mut PgConnection,
    currency: &str,
    entries: &[(Uuid, i64)],
) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    let count = i32::try_from(entries.len()).expect("a handful of entries");
    sqlx::query!(
        "INSERT INTO ledger_transactions (id, entry_count, currency) VALUES ($1, $2, $3)",
        id,
        count,
        currency
    )
    .execute(&mut *conn)
    .await?;
    for (line_no, &(account, amount)) in (1_i32..).zip(entries) {
        sqlx::query!(
            "INSERT INTO ledger_entries \
                 (transaction_id, line_no, account_id, amount, entry_count, currency) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            id,
            line_no,
            account,
            amount,
            count,
            currency
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(id)
}

/// A batch of the mock in `currency`, numbered `number`, with this payout.
pub async fn insert_batch(
    conn: &mut PgConnection,
    id: Uuid,
    currency: &str,
    number: i64,
    payout: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_batches (id, psp, currency, number, settled_at, payout) \
         VALUES ($1, 'mock', $2, $3, '2026-11-20 00:00:00+00', $4)",
        id,
        currency,
        number,
        payout
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A line of a batch's file, with its net.
pub async fn insert_line(
    conn: &mut PgConnection,
    batch: Uuid,
    line_no: i32,
    kind: &str,
    (gross, fee, net): (i64, i64, i64),
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_lines (batch_id, line_no, kind, gross, fee, net) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        batch,
        line_no,
        kind,
        gross,
        fee,
        net
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A step of a movement's chain, as the system role writes it.
#[derive(Debug, Clone)]
pub struct MovementStep {
    pub payment: Uuid,
    pub position: i32,
    pub seq: i32,
    pub prev_seq: i32,
    pub id: Uuid,
    pub kind: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub fact_kind: &'static str,
    pub amount: i64,
}

impl MovementStep {
    /// The first step of `movement`'s chain.
    pub fn open(movement: &Movement) -> Self {
        Self {
            payment: movement.payment,
            position: movement.position,
            seq: 0,
            prev_seq: 0,
            id: Uuid::now_v7(),
            kind: "open",
            from: "open",
            to: "open",
            fact_kind: movement.kind,
            amount: movement.amount,
        }
    }

    /// The step after this one, of `kind`, to `to`.
    pub fn then(&self, kind: &'static str, to: &'static str) -> Self {
        Self {
            seq: self.seq.checked_add(1).expect("a short chain"),
            prev_seq: self.seq,
            id: Uuid::now_v7(),
            kind,
            from: self.to,
            to,
            ..self.clone()
        }
    }

    pub async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO settlement_movement_steps \
                 (payment_id, position, seq, prev_seq, id, kind, from_state, to_state, \
                  fact_kind, amount) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            self.payment,
            self.position,
            self.seq,
            self.prev_seq,
            self.id,
            self.kind,
            self.from,
            self.to,
            self.fact_kind,
            self.amount
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// The match of a capture line with `capture`, whose settle step is `step`.
pub async fn insert_capture_match(
    conn: &mut PgConnection,
    batch: Uuid,
    line_no: i32,
    capture: &Movement,
    settled_position: i32,
    step: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_capture_matches \
             (batch_id, line_no, gross, psp, currency, payment_id, position, psp_fact_id, \
              request_id, settled_position, step_id) \
         VALUES ($1, $2, $3, 'mock', $4, $5, $6, $7, $8, $9, $10)",
        batch,
        line_no,
        capture.amount,
        capture.currency,
        capture.payment,
        capture.position,
        capture.fact_id,
        capture.request,
        settled_position,
        step
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The match of a refund line with `refund`, whose settle step is `step`.
pub async fn insert_refund_match(
    conn: &mut PgConnection,
    batch: Uuid,
    line_no: i32,
    refund: &Movement,
    step: Uuid,
) -> sqlx::Result<()> {
    let gross = refund
        .amount
        .checked_neg()
        .expect("a refund's amount is positive");
    sqlx::query!(
        "INSERT INTO settlement_refund_matches \
             (batch_id, line_no, gross, psp, currency, payment_id, position, psp_fact_id, \
              request_id, step_id) \
         VALUES ($1, $2, $3, 'mock', $4, $5, $6, $7, $8, $9)",
        batch,
        line_no,
        gross,
        refund.currency,
        refund.payment,
        refund.position,
        refund.fact_id,
        refund.request,
        step
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The link from a batch to its posting.
pub async fn link_posting(conn: &mut PgConnection, batch: Uuid, posting: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_batch_postings (batch_id, ledger_transaction_id) VALUES ($1, $2)",
        batch,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A discrepancy of `kind`, as the system role writes it.
pub async fn insert_discrepancy(conn: &mut PgConnection, id: Uuid, kind: &str) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_discrepancies (id, kind) VALUES ($1, $2)",
        id,
        kind
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A step of a discrepancy's chain.
pub async fn insert_discrepancy_step(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    (seq, prev_seq, id): (i32, i32, Uuid),
    (kind, from, to): (&str, &str, &str),
    discrepancy_kind: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_discrepancy_steps \
             (discrepancy_id, seq, prev_seq, id, kind, from_state, to_state, discrepancy_kind) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        discrepancy,
        seq,
        prev_seq,
        id,
        kind,
        from,
        to,
        discrepancy_kind
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The first step of a discrepancy of `kind`.
pub async fn find(conn: &mut PgConnection, discrepancy: Uuid, kind: &str) -> sqlx::Result<()> {
    insert_discrepancy_step(
        conn,
        discrepancy,
        (0, 0, Uuid::now_v7()),
        ("find", "open", "open"),
        kind,
    )
    .await
}

/// The subject of a discrepancy about a line that settles nothing of ours.
pub async fn insert_unmatched_line(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    (batch, line_no, net): (Uuid, i32, i64),
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_unmatched_lines (discrepancy_id, batch_id, line_no, net) \
         VALUES ($1, $2, $3, $4)",
        discrepancy,
        batch,
        line_no,
        net
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The subject of a discrepancy about a payout that differs from its nets.
pub async fn insert_payout_difference(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    batch: Uuid,
    amount: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_payout_differences (discrepancy_id, batch_id, amount) \
         VALUES ($1, $2, $3)",
        discrepancy,
        batch,
        amount
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The subject of a discrepancy about a fee off the agreed terms.
pub async fn insert_fee_difference(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    (batch, line_no): (Uuid, i32),
    (charged, agreed): (i64, i64),
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_fee_differences \
             (discrepancy_id, batch_id, line_no, charged, agreed) \
         VALUES ($1, $2, $3, $4, $5)",
        discrepancy,
        batch,
        line_no,
        charged,
        agreed
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The subject of a discrepancy about a missing batch.
pub async fn insert_missing_batch(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    (psp, currency, number): (&str, &str, i64),
    cause: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_missing_batches \
             (discrepancy_id, psp, currency, number, cause) \
         VALUES ($1, $2, $3, $4, $5)",
        discrepancy,
        psp,
        currency,
        number,
        cause
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The subject of an overdue movement's discrepancy, the record of its overdue `step`.
pub async fn insert_overdue_movement(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    step: &MovementStep,
    as_of: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_overdue_movements \
             (discrepancy_id, payment_id, position, step_id, as_of) \
         VALUES ($1, $2, $3, $4, $5::text::timestamptz)",
        discrepancy,
        step.payment,
        step.position,
        step.id,
        as_of
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The subject of a discrepancy about a posting that nothing causes.
pub async fn insert_uncaused_posting(
    conn: &mut PgConnection,
    discrepancy: Uuid,
    posting: Uuid,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO settlement_uncaused_postings (discrepancy_id, ledger_transaction_id) \
         VALUES ($1, $2)",
        discrepancy,
        posting
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Writes batch `number` in EUR with one line of 500, a fee of 32 and a net of 468 that
/// settles nothing of ours, and its posting. Returns the batch.
pub async fn batch_with_a_stray_line(
    conn: &mut PgConnection,
    accounts: &Accounts,
    number: i64,
) -> sqlx::Result<Uuid> {
    let batch = Uuid::now_v7();
    insert_batch(conn, batch, "EUR", number, 468).await?;
    insert_line(conn, batch, 2, "capture", (500, 32, 468)).await?;
    let posting = post(
        conn,
        "EUR",
        &[(accounts.payouts, 468), (accounts.suspense, -468)],
    )
    .await?;
    link_posting(conn, batch, posting).await?;
    Ok(batch)
}

/// Writes batch `number` in EUR with a payout of `payout` and its posting: line 2 of 500, a
/// fee of 32 and a net of 468, which settles nothing of ours, and line 3 settling `paid`, a
/// capture of 10,000, with a fee of 170 where the terms give 165 and a net of 9,830. The
/// payout less 9,830 goes to suspense. Returns the batch.
pub async fn batch_with_a_fee_off_the_terms(
    conn: &mut PgConnection,
    accounts: &Accounts,
    (number, payout): (i64, i64),
    paid: &Movement,
) -> sqlx::Result<Uuid> {
    let batch = Uuid::now_v7();
    insert_batch(conn, batch, "EUR", number, payout).await?;
    insert_line(conn, batch, 2, "capture", (500, 32, 468)).await?;
    insert_line(conn, batch, 3, "capture", (10_000, 170, 9_830)).await?;
    let open = MovementStep::open(paid);
    let settle = open.then("settle", "settled");
    open.insert(conn).await?;
    settle.insert(conn).await?;
    insert_capture_match(conn, batch, 3, paid, SETTLED_POSITION, settle.id).await?;
    let suspense = payout.checked_sub(9_830).expect("small amounts");
    let posting = post(
        conn,
        "EUR",
        &[
            (accounts.payouts, payout),
            (accounts.fees, 170),
            (accounts.receivable, -10_000),
            (
                accounts.suspense,
                suspense.checked_neg().expect("small amounts"),
            ),
        ],
    )
    .await?;
    link_posting(conn, batch, posting).await?;
    Ok(batch)
}

/// A committed posting that nothing causes.
pub async fn uncaused(pool: &PgPool, accounts: &Accounts) -> sqlx::Result<Uuid> {
    let mut tx = runtime_transaction(pool).await?;
    let posting = post(
        &mut tx,
        "EUR",
        &[(accounts.payouts, 1), (accounts.suspense, -1)],
    )
    .await?;
    tx.commit().await?;
    Ok(posting)
}

/// The open and overdue steps of `movement`'s chain. Returns the overdue step.
pub async fn overdue(conn: &mut PgConnection, movement: &Movement) -> sqlx::Result<MovementStep> {
    let open = MovementStep::open(movement);
    let overdue = open.then("overdue", "overdue");
    open.insert(conn).await?;
    overdue.insert(conn).await?;
    Ok(overdue)
}
