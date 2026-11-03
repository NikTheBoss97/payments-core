// Author: Nicola Antonello

//! Database tests for `idempotency_records` and `operator_idempotency_records`
//! (ADR-0026).
//!
//! Rows are written with raw SQL, bypassing the domain types, the way a buggy
//! code path or a manual session could write them: a merchant's records as the
//! tenant role, `payments_tenant`, and the operator's as the system role,
//! `payments_app`. Each rejected write breaks a single rule. The test asserts the
//! SQLSTATE and, where PostgreSQL reports one, the constraint or trigger that
//! rejects the write; privilege errors carry none.

mod common;

use common::payments::register_merchant;
use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE, UNIQUE_VIOLATION,
    assert_guard, assert_violation, runtime_connection, sqlstate, tenant_login, tenant_transaction,
    wait_for_lock_wait,
};
use futures_util::future::join;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

const BODY: &str = r#"{"seq":1}"#;

/// A digest of the right length, which is not the digest of anything.
fn fingerprint(n: u8) -> Vec<u8> {
    vec![n; 32]
}

/// A merchant's record, with one field to change per test.
#[derive(Debug, Clone, Copy)]
struct Record<'a> {
    merchant: Uuid,
    key: &'a str,
    fingerprint: &'a [u8],
    status: i16,
    body: &'a str,
}

impl Record<'_> {
    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO idempotency_records (merchant_id, key, fingerprint, status, body) \
             VALUES ($1, $2, $3, $4, $5)",
            self.merchant,
            self.key,
            self.fingerprint,
            self.status,
            self.body
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    async fn insert_as_operator(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO operator_idempotency_records (key, fingerprint, status, body) \
             VALUES ($1, $2, $3, $4)",
            self.key,
            self.fingerprint,
            self.status,
            self.body
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// A registered merchant, written by the system role.
async fn merchant(pool: &PgPool) -> sqlx::Result<Uuid> {
    let mut app = runtime_connection(pool).await?;
    register_merchant(&mut app).await
}

/// The first key character and the last, either side of each hole in the range.
const GOOD_KEYS: [&str; 8] = [
    "!",
    "#",
    "[",
    "]",
    "~",
    "8e03978e-40d5-43e8-bc93-6894a57f9324",
    "a.b_c-d/e+f=g",
    "KG5LxwFBepaKHyUD",
];
/// A space, each excluded character, either end of the range, and what is outside.
const BAD_KEYS: [&str; 7] = ["", " ", "\"", "\\", "\u{7f}", "é", "a b"];

#[sqlx::test]
async fn a_key_is_visible_ascii_without_quotes_or_backslashes(pool: PgPool) -> sqlx::Result<()> {
    let merchant = merchant(&pool).await?;
    let digest = fingerprint(1);
    let at_the_limit = "k".repeat(255);
    let over_the_limit = "k".repeat(256);
    let new_line = "a\nb";
    let good = GOOD_KEYS.iter().copied().chain([at_the_limit.as_str()]);
    let bad = BAD_KEYS
        .iter()
        .copied()
        .chain([over_the_limit.as_str(), new_line]);

    let mut tenant = tenant_transaction(&pool, merchant).await?;
    let mut app = runtime_connection(&pool).await?;
    for key in good {
        let record = Record {
            merchant,
            key,
            fingerprint: &digest,
            status: 200,
            body: BODY,
        };
        record.insert(&mut tenant).await?;
        record.insert_as_operator(&mut app).await?;
    }
    tenant.rollback().await?;
    for key in bad {
        let record = Record {
            merchant,
            key,
            fingerprint: &digest,
            status: 200,
            body: BODY,
        };
        let mut tenant = tenant_transaction(&pool, merchant).await?;
        let err = record.insert(&mut tenant).await.unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "idempotency_records_key_check");
        tenant.rollback().await?;
        let err = record.insert_as_operator(&mut app).await.unwrap_err();
        assert_violation(
            err,
            CHECK_VIOLATION,
            "operator_idempotency_records_key_check",
        );
    }
    Ok(())
}

