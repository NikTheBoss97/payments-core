// Author: Nicola Antonello

//! Database tests for `payment_requests` and `payment_request_answers` (ADR-0028,
//! ADR-0033, ADR-0037).
//!
//! Rows are written with raw SQL, bypassing the domain types, the way a buggy code
//! path or a manual session could write them: requests as the tenant role,
//! `payments_tenant`, the only role that asks the PSP for something, and answers as
//! the system role, `payments_app`, which runs the sender. Each rejected write
//! breaks a single rule. The test asserts the SQLSTATE and, where PostgreSQL reports
//! one, the constraint or trigger that rejects the write; privilege errors carry
//! none.

mod common;

use common::payments::{NewPayment, Step, VERSION_4, register_merchant};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, runtime_transaction, sqlstate,
    tenant_login, tenant_transaction,
};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

/// The cap of a ledger amount (ADR-0003).
const CAP: i64 = 1_000_000_000_000_000;

/// A committed payment and its merchant.
#[derive(Debug, Clone, Copy)]
struct Paid {
    merchant: Uuid,
    payment: Uuid,
}

/// Registers a merchant and commits a payment of its, with the first step that a
/// committed payment needs, as the system role.
async fn committed_payment(pool: &PgPool) -> sqlx::Result<Paid> {
    let mut tx = runtime_transaction(pool).await?;
    let merchant = register_merchant(&mut tx).await?;
    let payment = NewPayment::valid(merchant);
    payment.insert(&mut tx).await?;
    Step::first(payment.id).insert(&mut tx).await?;
    tx.commit().await?;
    Ok(Paid {
        merchant,
        payment: payment.id,
    })
}

/// A request as the tenant role writes it: every column except `recorded_at`.
#[derive(Debug, Clone, Copy)]
struct Request {
    id: Uuid,
    payment_id: Uuid,
    kind: &'static str,
    amount: i64,
}

