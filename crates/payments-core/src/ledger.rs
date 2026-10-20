// Author: Nicola Antonello

//! Ledger transactions (ADR-0004).
//!
//! A [`LedgerTransaction`] can only be built balanced: two or more entries,
//! in its one currency, summing to zero. Each entry's [`Amount`] is already
//! non-zero and within the cap (ADR-0003). The database checks the same rules
//! again at `COMMIT`, as a second layer (ADR-0004), and one rule this type
//! cannot: that each entry's account is kept in the transaction's currency
//! (ADR-0010).
//!
//! The lints are the same as in `money`, for the same reasons.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use crate::ids::{AccountId, TransactionId};
use crate::money::{Amount, Currency, total};

/// One line of a ledger transaction: an amount posted to an account,
/// positive for a debit and negative for a credit (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub account: AccountId,
    pub amount: Amount,
}

/// Why entries do not form a ledger transaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidTransaction {
    /// Fewer than two entries: money moves from one account to another.
    #[error("a ledger transaction needs at least two entries, not {0}")]
    TooFewEntries(usize),
    /// More entries than the database's `entry_count`, an `integer`, holds.
    #[error("a ledger transaction holds at most {max} entries, not {0}", max = i32::MAX)]
    TooManyEntries(usize),
    /// Entries that do not sum to zero, with their exact sum.
    #[error("the entries sum to {0} minor units, not zero")]
    Unbalanced(i128),
}

/// A balanced ledger transaction, ready to post: holding one proves the
/// rules above.
#[derive(Debug, PartialEq, Eq)]
pub struct LedgerTransaction {
    id: TransactionId,
    currency: Currency,
    entries: Vec<Entry>,
}

impl LedgerTransaction {
    /// Checks that there are at least two entries and that they sum to
    /// zero. The entries keep their order: the first is line 1.
    ///
    /// # Errors
    ///
    /// [`InvalidTransaction::TooFewEntries`] below two entries,
    /// [`InvalidTransaction::TooManyEntries`] beyond what `entry_count`
    /// holds, and [`InvalidTransaction::Unbalanced`] for a sum other than
    /// zero, checked in that order.
    pub fn new(
        id: TransactionId,
        currency: Currency,
        entries: Vec<Entry>,
    ) -> Result<Self, InvalidTransaction> {
        entry_count(entries.len())?;
        let sum = total(entries.iter().map(|entry| entry.amount));
        if sum != 0 {
            return Err(InvalidTransaction::Unbalanced(sum));
        }
        Ok(Self {
            id,
            currency,
            entries,
        })
    }

    /// The transaction's ID.
    pub fn id(&self) -> &TransactionId {
        &self.id
    }

    /// The one currency of every entry.
    pub fn currency(&self) -> Currency {
        self.currency
    }

    /// The entries, line 1 first.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The number of entries, as the database's `entry_count` stores it.
    pub fn entry_count(&self) -> i32 {
        entry_count(self.entries.len()).expect("new() checked that the count fits")
    }
}

