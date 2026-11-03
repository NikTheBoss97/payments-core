// Author: Nicola Antonello

//! Row-level security on the real tables (ADR-0025, invariant I7).
//!
//! Rows are written with raw SQL as the tenant role, `payments_tenant`, the way a
//! buggy code path or a manual session could write them, and the fixtures are
//! committed by the system role, `payments_app`, which sees every merchant. A
//! refusal carries no constraint name, so these tests assert the SQLSTATE alone.

mod common;

use common::payments::{
    AUTHORIZED, Books, FEE, NO_GENESIS, NO_RECORD, NewPayment, STEP_COMMIT_CHECK, Step,
    back_to_savepoint, captured_payment, insert_authorization, insert_capture, insert_refund,
    open_account, open_books, refund_step, register_merchant, savepoint, write_posting,
};
use common::{
    CHECK_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION, assert_guard, assert_violation,
    name_merchant, runtime_connection, runtime_transaction, sqlstate, tenant_login,
    tenant_transaction,
};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

/// Two merchants, each with a captured and partly refunded payment, written by
/// the system role. Their books share the platform's accounts.
struct Merchants {
    a: Books,
    a_step: Step,
    b: Books,
    b_step: Step,
}

async fn two_merchants(pool: &PgPool) -> sqlx::Result<Merchants> {
    let mut app = runtime_connection(pool).await?;
    let a = open_books(&mut app).await?;
    let merchant = register_merchant(&mut app).await?;
    let payable = open_account(
        &mut app,
        "merchant_payable",
        &merchant.hyphenated().to_string(),
        "credit",
    )
    .await?;
    let b = Books {
        merchant,
        payable,
        ..a
    };
    let mut tx = runtime_transaction(pool).await?;
    let a_step = captured_payment(&mut tx, &a).await?;
    let a_step = refund_step(&mut tx, &a, &a_step, "captured", 1_000).await?;
    let b_step = captured_payment(&mut tx, &b).await?;
    let b_step = refund_step(&mut tx, &b, &b_step, "captured", 1_000).await?;
    tx.commit().await?;
    Ok(Merchants {
        a,
        a_step,
        b,
        b_step,
    })
}

/// How many rows this session sees in each table that a merchant owns: merchants,
/// payments, steps, authorizations, captures and refunds.
async fn visible(conn: &mut PgConnection) -> sqlx::Result<[i64; 6]> {
    let row = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM merchants) AS "merchants!",
                  (SELECT count(*) FROM payments) AS "payments!",
                  (SELECT count(*) FROM payment_transitions) AS "steps!",
                  (SELECT count(*) FROM payment_authorizations) AS "authorizations!",
                  (SELECT count(*) FROM payment_captures) AS "captures!",
                  (SELECT count(*) FROM payment_refunds) AS "refunds!""#
    )
    .fetch_one(conn)
    .await?;
    Ok([
        row.merchants,
        row.payments,
        row.steps,
        row.authorizations,
        row.captures,
        row.refunds,
    ])
}

/// One merchant's rows: itself, one payment, its four steps and three records.
const ONE_MERCHANT: [i64; 6] = [1, 1, 4, 1, 1, 1];
const NOTHING: [i64; 6] = [0; 6];

