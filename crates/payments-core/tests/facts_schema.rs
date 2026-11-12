// Author: Nicola Antonello

//! Database tests for `payment_facts`, `payment_request_decisions`,
//! `payment_transition_facts` and `payment_stops` (ADR-0031, ADR-0034, ADR-0037).
//!
//! Rows are written with raw SQL as the system role, `payments_app`, which runs the
//! catch-up, the way a buggy code path or a manual session could write them. Each
//! rejected write breaks a single rule. The test asserts the SQLSTATE and, where
//! PostgreSQL reports one, the constraint or trigger that rejects the write;
//! privilege errors carry none.

mod common;

use std::error::Error;

use common::payments::{
    AUTHORIZED, FEE, NewPayment, REQUESTED, Step, authorized_payment, books, committed_at,
    insert_authorization, insert_capture, refund_step, register_merchant, write_posting,
};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, runtime_transaction, sqlstate,
    tenant_login, tenant_transaction,
};
use payments_core::db::commit;
use payments_core::lifecycle::PaymentStatus;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn Error>>;

/// The cap of a ledger amount (ADR-0003).
const CAP: i64 = 1_000_000_000_000_000;

/// A fresh ID of the PSP's shape for a fact.
fn fact_id() -> String {
    format!("fct_{}", Uuid::now_v7().simple())
}

/// One fact of a payment's history as the system role writes it: every column
/// except `recorded_at`.
#[derive(Debug, Clone)]
struct Fact {
    payment_id: Uuid,
    position: i32,
    prev_position: i32,
    psp: &'static str,
    psp_fact_id: String,
    kind: &'static str,
    amount: i64,
}

impl Fact {
    /// A payment's first fact, which points to itself.
    fn first(payment_id: Uuid, kind: &'static str, amount: i64) -> Self {
        Self {
            payment_id,
            position: 1,
            prev_position: 1,
            psp: "mock",
            psp_fact_id: fact_id(),
            kind,
            amount,
        }
    }

