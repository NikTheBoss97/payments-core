// Author: Nicola Antonello

//! The ledger under concurrent writers, as the runtime role (ADR-0002,
//! ADR-0009, ADR-0015).
//!
//! `join_all` polls many futures at once on the test's one thread. Each
//! holds its own pooled connection, so PostgreSQL runs their transactions at
//! the same time, up to the pool's ten connections. Balances are checked
//! against an in-memory model that knows nothing of the database (ADR-0002).

mod common;

use std::collections::HashMap;
use std::error::Error;

use common::{account_id, runtime_pool, transaction_id};
use futures_util::future::join_all;
use payments_core::accounts::{AccountKey, AccountKind, NormalSide, Party, open_account};
use payments_core::balances::balance;
use payments_core::ids::{AccountId, TransactionId};
use payments_core::ledger::{Entry, LedgerTransaction};
use payments_core::money::{Amount, Currency};
use payments_core::posting::{Posted, post};
use sqlx::PgPool;

type TestResult = Result<(), Box<dyn Error>>;

/// SplitMix64, a small deterministic generator, so that a failing run can be
/// repeated exactly from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number from 0 to `n - 1`.
    fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("usize fits in u64");
        usize::try_from(self.next() % n).expect("below n, which is a usize")
    }
}

fn currency(code: &str) -> Currency {
    Currency::parse(code).expect("valid code")
}

/// Opens a platform account of this kind and currency.
async fn open(
    app: &PgPool,
    n: u32,
    kind: &str,
    code: &str,
    side: NormalSide,
) -> Result<AccountId, Box<dyn Error>> {
    let key = AccountKey {
        kind: AccountKind::parse(kind)?,
        party: Party::label("platform")?,
        currency: currency(code),
    };
    Ok(open_account(app, account_id(n), &key, side).await?)
}

fn transaction(id: TransactionId, code: &str, lines: &[(AccountId, i64)]) -> LedgerTransaction {
    let entries = lines
        .iter()
        .map(|&(account, minor)| Entry {
            account,
            amount: Amount::new(minor).expect("valid amount"),
        })
        .collect();
    LedgerTransaction::new(id, currency(code), entries).expect("balanced")
}

/// The trial balance: every currency whose entries do not sum to zero
/// across the whole ledger. Invariant I1 says there are none (ADR-0002).
async fn unbalanced_currencies(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT currency AS "currency!" FROM ledger_entries
            GROUP BY currency HAVING sum(amount) <> 0"#
    )
    .fetch_all(pool)
    .await
}

#[sqlx::test]
async fn concurrent_postings_match_an_in_memory_model(pool: PgPool) -> TestResult {
    let app = runtime_pool(&pool).await?;
    let mut eur = Vec::new();
    for n in 1..=6 {
        let side = if n % 2 == 0 {
            NormalSide::Credit
        } else {
            NormalSide::Debit
        };
        eur.push(open(&app, n, &format!("kind_{n}"), "EUR", side).await?);
    }
    let usd = vec![
        open(&app, 7, "kind_7", "USD", NormalSide::Debit).await?,
        open(&app, 8, "kind_8", "USD", NormalSide::Credit).await?,
    ];

    // 200 postings on random accounts of one currency: one to four random
    // amounts, then the line that balances them, unless they already sum to
    // zero. One amount is never zero, so every posting has two lines or
    // more. The model adds every line up in Rust.
    let mut rng = Rng(0x5EED);
    let mut model: HashMap<AccountId, i128> = HashMap::new();
    let mut postings = Vec::new();
    for n in 1..=200 {
        let (code, accounts) = if rng.below(4) == 0 {
            ("USD", &usd)
        } else {
            ("EUR", &eur)
        };
        let mut lines = Vec::new();
        let mut sum = 0_i64;
        for _ in 0..=rng.below(4) {
            let magnitude = i64::try_from(rng.below(1_000_000))?
                .checked_add(1)
                .ok_or("overflow")?;
            let amount = if rng.below(2) == 0 {
                magnitude
            } else {
                -magnitude
            };
            sum = sum.checked_add(amount).ok_or("overflow")?;
            lines.push((accounts[rng.below(accounts.len())], amount));
        }
        if sum != 0 {
            let balancing = sum.checked_neg().ok_or("overflow")?;
            lines.push((accounts[rng.below(accounts.len())], balancing));
        }
        for &(account, amount) in &lines {
            let total = model.entry(account).or_insert(0);
            *total = total.checked_add(i128::from(amount)).ok_or("overflow")?;
        }
        postings.push(transaction(transaction_id(n), code, &lines));
    }

    let results = join_all(postings.iter().map(|posting| post(&app, posting))).await;
    for result in results {
        assert_eq!(result?, Posted::Applied);
    }
    for account in eur.iter().chain(&usd) {
        let read = balance(&app, *account).await?.expect("the account exists");
        let expected = model.get(account).copied().unwrap_or(0);
        assert_eq!(i128::from(read.debits_minus_credits), expected, "{account}");
    }
    assert_eq!(unbalanced_currencies(&pool).await?, Vec::<String>::new());
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn concurrent_retries_apply_a_posting_once(pool: PgPool) -> TestResult {
    // Eight copies of one posting, as eight retries racing each other: one
    // inserts it, and the others wait for it to commit, find the same ID and
    // compare (ADR-0015).
    let app = runtime_pool(&pool).await?;
    let cash = open(&app, 1, "cash", "EUR", NormalSide::Debit).await?;
    let payable = open(&app, 2, "merchant_payable", "EUR", NormalSide::Credit).await?;
    let id = transaction_id(1).get();
    let copies: Vec<LedgerTransaction> = (0..8)
        .map(|_| {
            let same = TransactionId::try_from(id).expect("a UUIDv7");
            transaction(same, "EUR", &[(cash, 500), (payable, -500)])
        })
        .collect();
    let results = join_all(copies.iter().map(|copy| post(&app, copy))).await;
    let count = |outcome: Posted| {
        results
            .iter()
            .filter(|result| matches!(result, Ok(posted) if *posted == outcome))
            .count()
    };
    assert_eq!(
        (count(Posted::Applied), count(Posted::AlreadyApplied)),
        (1, 7),
        "{results:?}"
    );
    let read = balance(&app, cash).await?.expect("the account exists");
    assert_eq!(read.debits_minus_credits, 500);
    app.close().await;
    Ok(())
}

#[sqlx::test]
async fn concurrent_openings_of_one_key_return_one_id(pool: PgPool) -> TestResult {
    // Eight callers race to open the same key with eight candidates: one
    // insert wins, the others wait for it and read its ID (ADR-0009).
    let app = runtime_pool(&pool).await?;
    let key = AccountKey {
        kind: AccountKind::parse("fee_revenue")?,
        party: Party::label("platform")?,
        currency: currency("EUR"),
    };
    let opened =
        join_all((1..=8).map(|n| open_account(&app, account_id(n), &key, NormalSide::Credit)))
            .await;
    let ids = opened.into_iter().collect::<Result<Vec<_>, _>>()?;
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    let stored: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "count!" FROM accounts"#)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 1);
    app.close().await;
    Ok(())
}
