// Author: Nicola Antonello

//! Database tests for `api_keys` and `api_key_revocations` (ADR-0024).
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
    assert_guard, assert_violation, runtime_connection, sqlstate,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// A digest of the right length: a secret is never stored, and these are not the
/// digest of anything.
fn digest(n: u8) -> Vec<u8> {
    vec![n; 32]
}

async fn insert_key(
    conn: &mut PgConnection,
    id: Uuid,
    merchant: Uuid,
    digest: &[u8],
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO api_keys (id, merchant_id, digest) VALUES ($1, $2, $3)",
        id,
        merchant,
        digest
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn revoke(conn: &mut PgConnection, key: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO api_key_revocations (api_key_id) VALUES ($1)",
        key
    )
    .execute(conn)
    .await?;
    Ok(())
}

#[sqlx::test]
async fn a_key_has_a_uuidv7_a_known_merchant_and_a_digest_of_its_own(
    pool: PgPool,
) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;

    for id in [Uuid::nil(), Uuid::max(), VERSION_4] {
        let err = insert_key(&mut app, id, merchant, &digest(1))
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "api_keys_id_check");
    }
    let err = insert_key(&mut app, Uuid::now_v7(), Uuid::now_v7(), &digest(1))
        .await
        .unwrap_err();
    assert_violation(err, FOREIGN_KEY_VIOLATION, "api_keys_merchant_fkey");
    for length in [0, 31, 33] {
        let err = insert_key(&mut app, Uuid::now_v7(), merchant, &vec![1; length])
            .await
            .unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "api_keys_digest_check");
    }

    let key = Uuid::now_v7();
    insert_key(&mut app, key, merchant, &digest(1)).await?;
    let err = insert_key(&mut app, key, merchant, &digest(2))
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "api_keys_pkey");
    let err = insert_key(&mut app, Uuid::now_v7(), merchant, &digest(1))
        .await
        .unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "api_keys_digest_key");

    // A merchant may hold several keys, which is how one is rotated.
    insert_key(&mut app, Uuid::now_v7(), merchant, &digest(2)).await?;
    Ok(())
}

#[sqlx::test]
async fn a_revocation_names_a_key_once(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let err = revoke(&mut app, Uuid::now_v7()).await.unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "api_key_revocations_api_key_fkey",
    );

    let key = Uuid::now_v7();
    insert_key(&mut app, key, merchant, &digest(1)).await?;
    revoke(&mut app, key).await?;
    let err = revoke(&mut app, key).await.unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "api_key_revocations_pkey");

    // How the library will revoke one: revoking again changes nothing.
    let done = sqlx::query!(
        "INSERT INTO api_key_revocations (api_key_id) VALUES ($1) \
         ON CONFLICT (api_key_id) DO NOTHING",
        key
    )
    .execute(&mut app)
    .await?;
    assert_eq!(done.rows_affected(), 0);
    Ok(())
}

#[sqlx::test]
async fn the_system_role_cannot_choose_a_recorded_at(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let merchant = register_merchant(&mut app).await?;
    let key = Uuid::now_v7();
    insert_key(&mut app, key, merchant, &digest(1)).await?;
    for err in [
        sqlx::query!(
            "INSERT INTO api_keys (id, merchant_id, digest, recorded_at) \
             VALUES ($1, $2, $3, now())",
            Uuid::now_v7(),
            merchant,
            &digest(2)
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
        sqlx::query!(
            "INSERT INTO api_key_revocations (api_key_id, recorded_at) VALUES ($1, now())",
            key
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
async fn the_system_role_cannot_change_keys_or_revocations(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    for err in [
        sqlx::query!("UPDATE api_keys SET digest = digest")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM api_keys")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE api_keys CASCADE")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("UPDATE api_key_revocations SET api_key_id = api_key_id")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM api_key_revocations")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE api_key_revocations")
            .execute(&mut app)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    Ok(())
}

#[sqlx::test]
async fn keys_and_revocations_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    // A revoked key that could be un-revoked, or a key whose digest could change,
    // would undo the record of who may act as a merchant.
    let mut owner = pool.acquire().await?;
    let merchant = register_merchant(&mut owner).await?;
    let key = Uuid::now_v7();
    insert_key(&mut owner, key, merchant, &digest(1)).await?;
    revoke(&mut owner, key).await?;

    let update = sqlx::query!("UPDATE api_keys SET digest = digest")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "api_keys_append_only", "api_keys");
    let delete = sqlx::query!("DELETE FROM api_keys")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "api_keys_append_only", "api_keys");
    // CASCADE, because a foreign key refers to this table: without it, PostgreSQL
    // fails with 0A000 before any trigger runs.
    let truncate = sqlx::query!("TRUNCATE api_keys CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "api_keys_no_truncate", "api_keys");

    let table = "api_key_revocations";
    let update = sqlx::query!("UPDATE api_key_revocations SET api_key_id = api_key_id")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "api_key_revocations_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM api_key_revocations")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "api_key_revocations_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE api_key_revocations")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "api_key_revocations_no_truncate",
        table,
    );
    Ok(())
}
