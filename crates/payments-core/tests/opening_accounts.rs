// Author: Nicola Antonello

//! Opening and finding accounts through the library (ADR-0009), as the
//! runtime role. `accounts.rs` tests the table's own rules; these tests check
//! what `open_account` and `find_account` make of them.

mod common;

use std::error::Error;

use common::{
    CHECK_VIOLATION, FOREIGN_KEY_VIOLATION, UNIQUE_VIOLATION, account_id, assert_violation,
    runtime_pool,
};
use payments_core::accounts::{
    Account, AccountError, AccountKey, AccountKind, NormalSide, Party, find_account, open_account,
};
use payments_core::money::Currency;
use sqlx::{PgConnection, PgPool};
use uuid::{Uuid, uuid};

type TestResult = Result<(), Box<dyn Error>>;

/// A platform account of this kind and currency.
fn platform(kind: &str, currency: &str) -> AccountKey {
    AccountKey {
        kind: AccountKind::parse(kind).expect("valid kind"),
        party: Party::label("platform").expect("valid party"),
        currency: Currency::parse(currency).expect("valid code"),
    }
}

/// How many accounts the table holds.
async fn accounts(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM accounts"#)
        .fetch_one(pool)
        .await
}

#[sqlx::test]
async fn a_new_key_gets_the_candidate_id(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let fees = platform("fee_revenue", "EUR");
    let id = open_account(&app, account_id(1), &fees, NormalSide::Credit).await?;
    assert_eq!(id, account_id(1));
    let found = find_account(&app, &fees).await?;
    assert_eq!(
        found,
        Some(Account {
            id,
            normal_side: NormalSide::Credit
        })
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_existing_key_keeps_its_first_id(pool: PgPool) -> TestResult {
    // Opening is idempotent by key (ADR-0009): the second candidate is
    // discarded, and the stored ID comes back.
    let app = runtime_pool(&pool).await?;
    let fees = platform("fee_revenue", "EUR");
    let first = open_account(&app, account_id(1), &fees, NormalSide::Credit).await?;
    let second = open_account(&app, account_id(2), &fees, NormalSide::Credit).await?;
    assert_eq!((first, second), (account_id(1), account_id(1)));
    assert_eq!(accounts(&pool).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn the_stored_normal_side_must_match(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let fees = platform("fee_revenue", "EUR");
    open_account(&app, account_id(1), &fees, NormalSide::Credit).await?;
    let err = open_account(&app, account_id(2), &fees, NormalSide::Debit)
        .await
        .unwrap_err();
    let AccountError::SideMismatch {
        key,
        stored,
        requested,
    } = err
    else {
        panic!("expected SideMismatch, got {err:?}");
    };
    assert_eq!(
        (key, stored, requested),
        (fees, NormalSide::Credit, NormalSide::Debit)
    );
    assert_eq!(accounts(&pool).await?, 1);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_currency_is_reported(pool: PgPool) -> TestResult {
    // XYZ is a well-formed code, but not in the currencies table.
    let app = runtime_pool(&pool).await?;
    let err = open_account(
        &app,
        account_id(1),
        &platform("fee_revenue", "XYZ"),
        NormalSide::Credit,
    )
    .await
    .unwrap_err();
    let AccountError::UnknownCurrency { currency, source } = err else {
        panic!("expected UnknownCurrency, got {err:?}");
    };
    assert_eq!(currency.as_str(), "XYZ");
    assert_violation(source, FOREIGN_KEY_VIOLATION, "accounts_currency_fkey");
    assert_eq!(accounts(&pool).await?, 0);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_reused_candidate_id_is_a_bug(pool: PgPool) -> TestResult {
    // Candidates come from AccountId::now_v7(), so two keys never share one.
    let app = runtime_pool(&pool).await?;
    let fees = platform("fee_revenue", "EUR");
    let cash = platform("cash", "EUR");
    open_account(&app, account_id(1), &fees, NormalSide::Credit).await?;
    let err = open_account(&app, account_id(1), &cash, NormalSide::Debit)
        .await
        .unwrap_err();
    let AccountError::Invariant { key, source } = err else {
        panic!("expected Invariant, got {err:?}");
    };
    assert_eq!(key, cash);
    assert_violation(source, UNIQUE_VIOLATION, "accounts_pkey");
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unopened_key_is_not_found(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    assert_eq!(
        find_account(&app, &platform("fee_revenue", "EUR")).await?,
        None
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn both_normal_sides_round_trip_through_the_database(pool: PgPool) -> TestResult {
    // SQL binds NormalSide::as_str() and Rust parses what comes back
    // (ADR-0009). This stands in for the check an enum type would give.
    let app = runtime_pool(&pool).await?;
    for (n, side) in [(1, NormalSide::Debit), (2, NormalSide::Credit)] {
        let key = platform(&format!("kind_{n}"), "EUR");
        let id = open_account(&app, account_id(n), &key, side).await?;
        assert_eq!(
            find_account(&app, &key).await?,
            Some(Account {
                id,
                normal_side: side
            })
        );
    }
    app.close().await;
    Ok(())
}

/// Whether the accounts table accepts this kind and party. The insert runs
/// in a savepoint, so a rejection does not abort the surrounding
/// transaction, and the row is never kept.
async fn accepts(conn: &mut PgConnection, kind: &str, party: &str) -> sqlx::Result<bool> {
    sqlx::query!("SAVEPOINT spelling")
        .execute(&mut *conn)
        .await?;
    let insert = sqlx::query!(
        "INSERT INTO accounts (id, kind, party, currency, normal_side) \
         VALUES ($1, $2, $3, $4, $5)",
        Uuid::now_v7(),
        kind,
        party,
        "EUR",
        NormalSide::Debit.as_str()
    )
    .execute(&mut *conn)
    .await;
    sqlx::query!("ROLLBACK TO SAVEPOINT spelling")
        .execute(&mut *conn)
        .await?;
    match insert {
        Ok(_) => Ok(true),
        Err(err) => {
            let code = err.as_database_error().and_then(|db| db.code());
            if code.as_deref() == Some(CHECK_VIOLATION) {
                Ok(false)
            } else {
                Err(err)
            }
        }
    }
}

#[sqlx::test]
async fn rust_accepts_exactly_the_spellings_the_checks_accept(pool: PgPool) -> TestResult {
    // The same strings, through the Rust types and through the table's
    // CHECKs: a key that parses must never fail on its spelling (ADR-0009).
    let merchant = uuid!("d1b2c3d4-e5f6-7a7b-8c9d-0e1f2a3b4c5d");
    let candidates: Vec<String> = [
        "",
        "a",
        "platform",
        "fee_revenue",
        "psp2",
        "Fee",
        "1fee",
        "_fee",
        "fee-revenue",
        "fee revenue",
        "fee\n",
        "fée",
        "ſ",
        "ı",
        "ﬀ",
        "fee.revenue",
    ]
    .into_iter()
    .map(String::from)
    .chain([
        format!("a{}", "b".repeat(62)),
        format!("a{}", "b".repeat(63)),
        merchant.simple().to_string(),
        "0123456789abcdef0123456789abcdef".to_owned(),
        merchant.hyphenated().to_string(),
        merchant.hyphenated().to_string().to_uppercase(),
        merchant.braced().to_string(),
    ])
    .collect();
    let mut tx = pool.begin().await?;
    for s in &candidates {
        let kind = accepts(&mut tx, s, "platform").await?;
        assert_eq!(AccountKind::parse(s).is_ok(), kind, "kind {s:?}");
        let party = accepts(&mut tx, "fee_revenue", s).await?;
        let canonical = Uuid::try_parse(s).is_ok_and(|id| Party::merchant(id).as_str() == s);
        assert_eq!(Party::label(s).is_ok() || canonical, party, "party {s:?}");
    }
    tx.rollback().await?;
    Ok(())
}
