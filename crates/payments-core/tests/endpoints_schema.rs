// Author: Nicola Antonello

//! Database tests for `webhook_endpoints` and `webhook_endpoint_disablements`
//! (ADR-0050, ADR-0055).
//!
//! Rows are written with raw SQL, bypassing the domain types, the way a buggy
//! code path or a manual session could write them. They run as the system role,
//! `payments_app`, unless a test says otherwise. Each rejected write breaks a
//! single rule. The test asserts the SQLSTATE and, where PostgreSQL reports one,
//! the constraint or trigger that rejects the write; privilege errors carry none.

mod common;

use common::payments::{VERSION_4, register_merchant};
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, sqlstate, tenant_login,
};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

/// The address of a merchant's handler, as the server would store it.
const URL: &str = "http://localhost:8080/webhooks";

async fn insert_endpoint(
    conn: &mut PgConnection,
    id: Uuid,
    merchant: Uuid,
    url: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO webhook_endpoints (id, merchant_id, url) VALUES ($1, $2, $3)",
        id,
        merchant,
        url
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn disable(conn: &mut PgConnection, endpoint: Uuid, reason: &str) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason) VALUES ($1, $2)",
        endpoint,
        reason
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[sqlx::test]
async fn an_endpoint_has_a_uuidv7_and_a_known_merchant(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;

    for id in [Uuid::nil(), Uuid::max(), VERSION_4] {
        let err = insert_endpoint(&mut app, id, merchant, URL)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "webhook_endpoints_id_check");
    }
    let err = insert_endpoint(&mut app, Uuid::now_v7(), Uuid::now_v7(), URL)
        .await
        .unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "webhook_endpoints_merchant_fkey",
    );

    let endpoint = Uuid::now_v7();
    insert_endpoint(&mut app, endpoint, merchant, URL).await?;
    let err = insert_endpoint(&mut app, endpoint, merchant, URL)
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "webhook_endpoints_pkey");

    // A merchant may hold several endpoints, even at one URL: moving a URL or changing
    // a secret registers the new endpoint before the old one is disabled (ADR-0050).
    insert_endpoint(&mut app, Uuid::now_v7(), merchant, URL).await?;
    Ok(())
}

#[sqlx::test]
async fn an_endpoint_is_an_absolute_http_url_without_a_credential_or_a_fragment(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;

    for url in [
        URL,
        "http://127.0.0.1:9000",
        "http://[::1]:3000/",
        "http://merchant.test/hooks/payments?source=payments-core",
        "http://merchant.test?source=payments-core",
        // An '@' after the authority belongs to the path, not to user information.
        "http://merchant.test/teams/@payments",
    ] {
        insert_endpoint(&mut app, Uuid::now_v7(), merchant, url).await?;
    }
    for url in [
        // Not http: https waits for the build that adds TLS (ADR-0050), and the server
        // stores the scheme in lowercase.
        "https://merchant.test/webhooks",
        "HTTP://merchant.test/webhooks",
        "ftp://merchant.test/webhooks",
        // Not absolute.
        "/webhooks",
        "merchant.test/webhooks",
        "",
        // No authority.
        "http://",
        "http:///webhooks",
        "http://?source=payments-core",
        // User information, which would store a credential.
        "http://payments:secret@merchant.test/webhooks",
        "http://@merchant.test/webhooks",
        // A fragment.
        "http://merchant.test/webhooks#payments",
        "http://merchant.test#payments",
        // Whitespace or a control character.
        " http://merchant.test/webhooks",
        "http://merchant .test/webhooks",
        "http://merchant.test/web hooks",
        "http://merchant.test/webhooks\n",
        "http://merchant.test/\u{7f}",
    ] {
        let err = insert_endpoint(&mut app, Uuid::now_v7(), merchant, url)
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "webhook_endpoints_url_check");
    }
    Ok(())
}

#[sqlx::test]
async fn a_disablement_names_an_endpoint_once_for_a_known_reason(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let err = disable(&mut app, Uuid::now_v7(), "operator")
        .await
        .unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "webhook_endpoint_disablements_endpoint_fkey",
    );

    let endpoint = Uuid::now_v7();
    insert_endpoint(&mut app, endpoint, merchant, URL).await?;
    for reason in ["", "Operator", "paused", "expired"] {
        let err = disable(&mut app, endpoint, reason).await.unwrap_err();
        assert_violation(
            err,
            CHECK_VIOLATION,
            "webhook_endpoint_disablements_reason_check",
        );
    }
    // The operator's route, a 410, and a delivery that went three days without a
    // success (ADR-0053).
    for reason in ["operator", "gone", "failing"] {
        let other = Uuid::now_v7();
        insert_endpoint(&mut app, other, merchant, URL).await?;
        disable(&mut app, other, reason).await?;
    }

    disable(&mut app, endpoint, "gone").await?;
    let err = disable(&mut app, endpoint, "operator").await.unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "webhook_endpoint_disablements_pkey");

    // How the library will disable one: disabling again changes nothing, whatever
    // the reason, and the first reason stays.
    let done = sqlx::query!(
        "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason) VALUES ($1, $2) \
         ON CONFLICT (endpoint_id) DO NOTHING",
        endpoint,
        "failing"
    )
    .execute(&mut app)
    .await?;
    assert_eq!(done.rows_affected(), 0);
    let reason = sqlx::query_scalar!(
        "SELECT reason FROM webhook_endpoint_disablements WHERE endpoint_id = $1",
        endpoint
    )
    .fetch_one(&mut app)
    .await?;
    assert_eq!(reason, "gone");
    Ok(())
}

