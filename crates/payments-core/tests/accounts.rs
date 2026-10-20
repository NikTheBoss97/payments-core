// Author: Nicola Antonello

//! Database tests for the `accounts` table (ADR-0009, ADR-0012, ADR-0004).
//!
//! Writes run as the runtime role, `payments_app`, which meets the same
//! guards as the application, unless a test says otherwise. Each rejected
//! write breaks a single rule of an otherwise valid account. The test
//! asserts the SQLSTATE and, where PostgreSQL reports one, the constraint or
//! trigger that rejects the write; privilege errors and NOT NULL violations
//! carry none.

mod common;

use common::{
    APPEND_ONLY, CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, INSUFFICIENT_PRIVILEGE,
    NOT_NULL_VIOLATION, UNIQUE_VIOLATION, assert_guard, assert_violation, runtime_connection,
    sqlstate,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// An account as the runtime role writes it: every column except
/// `recorded_at`, which only the database sets (ADR-0012).
#[derive(Debug, Clone, Copy)]
struct NewAccount<'a> {
    id: Uuid,
    kind: &'a str,
    party: &'a str,
    currency: &'a str,
    normal_side: &'a str,
}

impl NewAccount<'_> {
    /// A valid account with a fresh ID. Tests change one field to break one
    /// rule.
    fn valid() -> Self {
        Self {
            id: Uuid::now_v7(),
            kind: "fee_revenue",
            party: "platform",
            currency: "EUR",
            normal_side: "credit",
        }
    }

    async fn insert(&self, conn: &mut PgConnection) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO accounts (id, kind, party, currency, normal_side) \
             VALUES ($1, $2, $3, $4, $5)",
            self.id,
            self.kind,
            self.party,
            self.currency,
            self.normal_side
        )
        .execute(conn)
        .await?;
        Ok(())
    }

    /// Inserts the account and returns the error, failing the test if the
    /// insert succeeds.
    async fn insert_rejected(&self, conn: &mut PgConnection) -> sqlx::Error {
        self.insert(conn)
            .await
            .err()
            .unwrap_or_else(|| panic!("{self:?} was accepted"))
    }
}

async fn account_count(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM accounts"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn creating_an_account_twice_keeps_the_first(pool: PgPool) -> sqlx::Result<()> {
    // Get-or-create (ADR-0009): the insert does nothing when the key exists,
    // and a separate statement reads the ID back by key.
    let mut app = runtime_connection(&pool).await?;
    let first = NewAccount::valid();
    let retry = NewAccount {
        id: Uuid::now_v7(),
        ..first
    };
    for (account, expected_rows) in [(first, 1), (retry, 0)] {
        let inserted = sqlx::query!(
            "INSERT INTO accounts (id, kind, party, currency, normal_side) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (kind, party, currency) DO NOTHING",
            account.id,
            account.kind,
            account.party,
            account.currency,
            account.normal_side
        )
        .execute(&mut app)
        .await?;
        assert_eq!(inserted.rows_affected(), expected_rows);

        let id = sqlx::query_scalar!(
            "SELECT id FROM accounts WHERE kind = $1 AND party = $2 AND currency = $3",
            account.kind,
            account.party,
            account.currency
        )
        .fetch_one(&mut app)
        .await?;
        assert_eq!(id, first.id);
    }
    assert_eq!(account_count(&pool).await?, 1);
    Ok(())
}

#[sqlx::test]
async fn the_business_key_is_unique(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let first = NewAccount::valid();
    first.insert(&mut app).await?;

    let duplicate = NewAccount {
        id: Uuid::now_v7(),
        ..first
    };
    let err = duplicate.insert_rejected(&mut app).await;
    assert_violation(err, UNIQUE_VIOLATION, "accounts_kind_party_currency_key");

    // Changing any one column of the key makes another account.
    let other_kind = NewAccount {
        kind: "cash",
        ..NewAccount::valid()
    };
    let other_party = NewAccount {
        party: "mockpsp",
        ..NewAccount::valid()
    };
    let other_currency = NewAccount {
        currency: "USD",
        ..NewAccount::valid()
    };
    for account in [other_kind, other_party, other_currency] {
        account.insert(&mut app).await?;
    }
    assert_eq!(account_count(&pool).await?, 4);
    Ok(())
}