#[sqlx::test]
async fn a_record_holds_a_digest_a_final_status_and_json(pool: PgPool) -> sqlx::Result<()> {
    let merchant = merchant(&pool).await?;
    let digest = fingerprint(1);
    let valid = Record {
        merchant,
        key: "k",
        fingerprint: &digest,
        status: 200,
        body: BODY,
    };
    let mut app = runtime_connection(&pool).await?;

    for length in [0, 31, 33] {
        let wrong = vec![1; length];
        let record = Record {
            fingerprint: &wrong,
            ..valid
        };
        let mut tenant = tenant_transaction(&pool, merchant).await?;
        let err = record.insert(&mut tenant).await.unwrap_err();
        assert_violation(
            err,
            CHECK_VIOLATION,
            "idempotency_records_fingerprint_check",
        );
        tenant.rollback().await?;
        let err = record.insert_as_operator(&mut app).await.unwrap_err();
        assert_violation(
            err,
            CHECK_VIOLATION,
            "operator_idempotency_records_fingerprint_check",
        );
    }
    // A 5xx would record an outcome that is not known, and below 200 nothing is final.
    for status in [0, 199, 500, 503] {
        let record = Record { status, ..valid };
        let mut tenant = tenant_transaction(&pool, merchant).await?;
        let err = record.insert(&mut tenant).await.unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "idempotency_records_status_check");
        tenant.rollback().await?;
        let err = record.insert_as_operator(&mut app).await.unwrap_err();
        assert_violation(
            err,
            CHECK_VIOLATION,
            "operator_idempotency_records_status_check",
        );
    }
    for body in ["", "not json", "{"] {
        let record = Record { body, ..valid };
        let mut tenant = tenant_transaction(&pool, merchant).await?;
        let err = record.insert(&mut tenant).await.unwrap_err();
        assert_violation(err, CHECK_VIOLATION, "idempotency_records_body_check");
        tenant.rollback().await?;
        let err = record.insert_as_operator(&mut app).await.unwrap_err();
        assert_violation(
            err,
            CHECK_VIOLATION,
            "operator_idempotency_records_body_check",
        );
    }

    let mut tenant = tenant_transaction(&pool, merchant).await?;
    for (key, status) in [("first", 200), ("last", 499)] {
        Record {
            key,
            status,
            ..valid
        }
        .insert(&mut tenant)
        .await?;
    }
    tenant.rollback().await
}

#[sqlx::test]
async fn a_body_comes_back_byte_for_byte(pool: PgPool) -> sqlx::Result<()> {
    // A replay returns what the first answer returned, so its layout and the order
    // of its keys must survive, which jsonb would not keep.
    let merchant = merchant(&pool).await?;
    let digest = fingerprint(1);
    let body = "{ \"z\" : 1,  \"a\" : [ 2 ] }";
    let record = Record {
        merchant,
        key: "k",
        fingerprint: &digest,
        status: 200,
        body,
    };
    let mut tenant = tenant_transaction(&pool, merchant).await?;
    record.insert(&mut tenant).await?;
    let stored = sqlx::query_scalar!("SELECT body FROM idempotency_records WHERE key = 'k'")
        .fetch_one(&mut *tenant)
        .await?;
    assert_eq!(stored, body);
    tenant.rollback().await
}

#[sqlx::test]
async fn a_key_answers_once_per_merchant(pool: PgPool) -> sqlx::Result<()> {
    let a = merchant(&pool).await?;
    let b = merchant(&pool).await?;
    let digest = fingerprint(1);
    let of_a = Record {
        merchant: a,
        key: "k",
        fingerprint: &digest,
        status: 200,
        body: BODY,
    };
    let mut tenant = tenant_transaction(&pool, a).await?;
    of_a.insert(&mut tenant).await?;
    tenant.commit().await?;

    let mut tenant = tenant_transaction(&pool, a).await?;
    let err = of_a.insert(&mut tenant).await.unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "idempotency_records_pkey");
    tenant.rollback().await?;

    // Another merchant's key is another key.
    let mut tenant = tenant_transaction(&pool, b).await?;
    Record {
        merchant: b,
        ..of_a
    }
    .insert(&mut tenant)
    .await?;
    tenant.rollback().await?;

    // A merchant that does not exist, which only a wrong setting could name.
    let nobody = Uuid::now_v7();
    let mut tenant = tenant_transaction(&pool, nobody).await?;
    let err = Record {
        merchant: nobody,
        ..of_a
    }
    .insert(&mut tenant)
    .await
    .unwrap_err();
    assert_violation(
        err,
        FOREIGN_KEY_VIOLATION,
        "idempotency_records_merchant_fkey",
    );
    tenant.rollback().await?;

    // The operator's keys are apart from every merchant's.
    let mut app = runtime_connection(&pool).await?;
    of_a.insert_as_operator(&mut app).await?;
    let err = of_a.insert_as_operator(&mut app).await.unwrap_err();
    assert_violation(err, UNIQUE_VIOLATION, "operator_idempotency_records_pkey");
    Ok(())
}

