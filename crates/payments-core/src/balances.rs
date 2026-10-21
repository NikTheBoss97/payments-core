// Author: Nicola Antonello

//! Balances, derived from the entries (ADR-0002).
//!
//! No balance is stored anywhere. One statement sums an account's entries,
//! so it reads one snapshot and sees each ledger transaction whole or not
//! at all, even one that commits while the statement runs.
//!
//! The lints are the same as in `money`: this module handles amounts.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use sqlx::PgPool;

use crate::accounts::NormalSide;
use crate::db::code_and_constraint;
use crate::ids::AccountId;
use crate::money::Currency;

/// An account's balance, with the currency and normal side to read it in
/// (ADR-0002, amendment of 2026-10-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    pub currency: Currency,
    pub normal_side: NormalSide,
    /// The signed sum of the account's entries in minor units: debits are
    /// positive and credits negative (ADR-0003).
    pub debits_minus_credits: i64,
}

impl Balance {
    /// The balance as people read it: positive when it sits on the account's
    /// normal side, such as what a merchant is owed on a credit-normal
    /// payable account. An `i128` holds the negation of every `i64`.
    pub fn on_normal_side(&self) -> i128 {
        let sum = i128::from(self.debits_minus_credits);
        match self.normal_side {
            NormalSide::Debit => sum,
            NormalSide::Credit => sum
                .checked_neg()
                .expect("an i128 holds the negation of every i64"),
        }
    }
}

/// Why a balance could not be read.
#[derive(Debug, thiserror::Error)]
pub enum BalanceError {
    /// The sum is beyond the range of `bigint` (`22003`). It takes more than
    /// 9,223 entries at the cap on one side of one account (ADR-0003): a
    /// bug, never retried.
    #[error("the balance of account {account} is beyond the range of bigint")]
    Overflow {
        account: AccountId,
        #[source]
        source: sqlx::Error,
    },
    /// Any other failure. Reading again is safe.
    #[error("the balance of account {account} could not be read")]
    Database {
        account: AccountId,
        #[source]
        source: sqlx::Error,
    },
}

/// Reads an account's balance, or `None` when no account has this ID.
///
/// # Errors
///
/// [`BalanceError::Overflow`] when the sum does not fit in an `i64`, and
/// [`BalanceError::Database`] for any other failure.
pub async fn balance(pool: &PgPool, account: AccountId) -> Result<Option<Balance>, BalanceError> {
    // ADR-0002's sum, joined to the account row in the same statement, so
    // both come from one snapshot. SUM over no rows is NULL, hence COALESCE;
    // SUM(bigint) is numeric, and the cast back raises 22003 on overflow
    // instead of wrapping. GROUP BY a.id may select the account's other
    // columns, because id is its primary key.
    let row = sqlx::query!(
        r#"SELECT a.currency AS "currency!", a.normal_side,
                  COALESCE(SUM(e.amount), 0)::bigint AS "balance!"
             FROM accounts a
             LEFT JOIN ledger_entries e ON e.account_id = a.id
            WHERE a.id = $1
            GROUP BY a.id"#,
        account.get()
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| {
        let (code, _) = code_and_constraint(&source);
        if is_overflow(code.as_deref()) {
            BalanceError::Overflow { account, source }
        } else {
            BalanceError::Database { account, source }
        }
    })?;
    Ok(row.map(|row| Balance {
        currency: Currency::parse(&row.currency)
            .expect("currency_code_check allows only three uppercase letters"),
        normal_side: NormalSide::parse(&row.normal_side)
            .expect("accounts_normal_side_check allows only debit and credit"),
        debits_minus_credits: row.balance,
    }))
}

/// Whether a failed balance read means the sum overflowed:
/// `numeric_value_out_of_range`, from the cast to `bigint`.
fn is_overflow(code: Option<&str>) -> bool {
    code == Some("22003")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balance_of(normal_side: NormalSide, debits_minus_credits: i64) -> Balance {
        Balance {
            currency: Currency::parse("EUR").expect("valid code"),
            normal_side,
            debits_minus_credits,
        }
    }

    #[test]
    fn a_balance_reads_positive_on_its_normal_side() {
        use NormalSide::{Credit, Debit};
        for (side, sum, expected) in [
            (Debit, 10_000, 10_000),
            (Debit, -300, -300),
            // A merchant owed 97.00 on a credit-normal payable account.
            (Credit, -9_700, 9_700),
            (Credit, 300, -300),
            (Debit, 0, 0),
            (Credit, 0, 0),
            // The ends of i64: the negation of i64::MIN needs an i128.
            (Credit, i64::MIN, 9_223_372_036_854_775_808),
            (Credit, i64::MAX, -9_223_372_036_854_775_807),
            (Debit, i64::MIN, -9_223_372_036_854_775_808),
        ] {
            assert_eq!(
                balance_of(side, sum).on_normal_side(),
                expected,
                "{side} {sum}"
            );
        }
    }

    #[test]
    fn only_22003_is_an_overflow() {
        assert!(is_overflow(Some("22003")));
        for code in [Some("57014"), Some("42501"), Some("57P01"), None] {
            assert!(!is_overflow(code), "{code:?}");
        }
    }
}