#[sqlx::test]
async fn a_merchant_sees_only_its_own_rows(pool: PgPool) -> sqlx::Result<()> {
    let world = two_merchants(&pool).await?;
    for (mine, payment, other_payment) in [
        (
            world.a.merchant,
            world.a_step.payment_id,
            world.b_step.payment_id,
        ),
        (
            world.b.merchant,
            world.b_step.payment_id,
            world.a_step.payment_id,
        ),
    ] {
        let mut tx = tenant_transaction(&pool, mine).await?;
        assert_eq!(visible(&mut tx).await?, ONE_MERCHANT);
        let merchants = sqlx::query_scalar!("SELECT id FROM merchants")
            .fetch_all(&mut *tx)
            .await?;
        assert_eq!(merchants, [mine]);
        let payments = sqlx::query_scalar!("SELECT id FROM payments")
            .fetch_all(&mut *tx)
            .await?;
        assert_eq!(payments, [payment]);
        // Asking by ID finds nothing either, in any table.
        let row = sqlx::query!(
            r#"SELECT (SELECT count(*) FROM payments WHERE id = $1) AS "payments!",
                      (SELECT count(*) FROM payment_transitions WHERE payment_id = $1) AS "steps!",
                      (SELECT count(*) FROM payment_authorizations WHERE payment_id = $1)
                          AS "authorizations!",
                      (SELECT count(*) FROM payment_captures WHERE payment_id = $1)
                          AS "captures!",
                      (SELECT count(*) FROM payment_refunds WHERE payment_id = $1)
                          AS "refunds!""#,
            other_payment
        )
        .fetch_one(&mut *tx)
        .await?;
        assert_eq!(
            [
                row.payments,
                row.steps,
                row.authorizations,
                row.captures,
                row.refunds
            ],
            [0; 5]
        );
        tx.rollback().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn the_system_role_sees_every_merchant(pool: PgPool) -> sqlx::Result<()> {
    let _world = two_merchants(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    assert_eq!(visible(&mut app).await?, [2, 2, 8, 2, 2, 2]);
    Ok(())
}

#[sqlx::test]
async fn with_no_merchant_named_nothing_a_merchant_owns_is_visible(
    pool: PgPool,
) -> sqlx::Result<()> {
    let _world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    assert_eq!(visible(&mut tenant).await?, NOTHING);

    // What stays outside the policies: nothing here says whose it is, and no route
    // reads it (ADR-0025).
    let shared = sqlx::query!(
        r#"SELECT (SELECT count(*) FROM accounts) AS "accounts!",
                  (SELECT count(*) FROM ledger_transactions) AS "postings!""#
    )
    .fetch_one(&mut tenant)
    .await?;
    assert_eq!((shared.accounts, shared.postings), (4, 4));
    tenant.close().await
}

#[sqlx::test]
async fn with_no_merchant_named_nothing_can_be_written(pool: PgPool) -> sqlx::Result<()> {
    let world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    let err = NewPayment::valid(world.a.merchant)
        .insert_rejected(&mut tenant)
        .await;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    let err = world
        .a_step
        .then("settle", "settled", 0, world.a_step.refundable)
        .insert_rejected(&mut tenant)
        .await;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    tenant.close().await
}

#[sqlx::test]
async fn a_session_forgets_its_merchant_when_the_transaction_ends(
    pool: PgPool,
) -> sqlx::Result<()> {
    // What a pooled connection goes through: the setting goes with the transaction,
    // whether it commits or not. It is then '' rather than NULL, which a bare ::uuid
    // in a policy would turn into an error on every later statement.
    let world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;

    let mut tx = tenant.begin().await?;
    name_merchant(&mut tx, world.a.merchant).await?;
    assert_eq!(visible(&mut tx).await?, ONE_MERCHANT);
    tx.commit().await?;
    assert_eq!(visible(&mut tenant).await?, NOTHING);

    let mut tx = tenant.begin().await?;
    name_merchant(&mut tx, world.b.merchant).await?;
    assert_eq!(visible(&mut tx).await?, ONE_MERCHANT);
    tx.rollback().await?;
    assert_eq!(visible(&mut tenant).await?, NOTHING);
    tenant.close().await
}

#[sqlx::test]
async fn one_prepared_statement_serves_each_merchant(pool: PgPool) -> sqlx::Result<()> {
    // sqlx caches a statement per connection, and PostgreSQL may reuse its plan. A
    // generic plan must still read the setting each time it runs.
    let world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    sqlx::query!("SET plan_cache_mode = force_generic_plan")
        .execute(&mut tenant)
        .await?;
    for (merchant, payment) in [
        (world.a.merchant, world.a_step.payment_id),
        (world.b.merchant, world.b_step.payment_id),
        (world.a.merchant, world.a_step.payment_id),
    ] {
        let mut tx = tenant.begin().await?;
        name_merchant(&mut tx, merchant).await?;
        let steps = sqlx::query_scalar!(
            r#"SELECT DISTINCT payment_id AS "payment!" FROM payment_transitions"#
        )
        .fetch_all(&mut *tx)
        .await?;
        assert_eq!(steps, [payment]);
        tx.commit().await?;
    }
    tenant.close().await
}

#[sqlx::test]
async fn a_merchant_cannot_write_for_another(pool: PgPool) -> sqlx::Result<()> {
    let world = two_merchants(&pool).await?;
    let stranger = &world.b_step;
    let mut tx = tenant_transaction(&pool, world.a.merchant).await?;
    let posting = write_posting(&mut tx, &world.b.refund_lines(1_000)).await?;

    savepoint(&mut tx).await?;
    let err = NewPayment::valid(world.b.merchant)
        .insert_rejected(&mut tx)
        .await;
    back_to_savepoint(&mut tx).await?;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);

    let next = stranger.then("settle", "settled", 0, stranger.refundable);
    savepoint(&mut tx).await?;
    let err = next.insert_rejected(&mut tx).await;
    back_to_savepoint(&mut tx).await?;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);

    savepoint(&mut tx).await?;
    let err = insert_authorization(&mut tx, stranger, 10_000)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);

    savepoint(&mut tx).await?;
    let err = insert_capture(&mut tx, stranger, AUTHORIZED, FEE, posting)
        .await
        .unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);

    savepoint(&mut tx).await?;
    let err = insert_refund(&mut tx, stranger, posting).await.unwrap_err();
    back_to_savepoint(&mut tx).await?;
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    tx.rollback().await
}