#[sqlx::test]
async fn a_merchant_sees_and_writes_only_its_own_records(pool: PgPool) -> sqlx::Result<()> {
    let a = merchant(&pool).await?;
    let b = merchant(&pool).await?;
    let digest = fingerprint(1);
    let of_b = Record {
        merchant: b,
        key: "of-b",
        fingerprint: &digest,
        status: 200,
        body: BODY,
    };
    let mut tenant = tenant_transaction(&pool, b).await?;
    of_b.insert(&mut tenant).await?;
    tenant.commit().await?;

    let mut tenant = tenant_transaction(&pool, a).await?;
    let seen = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM idempotency_records"#)
        .fetch_one(&mut *tenant)
        .await?;
    assert_eq!(seen, 0);
    let err = Record { key: "new", ..of_b }
        .insert(&mut tenant)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    tenant.rollback().await?;

    let mut login = tenant_login(&pool).await?;
    let seen = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM idempotency_records"#)
        .fetch_one(&mut login)
        .await?;
    assert_eq!(seen, 0);
    let err = Record { key: "new", ..of_b }
        .insert(&mut login)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    login.close().await
}

#[sqlx::test]
async fn each_role_touches_only_its_own_table(pool: PgPool) -> sqlx::Result<()> {
    let merchant = merchant(&pool).await?;
    let digest = fingerprint(1);
    let record = Record {
        merchant,
        key: "k",
        fingerprint: &digest,
        status: 200,
        body: BODY,
    };
    let mut app = runtime_connection(&pool).await?;
    let mut tenant = tenant_login(&pool).await?;
    for err in [
        record.insert(&mut app).await.unwrap_err(),
        sqlx::query!("SELECT key FROM idempotency_records")
            .fetch_all(&mut app)
            .await
            .unwrap_err(),
        record.insert_as_operator(&mut tenant).await.unwrap_err(),
        sqlx::query!("SELECT key FROM operator_idempotency_records")
            .fetch_all(&mut tenant)
            .await
            .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    tenant.close().await
}

/// How a key's second attempt ends, once the first has ended this way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstAttempt {
    Commits,
    RollsBack,
}

async fn second_attempt_behind_a_first_that(ends: FirstAttempt, pool: &PgPool) -> sqlx::Result<()> {
    let merchant = merchant(pool).await?;
    let (first_print, second_print) = (fingerprint(1), fingerprint(2));
    let first = Record {
        merchant,
        key: "k",
        fingerprint: &first_print,
        status: 200,
        body: BODY,
    };
    let second = Record {
        fingerprint: &second_print,
        ..first
    };
    let mut holder = tenant_transaction(pool, merchant).await?;
    first.insert(&mut holder).await?;

    // The second attempt waits on the first's row, in the unique index. Then what it
    // finds is what the first attempt left.
    let attempt = async {
        let mut tx = tenant_transaction(pool, merchant).await?;
        let outcome = second.insert(&mut tx).await;
        match &outcome {
            Ok(()) => tx.commit().await?,
            Err(_) => tx.rollback().await?,
        }
        Ok::<_, sqlx::Error>(outcome)
    };
    let end_the_first = async {
        wait_for_lock_wait(pool, "transactionid").await?;
        match ends {
            FirstAttempt::Commits => holder.commit().await,
            FirstAttempt::RollsBack => holder.rollback().await,
        }
    };
    let (attempted, ended) = join(attempt, end_the_first).await;
    ended?;

    let mut tx = tenant_transaction(pool, merchant).await?;
    let stored = sqlx::query_scalar!("SELECT fingerprint FROM idempotency_records")
        .fetch_all(&mut *tx)
        .await?;
    tx.rollback().await?;
    match ends {
        FirstAttempt::Commits => {
            assert_violation(
                attempted?.unwrap_err(),
                UNIQUE_VIOLATION,
                "idempotency_records_pkey",
            );
            assert_eq!(stored, [first_print]);
        }
        FirstAttempt::RollsBack => {
            attempted??;
            assert_eq!(stored, [second_print]);
        }
    }
    Ok(())
}

