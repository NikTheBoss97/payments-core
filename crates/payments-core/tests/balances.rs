// Author: Nicola Antonello

//! Reading balances through the library (ADR-0002), as the runtime role.
//! A balance is derived from the entries on every read, so these tests
//! check what `balance` reports; `concurrency.rs` checks the sums against an
//! independent model.

mod common;

use std::error::Error;

use common::{account_id, runtime_pool, sqlstate, transaction_id};
use payments_core::accounts::{AccountKey, AccountKind, NormalSide, Party, open_account};
use payments_core::balances::{Balance, BalanceError, balance};
use payments_core::ids::{AccountId, TransactionId};
use payments_core::ledger::{Entry, LedgerTransaction};
use payments_core::money::{Amount, Currency};
use payments_core::posting::{Posted, post};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// The largest magnitude of a single amount (ADR-0003).
const CAP: i64 = 1_000_000_000_000_000;

fn eur_code() -> Currency {
    Currency::parse("EUR").expect("valid code")
}

/// Opens a platform account in EUR.
async fn open(
    app: &PgPool,
    n: u32,
    kind: &str,
    side: NormalSide,
) -> Result<AccountId, Box<dyn Error>> {
    let key = AccountKey {
        kind: AccountKind::parse(kind)?,
        party: Party::label("platform")?,
        currency: eur_code(),
    };
    Ok(open_account(app, account_id(n), &key, side).await?)
}

/// An EUR ledger transaction with these lines.
fn eur(id: TransactionId, lines: &[(AccountId, i64)]) -> LedgerTransaction {
    let entries = lines
        .iter()
        .map(|&(account, minor)| Entry {
            account,
            amount: Amount::new(minor).expect("valid amount"),
        })
        .collect();
    LedgerTransaction::new(id, eur_code(), entries).expect("balanced")
}

#[sqlx::test]
async fn a_new_account_has_a_zero_balance_in_its_currency(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let fees = open(&app, 1, "fee_revenue", NormalSide::Credit).await?;
    assert_eq!(
        balance(&app, fees).await?,
        Some(Balance {
            currency: eur_code(),
            normal_side: NormalSide::Credit,
            debits_minus_credits: 0,
        })
    );
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn an_unknown_account_has_no_balance(pool: PgPool) -> TestResult {
    // A bare sum would read as zero here.
    let app = runtime_pool(&pool).await?;
    assert_eq!(balance(&app, account_id(99)).await?, None);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_balance_is_the_sum_of_the_accounts_entries(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let receivable = open(&app, 1, "psp_receivable", NormalSide::Debit).await?;
    let payable = open(&app, 2, "merchant_payable", NormalSide::Credit).await?;
    let fees = open(&app, 3, "fee_revenue", NormalSide::Credit).await?;
    // Two captures of 100.00: 97.00 owed to the merchant, 3.00 of fees each.
    for n in 1..=2 {
        let capture = eur(
            transaction_id(n),
            &[(receivable, 10_000), (payable, -9_700), (fees, -300)],
        );
        assert_eq!(post(&app, &capture).await?, Posted::Applied);
    }
    for (account, sum, on_normal_side) in [
        (receivable, 20_000, 20_000),
        (payable, -19_400, 19_400),
        (fees, -600, 600),
    ] {
        let read = balance(&app, account).await?.expect("the account exists");
        assert_eq!(
            (read.debits_minus_credits, read.on_normal_side()),
            (sum, on_normal_side),
            "{account}"
        );
    }
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn a_balance_beyond_bigint_is_an_overflow(pool: PgPool) -> TestResult {
    // 9,224 entries at the cap on one side of each account: the smallest
    // count whose sum passes the range of i64 (ADR-0003). The posting itself
    // balances, so the ledger accepts it; only the read overflows.
    let app = runtime_pool(&pool).await?;
    let cash = open(&app, 1, "cash", NormalSide::Debit).await?;
    let payable = open(&app, 2, "merchant_payable", NormalSide::Credit).await?;
    let mut lines = vec![(cash, CAP); 9_224];
    lines.extend(vec![(payable, -CAP); 9_224]);
    assert_eq!(
        post(&app, &eur(transaction_id(1), &lines)).await?,
        Posted::Applied
    );
    for account in [cash, payable] {
        let err = balance(&app, account).await.unwrap_err();
        let BalanceError::Overflow { source, .. } = err else {
            panic!("expected Overflow, got {err:?}");
        };
        assert_eq!(sqlstate(source), "22003", "numeric_value_out_of_range");
    }
    app.close().await;
    Ok(())
}