#[sqlx::test]
async fn an_id_names_one_account(pool: PgPool) -> sqlx::Result<()> {
    // UNIQUE (id, currency), the target of the entries' key, would let one ID
    // name an account in each currency, and a balance read by account_id
    // (ADR-0002) would add their amounts together. Only the primary key
    // stops that.
    let mut app = runtime_connection(&pool).await?;
    let first = NewAccount::valid();
    first.insert(&mut app).await?;

    // A new business key in a new currency, so the ID is the only rule broken.
    let same_id = NewAccount {
        currency: "USD",
        ..first
    };
    let err = same_id.insert_rejected(&mut app).await;
    assert_violation(err, UNIQUE_VIOLATION, "accounts_pkey");
    assert_eq!(account_count(&pool).await?, 1);
    Ok(())
}

#[sqlx::test]
async fn kinds_are_lowercase_names(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let too_long = "a".repeat(64);
    for kind in [
        "",
        "Cash",
        "1cash",
        "_cash",
        "fee-revenue",
        "fee revenue",
        too_long.as_str(),
    ] {
        let account = NewAccount {
            kind,
            ..NewAccount::valid()
        };
        let err = account.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, "accounts_kind_check");
    }

    let longest = "a".repeat(63);
    for kind in ["cash", "fee_revenue", "psp2_receivable", longest.as_str()] {
        let account = NewAccount {
            kind,
            ..NewAccount::valid()
        };
        account.insert(&mut app).await?;
    }
    assert_eq!(account_count(&pool).await?, 4);
    Ok(())
}

#[sqlx::test]
async fn each_party_has_one_spelling(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    // A merchant ID that begins with a letter: without its hyphens it would
    // look like a label, so only the rule against 32 hex digits rejects it.
    let merchant = uuid::uuid!("d1b2c3d4-e5f6-7a7b-8c9d-0e1f2a3b4c5d");
    let canonical = merchant.to_string();
    let simple = merchant.simple().to_string();
    let uppercase = canonical.to_uppercase();
    let braced = merchant.braced().to_string();
    let prefixed = format!("acme_{canonical}");
    let suffixed = format!("{canonical}_x");
    let too_long = "a".repeat(64);
    for party in [
        "",
        "Platform",
        "mock-psp",
        "mock psp",
        "1psp",
        "_psp",
        too_long.as_str(),
        simple.as_str(),
        uppercase.as_str(),
        braced.as_str(),
        prefixed.as_str(),
        suffixed.as_str(),
    ] {
        let account = NewAccount {
            party,
            ..NewAccount::valid()
        };
        let err = account.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, "accounts_party_check");
    }

    // A label, or a UUID exactly as Rust's Display prints it.
    let longest = "a".repeat(63);
    for party in [
        "platform",
        "mock_psp",
        "psp2",
        longest.as_str(),
        canonical.as_str(),
    ] {
        let account = NewAccount {
            party,
            ..NewAccount::valid()
        };
        account.insert(&mut app).await?;
    }
    assert_eq!(account_count(&pool).await?, 5);
    Ok(())
}

#[sqlx::test]
async fn the_normal_side_is_debit_or_credit(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    for normal_side in ["", "Debit", "debit ", "dr", "asset"] {
        let account = NewAccount {
            normal_side,
            ..NewAccount::valid()
        };
        let err = account.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, "accounts_normal_side_check");
    }

    // No default: an insert that forgets the side fails.
    let missing = sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency) \
         VALUES ($1, 'cash', 'platform', 'EUR')",
        Uuid::now_v7()
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(missing), NOT_NULL_VIOLATION);

    for (kind, normal_side) in [("cash", "debit"), ("fee_revenue", "credit")] {
        let account = NewAccount {
            kind,
            normal_side,
            ..NewAccount::valid()
        };
        account.insert(&mut app).await?;
    }
    assert_eq!(account_count(&pool).await?, 2);
    Ok(())
}

#[sqlx::test]
async fn the_currency_must_be_a_known_code(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    // The column uses the currency_code domain, which checks the format.
    let malformed = NewAccount {
        currency: "eur",
        ..NewAccount::valid()
    };
    let err = malformed.insert_rejected(&mut app).await;
    assert_violation(err, CHECK_VIOLATION, "currency_code_check");

    // XTS is ISO 4217's code for testing: well formed, but not seeded.
    let unknown = NewAccount {
        currency: "XTS",
        ..NewAccount::valid()
    };
    let err = unknown.insert_rejected(&mut app).await;
    assert_violation(err, FOREIGN_KEY_VIOLATION, "accounts_currency_fkey");
    assert_eq!(account_count(&pool).await?, 0);
    Ok(())
}