#[sqlx::test]
async fn an_attempt_waits_for_the_first_and_loses_if_it_committed(
    pool: PgPool,
) -> sqlx::Result<()> {
    second_attempt_behind_a_first_that(FirstAttempt::Commits, &pool).await
}

#[sqlx::test]
async fn an_attempt_waits_for_the_first_and_wins_if_it_rolled_back(
    pool: PgPool,
) -> sqlx::Result<()> {
    second_attempt_behind_a_first_that(FirstAttempt::RollsBack, &pool).await
}

#[sqlx::test]
async fn the_roles_cannot_change_a_record_or_choose_its_recorded_at(
    pool: PgPool,
) -> sqlx::Result<()> {
    let merchant = merchant(&pool).await?;
    let digest = fingerprint(1);
    let mut tenant = tenant_login(&pool).await?;
    let mut app = runtime_connection(&pool).await?;
    for err in [
        sqlx::query!("UPDATE idempotency_records SET body = '{}'")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM idempotency_records")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE idempotency_records")
            .execute(&mut tenant)
            .await
            .unwrap_err(),
        sqlx::query!(
            "INSERT INTO idempotency_records \
                 (merchant_id, key, fingerprint, status, body, recorded_at) \
             VALUES ($1, 'k', $2, 200, '{}', now())",
            merchant,
            &digest
        )
        .execute(&mut tenant)
        .await
        .unwrap_err(),
        sqlx::query!("UPDATE operator_idempotency_records SET body = '{}'")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("DELETE FROM operator_idempotency_records")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!("TRUNCATE operator_idempotency_records")
            .execute(&mut app)
            .await
            .unwrap_err(),
        sqlx::query!(
            "INSERT INTO operator_idempotency_records (key, fingerprint, status, body, recorded_at) \
             VALUES ('k', $1, 200, '{}', now())",
            &digest
        )
        .execute(&mut app)
        .await
        .unwrap_err(),
    ] {
        assert_eq!(sqlstate(err), INSUFFICIENT_PRIVILEGE);
    }
    tenant.close().await
}

#[sqlx::test]
async fn records_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    // An answer that could change would no longer be the one the client saw.
    let merchant = merchant(&pool).await?;
    let digest = fingerprint(1);
    let record = Record {
        merchant,
        key: "k",
        fingerprint: &digest,
        status: 200,
        body: BODY,
    };
    let mut owner = pool.acquire().await?;
    record.insert(&mut owner).await?;
    record.insert_as_operator(&mut owner).await?;

    let table = "idempotency_records";
    let update = sqlx::query!("UPDATE idempotency_records SET body = '{}'")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "idempotency_records_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM idempotency_records")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "idempotency_records_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE idempotency_records")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "idempotency_records_no_truncate",
        table,
    );

    let table = "operator_idempotency_records";
    let update = sqlx::query!("UPDATE operator_idempotency_records SET body = '{}'")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        update,
        APPEND_ONLY,
        "operator_idempotency_records_append_only",
        table,
    );
    let delete = sqlx::query!("DELETE FROM operator_idempotency_records")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        delete,
        APPEND_ONLY,
        "operator_idempotency_records_append_only",
        table,
    );
    let truncate = sqlx::query!("TRUNCATE operator_idempotency_records")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(
        truncate,
        APPEND_ONLY,
        "operator_idempotency_records_no_truncate",
        table,
    );
    Ok(())
}