impl Request {
    /// A request with a fresh ID. Tests change one field to break one rule.
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

/// The PSP's answer to a send, as the system role writes it.
#[derive(Debug, Clone, Copy)]
struct Answer {
    request_id: Uuid,
    payment_id: Uuid,
    answer: &'static str,
}

impl Answer {
    fn to(request: &Request, answer: &'static str) -> Self {
        Self {
            request_id: request.id,
            payment_id: request.payment_id,
            answer,
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO payment_request_answers (request_id, payment_id, answer) \
             VALUES ($1, $2, $3)",
            self.request_id,
            self.payment_id,
            self.answer
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// How many requests and answers the connection sees.
async fn visible(conn: &mut PgConnection) -> sqlx::Result<(i64, i64)> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM payment_requests) AS "requests!",
                  (SELECT count(*) FROM payment_request_answers) AS "answers!""#
    )
    .fetch_one(conn)
    .await?;
    Ok((row.requests, row.answers))
}

#[sqlx::test]
async fn a_request_has_a_v7_id_a_kind_and_an_amount_for_its_kind(pool: PgPool) -> sqlx::Result<()> {
    let paid = committed_payment(&pool).await?;
    let mut tenant = tenant_transaction(&pool, paid.merchant).await?;
    for request in [
        Request::new(paid.payment, "authorize", CAP),
        Request::new(paid.payment, "capture", 1),
        Request::new(paid.payment, "refund", CAP),
        Request::new(paid.payment, "void", 0),
    ] {
        request.insert(&mut tenant).await?;
    }
    tenant.rollback().await?;

    // PostgreSQL checks a table's CHECKs in the order of their names, so a kind
    // that is not one asks for 1, which the amount's CHECK lets through.
    let capture = Request::new(paid.payment, "capture", 1);
    for (request, constraint) in [
        (
            Request {
                id: VERSION_4,
                ..capture
            },
            "payment_requests_id_check",
        ),
        (
            Request {
                kind: "cancel",
                ..capture
            },
            "payment_requests_kind_check",
        ),
        (
            Request {
                kind: "settle",
                ..capture
            },
            "payment_requests_kind_check",
        ),
        (
            Request {
                kind: "",
                ..capture
            },
            "payment_requests_kind_check",
        ),
        (
            Request::new(paid.payment, "void", 1),
            "payment_requests_amount_check",
        ),
        (
            Request::new(paid.payment, "authorize", 0),
            "payment_requests_amount_check",
        ),
        (
            Request::new(paid.payment, "capture", -1),
            "payment_requests_amount_check",
        ),
        (
            Request::new(paid.payment, "refund", CAP + 1),
            "payment_requests_amount_check",
        ),
    ] {
        let mut tenant = tenant_transaction(&pool, paid.merchant).await?;
        let err = request.insert(&mut tenant).await.unwrap_err();
        assert_violation(err, CHECK_VIOLATION, constraint);
        tenant.rollback().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn a_payment_gets_one_authorization_request(pool: PgPool) -> sqlx::Result<()> {
    let paid = committed_payment(&pool).await?;
    let mut tenant = tenant_transaction(&pool, paid.merchant).await?;
    Request::new(paid.payment, "authorize", 100)
        .insert(&mut tenant)
        .await?;
    tenant.commit().await?;

    let mut tenant = tenant_transaction(&pool, paid.merchant).await?;
    let err = Request::new(paid.payment, "authorize", 100)
        .insert(&mut tenant)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_requests_authorize_key");
    tenant.rollback().await?;

    // What is still in flight is Rust's to judge, under the payment's lock, so the
    // table takes any number of the other kinds.
    let mut tenant = tenant_transaction(&pool, paid.merchant).await?;
    for (kind, amount) in [
        ("capture", 1),
        ("capture", 1),
        ("refund", 1),
        ("refund", 1),
        ("void", 0),
        ("void", 0),
    ] {
        Request::new(paid.payment, kind, amount)
            .insert(&mut tenant)
            .await?;
    }
    tenant.rollback().await?;

    // Another payment of the same merchant gets its own.
    let other = committed_payment(&pool).await?;
    let mut tenant = tenant_transaction(&pool, other.merchant).await?;
    Request::new(other.payment, "authorize", 100)
        .insert(&mut tenant)
        .await?;
    tenant.rollback().await
}

#[sqlx::test]
async fn a_request_has_its_own_id_and_a_payment_that_exists(pool: PgPool) -> sqlx::Result<()> {
    // As the owner, which row-level security does not stop, so that the keys are
    // what refuse.
    let paid = committed_payment(&pool).await?;
    let other = committed_payment(&pool).await?;
    let request = Request::new(paid.payment, "capture", 1);
    let mut owner = pool.acquire().await?;
    request.insert(&mut owner).await?;

    let err = Request {
        payment_id: other.payment,
        ..request
    }
    .insert(&mut owner)
    .await
    .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "payment_requests_pkey");
    let err = Request::new(Uuid::now_v7(), "capture", 1)
        .insert(&mut owner)
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "payment_requests_payment_fkey");
    Ok(())
}

#[sqlx::test]
async fn a_request_gets_one_answer_naming_its_payment(pool: PgPool) -> sqlx::Result<()> {
    let paid = committed_payment(&pool).await?;
    let other = committed_payment(&pool).await?;
    let mut owner = pool.acquire().await?;
    let [held, malformed, third] = [
        Request::new(paid.payment, "capture", 1),
        Request::new(paid.payment, "refund", 1),
        Request::new(paid.payment, "void", 0),
    ];
    for request in [held, malformed, third] {
        request.insert(&mut owner).await?;
    }

    let mut app = runtime_connection(&pool).await?;
    Answer::to(&held, "acknowledged").insert(&mut app).await?;
    Answer::to(&malformed, "malformed").insert(&mut app).await?;

    // A second answer, of either kind.
    for (request, answer) in [(held, "malformed"), (malformed, "acknowledged")] {
        let err = Answer::to(&request, answer)
            .insert(&mut app)
            .await
            .unwrap_err();
        assert_violation(err, UNIQUE_VIOLATION, "payment_request_answers_pkey");
    }
    let err = Answer::to(&third, "declined")
        .insert(&mut app)
        .await
        .unwrap_err();
    assert_violation(err, CHECK_VIOLATION, "payment_request_answers_answer_check");
    // Another payment than the request's, and a request that does not exist.
    for answer in [
        Answer {
            payment_id: other.payment,
            ..Answer::to(&third, "acknowledged")
        },
        Answer {
            request_id: Uuid::now_v7(),
            ..Answer::to(&third, "acknowledged")
        },
    ] {
        let err = answer.insert(&mut app).await.unwrap_err();
        assert_violation(
            err,
            FOREIGN_KEY_VIOLATION,
            "payment_request_answers_request_fkey",
        );
    }
    Ok(())
}

#[sqlx::test]
async fn a_merchant_sees_and_asks_only_about_its_own_payments(pool: PgPool) -> sqlx::Result<()> {
    let a = committed_payment(&pool).await?;
    let b = committed_payment(&pool).await?;
    let of_b = Request::new(b.payment, "capture", 1);
    let mut tenant = tenant_transaction(&pool, b.merchant).await?;
    of_b.insert(&mut tenant).await?;
    tenant.commit().await?;
    let mut app = runtime_connection(&pool).await?;
    Answer::to(&of_b, "acknowledged").insert(&mut app).await?;

    let mut tenant = tenant_transaction(&pool, b.merchant).await?;
    assert_eq!(visible(&mut tenant).await?, (1, 1));
    tenant.rollback().await?;

    // The other merchant sees neither, and cannot ask about the payment: with no
    // WITH CHECK, the policy's USING judges the new row.
    let mut tenant = tenant_transaction(&pool, a.merchant).await?;
    assert_eq!(visible(&mut tenant).await?, (0, 0));
    let err = Request::new(b.payment, "refund", 1)
        .insert(&mut tenant)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    tenant.rollback().await?;

    // Nor does a login that has named no merchant.
    let mut login = tenant_login(&pool).await?;
    assert_eq!(visible(&mut login).await?, (0, 0));
    let err = Request::new(b.payment, "refund", 1)
        .insert(&mut login)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    login.close().await
}

#[sqlx::test]
async fn each_role_writes_only_its_own_rows_and_never_changes_one(
    pool: PgPool,
) -> sqlx::Result<()> {
    let paid = committed_payment(&pool).await?;
    let request = Request::new(paid.payment, "capture", 1);
    let mut owner = pool.acquire().await?;
    request.insert(&mut owner).await?;
    let mut app = runtime_connection(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        // The system role asks the PSP for nothing, and the tenant role records no
        // answer. Neither chooses a recorded_at (ADR-0012).
        Request::new(paid.payment, "refund", 1)
            .insert(&mut app)
            .await
            .unwrap_err(),
        Answer::to(&request, "acknowledged")
            .insert(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_requests (id, payment_id, kind, amount, recorded_at) \
             VALUES ($1, $2, 'refund', 1, now())",
            Uuid::now_v7(),
            paid.payment
        )
        .execute(&mut tenant)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO payment_request_answers (request_id, payment_id, answer, recorded_at) \
             VALUES ($1, $2, 'acknowledged', now())",
            request.id,
            paid.payment
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    for conn in [&mut app, &mut tenant] {
        for err in [
            sqlx::query!("UPDATE payment_requests SET amount = 2")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_requests")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_requests CASCADE")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE payment_request_answers SET answer = 'malformed'")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM payment_request_answers")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE payment_request_answers")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
        ] {
            assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
        }
    }
    tenant.close().await
}