#[sqlx::test]
async fn ids_are_uuidv7(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    // The nil UUID, and a version 7 without the RFC 9562 variant, make
    // uuid_extract_version return NULL, which the CHECK must not accept.
    let not_rfc_variant = uuid::uuid!("00000000-0000-7000-0000-000000000000");
    let version_4 = uuid::uuid!("00000000-0000-4000-8000-000000000000");
    let version_8 = uuid::uuid!("00000000-0000-8000-8000-000000000000");
    for id in [Uuid::nil(), not_rfc_variant, version_4, version_8] {
        let account = NewAccount {
            id,
            ..NewAccount::valid()
        };
        let err = account.insert_rejected(&mut app).await;
        assert_violation(err, CHECK_VIOLATION, "accounts_id_check");
    }
    assert_eq!(account_count(&pool).await?, 0);
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_cannot_choose_recorded_at(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    let account = NewAccount::valid();
    let chosen = sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency, normal_side, recorded_at) \
         VALUES ($1, $2, $3, $4, $5, '2000-01-01 00:00:00+00')",
        account.id,
        account.kind,
        account.party,
        account.currency,
        account.normal_side
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    // The message names the table, not the column, so the code is checked.
    assert_eq!(sqlstate(chosen), INSUFFICIENT_PRIVILEGE);

    // Even naming the column to ask for its default needs the privilege.
    let default = sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency, normal_side, recorded_at) \
         VALUES ($1, $2, $3, $4, $5, DEFAULT)",
        account.id,
        account.kind,
        account.party,
        account.currency,
        account.normal_side
    )
    .execute(&mut app)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(default), INSUFFICIENT_PRIVILEGE);
    assert_eq!(account_count(&pool).await?, 0);
    Ok(())
}

#[sqlx::test]
async fn recorded_at_is_the_start_of_the_inserting_transaction(pool: PgPool) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SET LOCAL ROLE payments_app")
        .execute(&mut *tx)
        .await?;
    // Let the clock move on, so that the start of the transaction and the
    // start of the insert are different instants.
    sqlx::query!(r#"SELECT 1 AS "slept!" FROM pg_sleep(0.01)"#)
        .fetch_one(&mut *tx)
        .await?;

    let account = NewAccount::valid();
    let stamp = sqlx::query!(
        r#"INSERT INTO accounts (id, kind, party, currency, normal_side)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING recorded_at = transaction_timestamp() AS "at_transaction_start!",
                     recorded_at < statement_timestamp() AS "before_the_insert!""#,
        account.id,
        account.kind,
        account.party,
        account.currency,
        account.normal_side
    )
    .fetch_one(&mut *tx)
    .await?;
    assert!(stamp.at_transaction_start, "not the transaction's start");
    assert!(stamp.before_the_insert, "not earlier than the insert");
    tx.rollback().await
}

#[sqlx::test]
async fn the_runtime_role_cannot_change_accounts(pool: PgPool) -> sqlx::Result<()> {
    let mut app = runtime_connection(&pool).await?;
    NewAccount::valid().insert(&mut app).await?;

    let update = sqlx::query!("UPDATE accounts SET kind = 'cash'")
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(update), INSUFFICIENT_PRIVILEGE);

    let delete = sqlx::query!("DELETE FROM accounts")
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(delete), INSUFFICIENT_PRIVILEGE);

    let truncate = sqlx::query!("TRUNCATE accounts")
        .execute(&mut app)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(truncate), INSUFFICIENT_PRIVILEGE);
    assert_eq!(account_count(&pool).await?, 1);
    Ok(())
}

#[sqlx::test]
async fn accounts_are_append_only_even_for_the_owner(pool: PgPool) -> sqlx::Result<()> {
    // The test pool connects as the role that owns the tables (ADR-0008),
    // which holds every privilege, so only the trigger stands in the way.
    let mut owner = pool.acquire().await?;
    NewAccount::valid().insert(&mut owner).await?;

    let update = sqlx::query!("UPDATE accounts SET kind = 'cash'")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(update, APPEND_ONLY, "accounts_append_only", "accounts");

    let delete = sqlx::query!("DELETE FROM accounts")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(delete, APPEND_ONLY, "accounts_append_only", "accounts");

    // CASCADE, so that the test still reaches the trigger once the ledger's
    // entries refer to accounts: a plain TRUNCATE of a referenced table
    // fails with 0A000 before any trigger runs.
    let truncate = sqlx::query!("TRUNCATE accounts CASCADE")
        .execute(&mut *owner)
        .await
        .unwrap_err();
    assert_guard(truncate, APPEND_ONLY, "accounts_no_truncate", "accounts");
    assert_eq!(account_count(&pool).await?, 1);
    Ok(())
}