#[sqlx::test]
async fn the_system_role_cannot_choose_a_recorded_at(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let endpoint = Uuid::now_v7();
    insert_endpoint(&mut app, endpoint, merchant, URL).await?;
    for err in [
        sqlx::query!(
            "INSERT INTO webhook_endpoints (id, merchant_id, url, recorded_at) \
             VALUES ($1, $2, $3, now())",
            Uuid::now_v7(),
            merchant,
            URL
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO webhook_endpoint_disablements (endpoint_id, reason, recorded_at) \
             VALUES ($1, $2, now())",
            endpoint,
            "operator"
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    Ok(())
}

#[sqlx::test]
async fn a_merchants_role_neither_reads_nor_writes_endpoints(pool: PgPool) -> sqlx::Result<()> {
    // The operator registers a merchant's endpoints (ADR-0050), so a merchant's
    // credential has no privilege on either table, even for its own rows.
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let endpoint = Uuid::now_v7();
    insert_endpoint(&mut app, endpoint, merchant, URL).await?;

    let mut tenant = tenant_login(&pool).await?;
    for err in [
        sqlx::query_scalar!("SELECT count(*) FROM webhook_endpoints")
            .fetch_one(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query_scalar!("SELECT count(*) FROM webhook_endpoint_disablements")
            .fetch_one(&mut tenant)
            .await
            .unwrap_err(),
        insert_endpoint(&mut tenant, Uuid::now_v7(), merchant, URL)
            .await
            .unwrap_err(),
        disable(&mut tenant, endpoint, "operator")
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    tenant.close().await?;
    Ok(())
}

#[sqlx::test]
async fn neither_role_changes_an_endpoint_or_a_disablement(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let endpoint = Uuid::now_v7();
    insert_endpoint(&mut app, endpoint, merchant, URL).await?;
    disable(&mut app, endpoint, "operator").await?;

    let mut tenant = tenant_login(&pool).await?;
    for conn in [&mut app, &mut tenant] {
        for err in [
            sqlx::query!("UPDATE webhook_endpoints SET url = url")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM webhook_endpoints")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE webhook_endpoints CASCADE")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("UPDATE webhook_endpoint_disablements SET reason = reason")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("DELETE FROM webhook_endpoint_disablements")
                .execute(&mut *conn)
                .await
                .unwrap_err(),
            sqlx::query!("TRUNCATE webhook_endpoint_disablements")
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
async fn endpoints_and_disablements_are_append_only_even_for_the_owner(
    pool: PgPool,
) -> sqlx::Result<()> {
    // An endpoint whose URL could change would send a merchant's events elsewhere
    // under the same secret, and a disablement taken back would revive an endpoint
    // whose deliveries ended with it (ADR-0053).
    let mut owner = pool.acquire().await?;
    let merchant = register_merchant(&mut owner).await?;
    let endpoint = Uuid::now_v7();
    insert_endpoint(&mut owner, endpoint, merchant, URL).await?;
    disable(&mut owner, endpoint, "operator").await?;

    let table = "webhook_endpoints";
    let update = sqlx::query!("UPDATE webhook_endpoints SET url = url")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "webhook_endpoints_append_only", table);
    let delete = sqlx::query!("DELETE FROM webhook_endpoints")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "webhook_endpoints_append_only", table);
    // CASCADE, because a foreign key refers to this table: without it, PostgreSQL
    // fails with 0A000 before any trigger runs.
    let truncate = sqlx::query!("TRUNCATE webhook_endpoints CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "webhook_endpoints_no_truncate",
        table,
    );

    let table = "webhook_endpoint_disablements";
    let update = sqlx::query!("UPDATE webhook_endpoint_disablements SET reason = reason")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "webhook_endpoint_disablements_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM webhook_endpoint_disablements")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "webhook_endpoint_disablements_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE webhook_endpoint_disablements")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "webhook_endpoint_disablements_no_truncate",
        table,
    );
    Ok(())
}