    /// The fact after this one. The table does not judge the order of kinds,
    /// which is the PSP's: the catch-up applies them through the table of moves.
    fn then(&self, kind: &'static str, amount: i64) -> Self {
        Self {
            payment_id: self.payment_id,
            position: self.position.checked_add(1).expect("a short history"),
            prev_position: self.position,
            psp: self.psp,
            psp_fact_id: fact_id(),
            kind,
            amount,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_facts \
                 (payment_id, position, prev_position, psp, psp_fact_id, kind, amount) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            self.payment_id,
            self.position,
            self.prev_position,
            self.psp,
            self.psp_fact_id,
            self.kind,
            self.amount
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// A request to the PSP, which the owner writes here to set a decision up.
#[derive(Debug, Clone, Copy)]
struct Request {
    id: Uuid,
    payment_id: Uuid,
    kind: &'static str,
    amount: i64,
}

impl Request {
    fn new(payment_id: Uuid, kind: &'static str, amount: i64) -> Self {
        Self {
            id: Uuid::now_v7(),
            payment_id,
            kind,
            amount,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_requests (id, payment_id, kind, amount) VALUES ($1, $2, $3, $4)",
            self.id,
            self.payment_id,
            self.kind,
            self.amount
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// The fact that decided a request, as the system role writes it.
#[derive(Debug, Clone, Copy)]
struct Decision {
    request_id: Uuid,
    payment_id: Uuid,
    request_kind: &'static str,
    position: i32,
    fact_kind: &'static str,
}

impl Decision {
    /// The decision of this request by this fact, with both kinds copied right.
    fn of(request: &Request, fact: &Fact) -> Self {
        Self {
            request_id: request.id,
            payment_id: fact.payment_id,
            request_kind: request.kind,
            position: fact.position,
            fact_kind: fact.kind,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_request_decisions \
                 (request_id, payment_id, request_kind, position, fact_kind) \
             VALUES ($1, $2, $3, $4, $5)",
            self.request_id,
            self.payment_id,
            self.request_kind,
            self.position,
            self.fact_kind
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// The fact that made a step, as the system role writes it.
#[derive(Debug, Clone, Copy)]
struct Link {
    transition_id: Uuid,
    payment_id: Uuid,
    kind: &'static str,
    amount: i64,
    position: i32,
}

impl Link {
    /// The link from this step to this fact, with the step's kind and amount.
    fn of(step: &Step, fact: &Fact) -> Self {
        Self {
            transition_id: step.id,
            payment_id: step.payment_id,
            kind: step.kind,
            amount: step.amount,
            position: fact.position,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_transition_facts \
                 (transition_id, payment_id, kind, amount, position) \
             VALUES ($1, $2, $3, $4, $5)",
            self.transition_id,
            self.payment_id,
            self.kind,
            self.amount,
            self.position
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// A payment that the catch-up stopped, as the system role writes it.
#[derive(Debug, Clone, Copy)]
struct Stop {
    payment_id: Uuid,
    position: i32,
    reason: &'static str,
}

impl Stop {
    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_stops (payment_id, position, reason) VALUES ($1, $2, $3)",
            self.payment_id,
            self.position,
            self.reason
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// A new merchant's payment, committed by the system role with the first step that a
/// committed payment needs, and authorized too, with its record, if `authorize`.
/// Returns the merchant and the payment's newest step. A test that needs several
/// merchants uses this rather than [`books`], which opens the platform's accounts,
/// and these exist once in a database.
async fn new_payment(pool: &PgPool, authorize: bool) -> sqlx::Result<(Uuid, Step)> {
    let mut tx = runtime_transaction(pool).await?;
    let merchant = register_merchant(&mut tx).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    let mut newest = Step::first(payment.id);
    newest.insert(&mut tx).await?;
    if authorize {
        newest = newest.then("authorize", "authorized", AUTHORIZED, 0);
        newest.insert(&mut tx).await?;
        insert_authorization(&mut tx, &newest, REQUESTED).await?;
    }
    tx.commit().await?;
    Ok((merchant, newest))
}

/// A new merchant's payment, created and committed.
async fn created_payment(pool: &PgPool) -> sqlx::Result<Uuid> {
    Ok(new_payment(pool, false).await?.1.payment_id)
}

/// How many facts, decisions, links of steps and stops the connection sees.
async fn visible(conn: &mut PgConnection) -> sqlx::Result<(i64, i64, i64, i64)> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM payment_facts) AS "facts!",
                  (SELECT count(*) FROM payment_request_decisions) AS "decisions!",
                  (SELECT count(*) FROM payment_transition_facts) AS "links!",
                  (SELECT count(*) FROM payment_stops) AS "stops!""#
    )
    .fetch_one(conn)
    .await?;
    Ok((row.facts, row.decisions, row.links, row.stops))
}

#[sqlx::test]
async fn facts_chain_from_position_one_without_a_gap(pool: PgPool) -> TestResult {
    let payment = created_payment(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    let first = Fact::first(payment, "authorize", 100);
    first.insert(&mut app).await?;
    let second = first.then("capture", 100);
    second.insert(&mut app).await?;

    let err = Fact {
        psp_fact_id: fact_id(),
        ..second.clone()
    }
    .insert(&mut app)
    .await
    .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_facts_pkey");
    // The fact after a position that the payment does not hold.
    let third = second.then("refund", 1);
    let err = third.then("refund", 1).insert(&mut app).await.unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_facts_prev_fkey");
    // Positions that skip their predecessor, or that do not start at 1.
    for (position, prev_position) in [(3, 1), (3, 3), (1, 0), (0, 0), (0, -1), (-1, -2)] {
        let err = Fact {
            position,
            prev_position,
            ..third.clone()
        }
        .insert(&mut app)
        .await
        .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "payment_facts_position_check");
    }
    Ok(())
}

#[sqlx::test]
async fn a_fact_comes_from_its_payments_psp_under_an_id_unique_per_psp(pool: PgPool) -> TestResult {
    let a = created_payment(&pool).await?;
    let b = created_payment(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    let of_a = Fact::first(a, "authorize", 100);
    of_a.insert(&mut app).await?;

    let err = Fact {
        psp_fact_id: of_a.psp_fact_id.clone(),
        ..Fact::first(b, "authorize", 100)
    }
    .insert(&mut app)
    .await
    .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_facts_psp_fact_id_key");
    // Another PSP than the payment's, and a payment that does not exist.
    for fact in [
        Fact {
            psp: "stripe",
            ..Fact::first(b, "authorize", 100)
        },
        Fact::first(Uuid::now_v7(), "authorize", 100),
    ] {
        let err = fact.insert(&mut app).await.unwrap_err();
        assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_facts_payment_fkey");
    }
    Ok(())
}

#[sqlx::test]
async fn a_fact_has_a_known_kind_and_an_amount_for_its_kind(pool: PgPool) -> TestResult {
    let payment = created_payment(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let mut fact = Fact::first(payment, "authorize", 1);
    fact.insert(&mut tx).await?;
    for (kind, amount) in [
        ("decline", 0),
        ("capture", CAP),
        ("capture_refused", 0),
        ("void", 0),
        ("void_refused", 0),
        ("expire", 0),
        ("settle", 0),
        ("refund", 1),
        ("refund_refused", 0),
    ] {
        fact = fact.then(kind, amount);
        fact.insert(&mut tx).await?;
    }
    tx.rollback().await?;

    // PostgreSQL checks a table's CHECKs in the order of their names, so a kind
    // that is not one moves 0, which the amount's CHECK lets through.
    let mut app = runtime_connection(&pool).await?;
    for (kind, amount, constraint) in [
        ("create", 0, "payment_facts_kind_check"),
        ("cancel", 0, "payment_facts_kind_check"),
        ("refunded", 0, "payment_facts_kind_check"),
        ("", 0, "payment_facts_kind_check"),
        ("authorize", 0, "payment_facts_amount_check"),
        ("capture", CAP + 1, "payment_facts_amount_check"),
        ("refund", -1, "payment_facts_amount_check"),
        ("decline", 1, "payment_facts_amount_check"),
        ("capture_refused", 1, "payment_facts_amount_check"),
        ("settle", 1, "payment_facts_amount_check"),
    ] {
        let err = Fact::first(payment, kind, amount)
            .insert(&mut app)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, constraint);
    }
    Ok(())
}

#[sqlx::test]
async fn a_fact_id_has_one_shape_without_a_dot(pool: PgPool) -> TestResult {
    let payment = created_payment(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let mut fact = Fact {
        psp_fact_id: "f".repeat(255),
        ..Fact::first(payment, "authorize", 1)
    };
    fact.insert(&mut tx).await?;
    for id in ["0", "z", "_", "-", "re_3MtwBwLkdIwHu7ix28a3tqPa"] {
        fact = Fact {
            psp_fact_id: id.to_owned(),
            ..fact.then("settle", 0)
        };
        fact.insert(&mut tx).await?;
    }
    tx.rollback().await?;

    let mut app = runtime_connection(&pool).await?;
    let over_the_limit = "f".repeat(256);
    for id in [
        "",
        ".",
        "fct.1",
        " ",
        "a b",
        "é",
        "a\nb",
        over_the_limit.as_str(),
    ] {
        let err = Fact {
            psp_fact_id: id.to_owned(),
            ..Fact::first(payment, "authorize", 1)
        }
        .insert(&mut app)
        .await
        .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "payment_facts_psp_fact_id_check");
    }
    Ok(())
}

#[sqlx::test]
async fn a_request_is_decided_once_by_a_fact_of_a_matching_kind(pool: PgPool) -> TestResult {
    let payment = created_payment(&pool).await?;
    let other = created_payment(&pool).await?;
    let [authorize, capture, second_capture, void, refund, of_other] = [
        Request::new(payment, "authorize", 100),
        Request::new(payment, "capture", 100),
        Request::new(payment, "capture", 100),
        Request::new(payment, "void", 0),
        Request::new(payment, "refund", 50),
        Request::new(other, "capture", 100),
    ];
    let mut owner = pool.acquire().await?;
    for request in [authorize, capture, second_capture, void, refund, of_other] {
        request.insert(&mut owner).await?;
    }
    let mut app = runtime_connection(&pool).await?;
    let authorized = Fact::first(payment, "authorize", 100);
    let captured = authorized.then("capture", 100);
    let refused = captured.then("refund_refused", 0);
    let declined = refused.then("decline", 0);
    let captured_again = declined.then("capture", 100);
    for fact in [&authorized, &captured, &refused, &declined, &captured_again] {
        fact.insert(&mut app).await?;
    }
    Decision::of(&authorize, &authorized)
        .insert(&mut app)
        .await?;
    Decision::of(&capture, &captured).insert(&mut app).await?;
    Decision::of(&refund, &refused).insert(&mut app).await?;

    // A second decision of a request, by a fact that could have decided it.
    let err = Decision::of(&authorize, &declined)
        .insert(&mut app)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_request_decisions_pkey");
    // A fact that decides a second request.
    let err = Decision::of(&second_capture, &captured)
        .insert(&mut app)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_request_decisions_fact_key");
    // A fact of a kind that cannot decide the request.
    let err = Decision::of(&void, &captured_again)
        .insert(&mut app)
        .await
        .unwrap_err();
    assert_violation(
        err,
        CHECK_VIOLATION,
        "payment_request_decisions_kinds_check",
    );
    // Copies that are not true, of the request's kind and then of the fact's, and a
    // request of another payment than the fact's.
    for (decision, constraint) in [
        (
            Decision {
                request_kind: "capture",
                ..Decision::of(&void, &captured_again)
            },
            "payment_request_decisions_request_fkey",
        ),
        (
            Decision {
                fact_kind: "void",
                ..Decision::of(&void, &captured_again)
            },
            "payment_request_decisions_fact_fkey",
        ),
        (
            Decision::of(&of_other, &captured_again),
            "payment_request_decisions_request_fkey",
        ),
    ] {
        let err = decision.insert(&mut app).await.unwrap_err();
        assert_violation(err, FOREIGN_KEY_VIOLATION, constraint);
    }
    Ok(())
}

#[sqlx::test]
async fn a_step_and_the_fact_that_made_it_agree_on_kind_and_amount(pool: PgPool) -> TestResult {
    // A capture and two refunds of the same amount: two steps of one kind and one
    // amount, which is what a fact needs to make two steps.
    let books = books(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    let authorized = authorized_payment(&mut tx, &books).await?;
    let captured = authorized.then("capture", "captured", AUTHORIZED, AUTHORIZED);
    captured.insert(&mut tx).await?;
    let posting = write_posting(&mut tx, &books.capture_lines(AUTHORIZED, FEE)).await?;
    insert_capture(&mut tx, &captured, AUTHORIZED, FEE, posting).await?;
    let first_refund = refund_step(&mut tx, &books, &captured, "captured", 100).await?;
    let second_refund = refund_step(&mut tx, &books, &first_refund, "captured", 100).await?;
    commit(tx).await?;
    let payment = authorized.payment_id;
    let created = sqlx::query_scalar!(
        "SELECT id FROM payment_transitions WHERE payment_id = $1 AND seq = 0",
        payment
    )
    .fetch_one(&pool)
    .await?;
    let cancelled = committed_at(&pool, &books, PaymentStatus::Voided).await?;

    let mut app = runtime_connection(&pool).await?;
    let f1 = Fact::first(payment, "authorize", AUTHORIZED);
    let f2 = f1.then("capture", AUTHORIZED);
    let f3 = f2.then("refund", 100);
    let f4 = f3.then("authorize", AUTHORIZED);
    let f5 = f4.then("refund", 200);
    let f6 = f5.then("refund_refused", 0);
    for fact in [&f1, &f2, &f3, &f4, &f5, &f6] {
        fact.insert(&mut app).await?;
    }
    let of_cancelled = Fact::first(cancelled.payment_id, "void", 0);
    of_cancelled.insert(&mut app).await?;
    Link::of(&authorized, &f1).insert(&mut app).await?;
    Link::of(&captured, &f2).insert(&mut app).await?;
    Link::of(&first_refund, &f3).insert(&mut app).await?;

    // A second fact for a step, and a fact for a second step.
    let err = Link::of(&authorized, &f4)
        .insert(&mut app)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_transition_facts_pkey");
    let err = Link::of(&second_refund, &f3)
        .insert(&mut app)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_transition_facts_fact_key");
    // Each link below names a position that no link holds yet, so that the unique
    // key, checked as the row goes in, cannot fire before the foreign keys, which
    // PostgreSQL checks at the end of the statement.
    for (link, constraint) in [
        // A fact of another kind and amount than the step's.
        (
            Link::of(&second_refund, &f4),
            "payment_transition_facts_fact_fkey",
        ),
        // A step of another amount than the fact's.
        (
            Link {
                amount: 200,
                ..Link::of(&second_refund, &f5)
            },
            "payment_transition_facts_step_fkey",
        ),
        // A refusal, which makes no step.
        (
            Link {
                kind: "refund_refused",
                amount: 0,
                ..Link::of(&second_refund, &f6)
            },
            "payment_transition_facts_step_fkey",
        ),
        // The service's own steps, which no fact makes.
        (
            Link {
                transition_id: created,
                kind: "create",
                amount: 0,
                ..Link::of(&authorized, &f4)
            },
            "payment_transition_facts_fact_fkey",
        ),
        (
            Link::of(&cancelled, &of_cancelled),
            "payment_transition_facts_fact_fkey",
        ),
    ] {
        let err = link.insert(&mut app).await.unwrap_err();
        assert_violation(err, FOREIGN_KEY_VIOLATION, constraint);
    }
    Ok(())
}

#[sqlx::test]
async fn a_payment_stops_once_for_a_known_reason(pool: PgPool) -> TestResult {
    let payment = created_payment(&pool).await?;
    let other = created_payment(&pool).await?;
    let mut tx = runtime_transaction(&pool).await?;
    for (payment, reason) in [
        (payment, "history_changed"),
        (other, "no_place"),
        (created_payment(&pool).await?, "contradiction"),
    ] {
        Stop {
            payment_id: payment,
            position: 1,
            reason,
        }
        .insert(&mut tx)
        .await?;
    }
    tx.rollback().await?;

    let mut app = runtime_connection(&pool).await?;
    let stop = Stop {
        payment_id: payment,
        position: 3,
        reason: "history_changed",
    };
    stop.insert(&mut app).await?;
    let err = Stop {
        position: 4,
        reason: "no_place",
        ..stop
    }
    .insert(&mut app)
    .await
    .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_stops_pkey");
    for (stop, code, constraint) in [
        (
            Stop {
                payment_id: other,
                position: 0,
                reason: "no_place",
            },
            CHECK_VIOLATION,
            "payment_stops_position_check",
        ),
        (
            Stop {
                payment_id: other,
                position: 1,
                reason: "skipped",
            },
            CHECK_VIOLATION,
            "payment_stops_reason_check",
        ),
        (
            Stop {
                payment_id: Uuid::now_v7(),
                position: 1,
                reason: "no_place",
            },
            FOREIGN_KEY_VIOLATION,
            "payment_stops_payment_fkey",
        ),
    ] {
        let err = stop.insert(&mut app).await.unwrap_err();
        assert_violation(err, code, constraint);
    }
    Ok(())
}

/// An authorized payment of a new merchant, with its fact, its link, its request
/// and decision, and a stop: a row in each table. Returns the merchant and the
/// payment.
async fn one_of_each(pool: &PgPool) -> sqlx::Result<(Uuid, Uuid)> {
    let (merchant, authorized) = new_payment(pool, true).await?;
    let payment = authorized.payment_id;
    let request = Request::new(payment, "authorize", AUTHORIZED);
    let mut owner = pool.acquire().await?;
    request.insert(&mut owner).await?;
    let mut app = runtime_connection(pool).await?;
    let fact = Fact::first(payment, "authorize", AUTHORIZED);
    fact.insert(&mut app).await?;
    Link::of(&authorized, &fact).insert(&mut app).await?;
    Decision::of(&request, &fact).insert(&mut app).await?;
    Stop {
        payment_id: payment,
        position: 2,
        reason: "no_place",
    }
    .insert(&mut app)
    .await?;
    Ok((merchant, payment))
}

#[sqlx::test]
async fn a_merchant_reads_only_its_own_facts_decisions_links_and_stops(pool: PgPool) -> TestResult {
    let (a, _) = one_of_each(&pool).await?;
    let (b, _) = one_of_each(&pool).await?;
    for merchant in [a, b] {
        let mut tenant = tenant_transaction(&pool, merchant).await?;
        assert_eq!(visible(&mut tenant).await?, (1, 1, 1, 1));
        tenant.rollback().await?;
    }
    let mut login = tenant_login(&pool).await?;
    assert_eq!(visible(&mut login).await?, (0, 0, 0, 0));
    login.close().await?;
    Ok(())
}

#[sqlx::test]
async fn only_the_system_role_writes_them_and_neither_role_changes_one(pool: PgPool) -> TestResult {
    let (_, payment) = one_of_each(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        // A merchant reads the facts and decisions of its requests, the links of its
        // steps (ADR-0045) and its payments' stops (ADR-0042), and writes none of these
        // tables.
        Fact::first(payment, "settle", 0)
            .insert(&mut tenant)
            .await
            .unwrap_err(),
        Stop {
            payment_id: payment,
            position: 1,
            reason: "no_place",
        }
        .insert(&mut tenant)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_request_decisions \
                 (request_id, payment_id, request_kind, position, fact_kind) \
             VALUES ($1, $2, 'void', 1, 'void')",
            Uuid::now_v7(),
            payment
        )
        .execute(&mut tenant)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_transition_facts \
                 (transition_id, payment_id, kind, amount, position) \
             VALUES ($1, $2, 'settle', 0, 1)",
            Uuid::now_v7(),
            payment
        )
        .execute(&mut tenant)
        .await
        .unwrap_err(),
        // Only the database chooses a recorded_at (ADR-0012).
        sqlx::query!(
            "INSERT INTO payment_facts \
                 (payment_id, position, prev_position, psp, psp_fact_id, kind, amount, \
                  recorded_at) \
             VALUES ($1, 2, 1, 'mock', 'fct_2', 'settle', 0, now())",
            payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_stops (payment_id, position, reason, recorded_at) \
             VALUES ($1, 1, 'no_place', now())",
            payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    for conn in [&mut app, &mut tenant] {
        for err in [
            sqlx::query!("UPDATE payment_facts SET amount = 1")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_facts")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_facts CASCADE")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE payment_request_decisions SET position = 2")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_request_decisions")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_request_decisions")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE payment_transition_facts SET position = 2")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_transition_facts")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_transition_facts")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE payment_stops SET reason = 'contradiction'")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_stops")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_stops")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
        }
    }
    tenant.close().await?;
    Ok(())
}

#[sqlx::test]
async fn facts_and_what_hangs_on_them_are_append_only_even_for_the_owner(
    pool: PgPool,
) -> TestResult {
    // A fact that could change would no longer be the PSP's, and a link or a stop
    // taken back would let the catch-up apply a fact twice, or past a stop.
    one_of_each(&pool).await?;
    let mut owner = pool.acquire().await?;

    let table = "payment_facts";
    let update = sqlx::query!("UPDATE payment_facts SET amount = 1")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "payment_facts_append_only", table);
    let delete = sqlx::query!("DELETE FROM payment_facts")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "payment_facts_append_only", table);
    // CASCADE, because the decisions, the links and the facts themselves refer to
    // the facts: without it, PostgreSQL fails with 0A000 before any trigger runs.
    // The triggers of the named table fire first, which the trigger's name proves.
    let truncate = sqlx::query!("TRUNCATE payment_facts CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "payment_facts_no_truncate", table);

    let table = "payment_request_decisions";
    let update = sqlx::query!("UPDATE payment_request_decisions SET position = 2")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "payment_request_decisions_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM payment_request_decisions")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "payment_request_decisions_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE payment_request_decisions")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "payment_request_decisions_no_truncate",
        table,
    );

    let table = "payment_transition_facts";
    let update = sqlx::query!("UPDATE payment_transition_facts SET position = 2")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "payment_transition_facts_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM payment_transition_facts")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "payment_transition_facts_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE payment_transition_facts")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "payment_transition_facts_no_truncate",
        table,
    );

    let table = "payment_stops";
    let update = sqlx::query!("UPDATE payment_stops SET reason = 'contradiction'")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "payment_stops_append_only", table);
    let delete = sqlx::query!("DELETE FROM payment_stops")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "payment_stops_append_only", table);
    let truncate = sqlx::query!("TRUNCATE payment_stops")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "payment_stops_no_truncate", table);
    Ok(())
}