#[sqlx::test]
async fn a_whole_lifecycle_commits_as_the_tenant(pool: PgPool) -> sqlx::Result<()> {
    // The deferred checks run as the tenant role, and read through the policies at
    // COMMIT, so a check that lost sight of a row would refuse this.
    let world = two_merchants(&pool).await?;
    let mut tx = tenant_transaction(&pool, world.a.merchant).await?;
    let captured = captured_payment(&mut tx, &world.a).await?;
    refund_step(&mut tx, &world.a, &captured, "captured", 2_000).await?;
    tx.commit().await?;

    let mut tx = tenant_transaction(&pool, world.a.merchant).await?;
    assert_eq!(visible(&mut tx).await?, [1, 2, 8, 2, 2, 2]);
    tx.rollback().await
}

#[sqlx::test]
async fn the_checks_at_commit_still_refuse_what_the_tenant_writes(
    pool: PgPool,
) -> sqlx::Result<()> {
    // Row-level security must not blind a deferred check: each of these commits
    // would otherwise leave a broken row for the merchant's own payment.
    let world = two_merchants(&pool).await?;
    let merchant = world.a.merchant;

    let mut tx = tenant_transaction(&pool, merchant).await?;
    NewPayment::valid(merchant).insert(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_GENESIS, "payments_commit_check", "payments");

    let mut tx = tenant_transaction(&pool, merchant).await?;
    let settled = world
        .a_step
        .then("settle", "settled", 0, world.a_step.refundable);
    settled.insert(&mut tx).await?;
    let recordless = settled.then("refund", "settled", 1_000, settled.refundable - 1_000);
    recordless.insert(&mut tx).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(err, NO_RECORD, STEP_COMMIT_CHECK, "payment_transitions");

    let mut tx = tenant_transaction(&pool, merchant).await?;
    let refund = world
        .a_step
        .then("refund", "captured", 1_000, world.a_step.refundable - 1_000);
    refund.insert(&mut tx).await?;
    let wrong = write_posting(&mut tx, &world.a.refund_lines(999)).await?;
    insert_refund(&mut tx, &refund, wrong).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(
        err,
        "LG006",
        "payment_refunds_commit_check",
        "payment_refunds",
    );

    let mut tx = tenant_transaction(&pool, merchant).await?;
    write_posting(&mut tx, &[(world.a.payable, 1), (world.a.fees, 2)]).await?;
    let err = tx.commit().await.unwrap_err();
    assert_guard(
        err,
        "LG003",
        "ledger_transactions_commit_check",
        "ledger_transactions",
    );
    Ok(())
}