#[sqlx::test]
async fn requests_and_answers_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    // A request that could change would no longer be the one the PSP holds, and an
    // answer that could be taken back would send the request again.
    let paid = committed_payment(&pool).await?;
    let request = Request::new(paid.payment, "capture", 1);
    let mut owner = pool.acquire().await?;
    request.insert(&mut owner).await?;
    Answer::to(&request, "acknowledged")
        .insert(&mut owner)
        .await?;

    let table = "payment_requests";
    let update = sqlx::query!("UPDATE payment_requests SET amount = 2")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "payment_requests_append_only", table);
    let delete = sqlx::query!("DELETE FROM payment_requests")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "payment_requests_append_only", table);
    // CASCADE, because the answers refer to the requests: without it, PostgreSQL
    // fails with 0A000 before any trigger runs. The triggers of the named table fire
    // first, which the trigger's name proves.
    let truncate = sqlx::query!("TRUNCATE payment_requests CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "payment_requests_no_truncate", table);

    let table = "payment_request_answers";
    let update = sqlx::query!("UPDATE payment_request_answers SET answer = 'malformed'")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "payment_request_answers_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM payment_request_answers")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "payment_request_answers_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE payment_request_answers")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "payment_request_answers_no_truncate",
        table,
    );
    Ok(())
}