/// `len` as an `entry_count`, which needs at least two entries and holds at
/// most `i32::MAX`.
fn entry_count(len: usize) -> Result<i32, InvalidTransaction> {
    if len < 2 {
        return Err(InvalidTransaction::TooFewEntries(len));
    }
    i32::try_from(len).map_err(|_| InvalidTransaction::TooManyEntries(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The per-entry cap, written out, as in the money tests.
    const CAP: i64 = 1_000_000_000_000_000;

    fn eur() -> Currency {
        Currency::parse("EUR").expect("valid code")
    }

    fn entry(minor: i64) -> Entry {
        Entry {
            account: AccountId::now_v7(),
            amount: Amount::new(minor).expect("valid amount"),
        }
    }

    fn entries(amounts: &[i64]) -> Vec<Entry> {
        amounts.iter().copied().map(entry).collect()
    }

    #[test]
    fn a_balanced_transaction_keeps_its_id_currency_and_entries_in_order() {
        // A capture: the customer pays 100.00, the merchant gets 97.00 and
        // the platform keeps 3.00 in fees.
        let id = TransactionId::now_v7();
        let uuid = id.get();
        let lines = entries(&[10_000, -9_700, -300]);
        let transaction = LedgerTransaction::new(id, eur(), lines.clone()).expect("balanced");
        assert_eq!(transaction.id().get(), uuid);
        assert_eq!(transaction.currency(), eur());
        assert_eq!(transaction.entries(), lines);
        assert_eq!(transaction.entry_count(), 3);
    }

    #[test]
    fn balance_is_exact_at_the_cap() {
        for amounts in [
            vec![CAP, -CAP],
            vec![-CAP, CAP],
            vec![CAP, CAP, -CAP, -CAP],
            vec![CAP, -1, -999_999_999_999_999],
        ] {
            let transaction =
                LedgerTransaction::new(TransactionId::now_v7(), eur(), entries(&amounts));
            assert!(transaction.is_ok(), "{amounts:?}");
        }
    }

    #[test]
    fn fewer_than_two_entries_are_rejected() {
        for amounts in [vec![], vec![1050]] {
            let count = amounts.len();
            assert_eq!(
                LedgerTransaction::new(TransactionId::now_v7(), eur(), entries(&amounts)),
                Err(InvalidTransaction::TooFewEntries(count)),
                "{amounts:?}"
            );
        }
    }

    #[test]
    fn an_unbalanced_transaction_reports_its_exact_sum() {
        // Both signs, and a sum beyond i64: 10,000 entries at the cap.
        let many = vec![CAP; 10_000];
        for (amounts, sum) in [
            (vec![100, -90], 10),
            (vec![90, -100], -10),
            (vec![CAP, CAP], 2_000_000_000_000_000),
            (many, 10_000_000_000_000_000_000_i128),
        ] {
            assert_eq!(
                LedgerTransaction::new(TransactionId::now_v7(), eur(), entries(&amounts)),
                Err(InvalidTransaction::Unbalanced(sum))
            );
        }
    }

    #[test]
    fn the_entry_count_fits_the_database_column() {
        // A vector of 2^31 entries would not fit in memory, so the count is
        // checked on its own. i32::MAX is the largest PostgreSQL integer.
        let max = usize::try_from(i32::MAX).expect("i32::MAX fits in usize");
        let beyond = max.checked_add(1).expect("no overflow");
        assert_eq!(entry_count(2), Ok(2));
        assert_eq!(entry_count(max), Ok(i32::MAX));
        assert_eq!(
            entry_count(beyond),
            Err(InvalidTransaction::TooManyEntries(beyond))
        );
        assert_eq!(
            entry_count(usize::MAX),
            Err(InvalidTransaction::TooManyEntries(usize::MAX))
        );
        assert_eq!(entry_count(1), Err(InvalidTransaction::TooFewEntries(1)));
    }

    #[test]
    fn errors_name_the_rule_and_the_numbers() {
        for (error, text) in [
            (
                InvalidTransaction::TooFewEntries(1),
                "a ledger transaction needs at least two entries, not 1",
            ),
            (
                InvalidTransaction::TooManyEntries(2_147_483_648),
                "a ledger transaction holds at most 2147483647 entries, not 2147483648",
            ),
            (
                InvalidTransaction::Unbalanced(-10),
                "the entries sum to -10 minor units, not zero",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    /// Any valid amount, with both ends of each range drawn often.
    fn amounts() -> impl Strategy<Value = i64> {
        prop_oneof![
            prop::sample::select(vec![-CAP, -1, 1, CAP]),
            -CAP..=-1,
            1..=CAP,
        ]
    }

    /// A tenth of the cap: nine amounts this large sum to less than the cap.
    const TENTH: i64 = 100_000_000_000_000;

    /// Two to ten valid amounts that sum to zero: up to nine amounts within
    /// a tenth of the cap, then the one that balances them, shuffled.
    fn balanced_amounts() -> impl Strategy<Value = Vec<i64>> {
        let part = prop_oneof![-TENTH..=-1, 1..=TENTH];
        prop::collection::vec(part, 1..10)
            .prop_filter_map("the parts sum to zero", |mut parts| {
                let sum = parts
                    .iter()
                    .try_fold(0_i64, |sum, &part| sum.checked_add(part))?;
                parts.push(sum.checked_neg()?);
                (sum != 0).then_some(parts)
            })
            .prop_shuffle()
    }

    proptest! {
        #[test]
        fn new_accepts_exactly_two_or_more_entries_that_sum_to_zero(
            amounts in prop_oneof![prop::collection::vec(amounts(), 0..12), balanced_amounts()]
        ) {
            // The rules restated, with the sum computed independently.
            let sum = amounts
                .iter()
                .try_fold(0_i128, |sum, &amount| sum.checked_add(i128::from(amount)))
                .expect("no overflow");
            let expected = if amounts.len() < 2 {
                Err(InvalidTransaction::TooFewEntries(amounts.len()))
            } else if sum != 0 {
                Err(InvalidTransaction::Unbalanced(sum))
            } else {
                Ok(())
            };
            let lines = entries(&amounts);
            let transaction = LedgerTransaction::new(TransactionId::now_v7(), eur(), lines.clone());
            prop_assert_eq!(transaction.as_ref().map(|_| ()).map_err(Clone::clone), expected);
            if let Ok(transaction) = transaction {
                prop_assert_eq!(transaction.entries(), lines.as_slice());
                prop_assert_eq!(
                    usize::try_from(transaction.entry_count()).expect("positive"),
                    lines.len()
                );
            }
        }
    }
}