#[sqlx::test]
async fn a_constraint_still_answers_for_a_row_the_tenant_may_write(
    pool: PgPool,
) -> sqlx::Result<()> {
    // The policies add to the constraints and replace none of them.
    let world = two_merchants(&pool).await?;
    let mut tx = tenant_transaction(&pool, world.a.merchant).await?;
    let err = NewPayment {
        amount: 0,
        ..NewPayment::valid(world.a.merchant)
    }
    .insert_rejected(&mut tx)
    .await;
    assert_violation(err, CHECK_VIOLATION, "payments_amount_check");
    tx.rollback().await
}

#[sqlx::test]
async fn a_key_collision_reveals_a_row_the_tenant_cannot_see(pool: PgPool) -> sqlx::Result<()> {
    // The manual's covert channel, pinned so that nothing relies on the opposite. IDs
    // are made by the server, so a merchant cannot choose one to probe with (ADR-0025).
    let world = two_merchants(&pool).await?;
    let mut tx = tenant_transaction(&pool, world.a.merchant).await?;
    let err = NewPayment {
        id: world.b_step.payment_id,
        ..NewPayment::valid(world.a.merchant)
    }
    .insert_rejected(&mut tx)
    .await;
    assert_violation(err, UNIQUE_VIOLATION, "payments_pkey");
    tx.rollback().await
}

#[sqlx::test]
async fn the_tenant_role_cannot_touch_what_is_not_a_merchants(pool: PgPool) -> sqlx::Result<()> {
    let world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        // Only the operator creates a merchant (ADR-0024).
        sqlx::query!(
            "INSERT INTO merchants (id, fee_bps) VALUES ($1, 0)",
            Uuid::now_v7()
        )
        .execute(&mut tenant)
        .await
        .unwrap_err(),
        sqlx::query!("SELECT id FROM api_keys")
            .fetch_all(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!(
            "INSERT INTO api_keys (id, merchant_id, digest) VALUES ($1, $2, $3)",
            Uuid::now_v7(),
            world.a.merchant,
            &[0_u8; 32][..]
        )
        .execute(&mut tenant)
        .await
        .unwrap_err(),
        sqlx::query!("SELECT api_key_id FROM api_key_revocations")
            .fetch_all(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("UPDATE payments SET amount = 1")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM payment_transitions")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE payment_refunds")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    tenant.close().await
}

#[sqlx::test]
async fn the_tenant_role_cannot_get_out(pool: PgPool) -> sqlx::Result<()> {
    let _world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        sqlx::query!("SET ROLE payments_app")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("SET SESSION AUTHORIZATION payments_app")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("ALTER TABLE payments DISABLE ROW LEVEL SECURITY")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("DROP POLICY payments_tenant_isolation ON payments")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("ALTER ROLE payments_tenant BYPASSRLS")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }

    // Turning the setting off bypasses nothing: a query that a policy would filter
    // raises an error instead.
    sqlx::query!("SET row_security = off")
        .execute(&mut tenant)
        .await?;
    let err = sqlx::query!("SELECT id FROM payments")
        .fetch_all(&mut tenant)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    tenant.close().await
}

#[sqlx::test]
async fn the_known_limit_the_role_may_name_any_merchant(pool: PgPool) -> sqlx::Result<()> {
    // Pinned so that the claim stays honest: the policies guard against a forgotten
    // or wrong merchant, not against SQL written on purpose to cross over. Nothing
    // can tell this session from one that merchant B opened (ADR-0025).
    let world = two_merchants(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    let mut tx = tenant.begin().await?;
    name_merchant(&mut tx, world.b.merchant).await?;
    let payments = sqlx::query_scalar!("SELECT id FROM payments")
        .fetch_all(&mut *tx)
        .await?;
    assert_eq!(payments, [world.b_step.payment_id]);
    tx.rollback().await?;
    tenant.close().await
}
