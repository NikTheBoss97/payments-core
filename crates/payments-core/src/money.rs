// Author: Nicola Antonello

//! Money types (ADR-0003).
//!
//! Every value is checked once, when it is built, so holding one proves it
//! is valid ("parse, don't validate"). The types offer no `+` or `-`: sums of
//! amounts in one currency go through [`total`].
//!
//! The lints below reject `as` casts and arithmetic operators such as
//! `a + b`, and `forbid` keeps an inner `#[allow]` from lifting them. The
//! arithmetic lint has gaps: it skips the bodies of plain `#[test]`
//! functions (rust-clippy#13845), where `as` is still rejected, and it
//! misses shifts and most methods. Of those, `sum`, `abs` and `pow` still
//! panic on overflow, since overflow checks are on in every profile, while
//! `<<`, `wrapping_*` and `saturating_*` return a wrong amount silently. Use
//! `checked_*`, and never shift an amount.

#![forbid(clippy::arithmetic_side_effects, clippy::as_conversions)]

use std::fmt;

/// The largest magnitude of a single amount, in minor units: 10 trillion in
/// a two-decimal currency.
pub const MAX_ABS: i64 = 1_000_000_000_000_000;

/// Why a value is not valid money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    /// A ledger amount of zero, which moves nothing.
    Zero,
    /// A magnitude above [`MAX_ABS`].
    OutOfRange(i64),
    /// An amount from the API that is zero or negative.
    NotPositive(i64),
    /// Not exactly three uppercase ASCII letters.
    InvalidCurrency(String),
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(f, "an amount cannot be zero"),
            Self::OutOfRange(minor) => {
                write!(f, "amount {minor} is beyond ±{MAX_ABS} minor units")
            }
            Self::NotPositive(minor) => write!(f, "amount {minor} must be positive"),
            Self::InvalidCurrency(code) => {
                write!(f, "currency {code:?} must be three uppercase ASCII letters")
            }
        }
    }
}

impl std::error::Error for MoneyError {}

/// A signed ledger amount in minor units: positive for a debit, negative for
/// a credit. Never zero and never beyond ±[`MAX_ABS`], so negating it cannot
/// overflow. Sums go through [`total`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount(i64);

impl Amount {
    /// Checks that `minor` is non-zero and within ±[`MAX_ABS`].
    ///
    /// # Errors
    ///
    /// [`MoneyError::Zero`] for zero, and [`MoneyError::OutOfRange`] beyond
    /// the cap.
    pub fn new(minor: i64) -> Result<Self, MoneyError> {
        if minor == 0 {
            Err(MoneyError::Zero)
        } else if minor.unsigned_abs() > MAX_ABS.unsigned_abs() {
            // unsigned_abs, because i64::MIN.abs() would overflow.
            Err(MoneyError::OutOfRange(minor))
        } else {
            Ok(Self(minor))
        }
    }

    /// The amount in minor units.
    pub fn get(self) -> i64 {
        self.0
    }

    /// The same amount on the other side of the ledger.
    #[must_use]
    pub fn negate(self) -> Self {
        Self(
            self.0
                .checked_neg()
                .expect("the cap keeps amounts away from i64::MIN"),
        )
    }
}

impl TryFrom<i64> for Amount {
    type Error = MoneyError;

    fn try_from(minor: i64) -> Result<Self, Self::Error> {
        Self::new(minor)
    }
}

/// An amount as the API receives it: from 1 to [`MAX_ABS`] minor units. It
/// has no sign, so the client cannot pick one: the domain turns it into a
/// ledger [`Amount`] with [`debit`](Self::debit) or [`credit`](Self::credit),
/// which name the side at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositiveAmount(i64);

impl PositiveAmount {
    /// Checks that `minor` is positive and at most [`MAX_ABS`].
    ///
    /// # Errors
    ///
    /// [`MoneyError::NotPositive`] for zero or less, and
    /// [`MoneyError::OutOfRange`] above the cap.
    pub fn new(minor: i64) -> Result<Self, MoneyError> {
        if minor <= 0 {
            Err(MoneyError::NotPositive(minor))
        } else if minor > MAX_ABS {
            Err(MoneyError::OutOfRange(minor))
        } else {
            Ok(Self(minor))
        }
    }

    /// The amount in minor units. To post it, use [`debit`](Self::debit) or
    /// [`credit`](Self::credit): `Amount::new(p.get())` also compiles, but
    /// leaves the side unnamed.
    pub fn get(self) -> i64 {
        self.0
    }

    /// This amount as a debit (positive) ledger amount.
    #[must_use]
    pub fn debit(self) -> Amount {
        Amount(self.0)
    }

    /// This amount as a credit (negative) ledger amount.
    #[must_use]
    pub fn credit(self) -> Amount {
        self.debit().negate()
    }
}

impl TryFrom<i64> for PositiveAmount {
    type Error = MoneyError;

    fn try_from(minor: i64) -> Result<Self, Self::Error> {
        Self::new(minor)
    }
}

/// An ISO 4217 alphabetic code: exactly three uppercase ASCII letters,
/// ordered alphabetically. Whether the currency is known is checked by the
/// database, through the `currencies` table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Currency([u8; 3]);

impl Currency {
    /// Checks that `code` is three uppercase ASCII letters.
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidCurrency`] for anything else.
    pub fn parse(code: &str) -> Result<Self, MoneyError> {
        match *code.as_bytes() {
            [a, b, c] if [a, b, c].iter().all(u8::is_ascii_uppercase) => Ok(Self([a, b, c])),
            _ => Err(MoneyError::InvalidCurrency(code.to_owned())),
        }
    }

    /// The code as text, such as `"EUR"`.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("ASCII letters are valid UTF-8")
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // pad, unlike write_str, applies width and alignment as for a &str.
        f.pad(self.as_str())
    }
}

// Written by hand: the derive would show the bytes, Currency([69, 85, 82]).
impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Currency").field(&self.as_str()).finish()
    }
}

/// The exact sum of `amounts`, which must all be in one currency: an
/// [`Amount`] does not record its currency, so this function cannot check
/// that, and a sum across currencies can be zero while each currency is
/// unbalanced. Overflowing an `i128` would take more than 10^23 amounts at
/// the cap, so the checked addition never fails in practice.
pub fn total(amounts: impl IntoIterator<Item = Amount>) -> i128 {
    amounts.into_iter().fold(0_i128, |sum, amount| {
        sum.checked_add(i128::from(amount.get()))
            .expect("an i128 holds more than 10^23 amounts at the cap")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::cmp::Ordering;
    use std::collections::HashSet;

    /// The per-entry cap, 10^15 minor units, written out rather than taken
    /// from MAX_ABS, so the tests do not trust the constant they check.
    const CAP: i64 = 1_000_000_000_000_000;

    /// One minor unit beyond the cap.
    const BEYOND: i64 = CAP + 1;

    #[test]
    fn constructors_follow_the_rules_at_every_edge() {
        use MoneyError::{NotPositive, OutOfRange, Zero};
        const MIN: i64 = i64::MIN;
        const MAX: i64 = i64::MAX;
        // Each bound, the value just beyond it and the ends of i64, with what
        // Amount and PositiveAmount must return. The TryFrom impls are public
        // constructors too, and ADR-0003 allows only validated ones, so
        // try_from must agree with new.
        let cases = [
            (MIN, Err(OutOfRange(MIN)), Err(NotPositive(MIN))),
            (-BEYOND, Err(OutOfRange(-BEYOND)), Err(NotPositive(-BEYOND))),
            (-CAP, Ok(-CAP), Err(NotPositive(-CAP))),
            (-1, Ok(-1), Err(NotPositive(-1))),
            (0, Err(Zero), Err(NotPositive(0))),
            (1, Ok(1), Ok(1)),
            (CAP, Ok(CAP), Ok(CAP)),
            (BEYOND, Err(OutOfRange(BEYOND)), Err(OutOfRange(BEYOND))),
            (MAX, Err(OutOfRange(MAX)), Err(OutOfRange(MAX))),
        ];
        for (value, amount, positive) in cases {
            assert_eq!(Amount::new(value).map(Amount::get), amount, "{value}");
            assert_eq!(Amount::try_from(value).map(Amount::get), amount, "{value}");
            let get = PositiveAmount::get;
            assert_eq!(PositiveAmount::new(value).map(get), positive, "{value}");
            assert_eq!(
                PositiveAmount::try_from(value).map(get),
                positive,
                "{value}"
            );
        }
    }

    #[test]
    fn positive_amount_becomes_a_debit_or_a_credit() {
        // debit() and credit() build an Amount without Amount::new, so check
        // them against it, at both edges.
        for (value, credit) in [(1, -1), (1050, -1050), (CAP, -CAP)] {
            let amount = PositiveAmount::new(value).expect("valid amount");
            assert_eq!(Ok(amount.debit()), Amount::new(value), "{value}");
            assert_eq!(Ok(amount.credit()), Amount::new(credit), "{value}");
        }
    }

    #[test]
    fn negation_flips_the_sign() {
        for (value, negated) in [(1, -1), (-1, 1), (1050, -1050), (CAP, -CAP), (-CAP, CAP)] {
            let amount = Amount::new(value).expect("valid amount");
            assert_eq!(amount.negate().get(), negated, "{value}");
        }
    }

    #[test]
    fn currency_accepts_every_three_letter_code_in_order() {
        // There are only 26^3 = 17,576 codes, so check every one. They are
        // generated alphabetically, so each must sort after the previous one
        // (Ord), differ from it (Eq) and differ from every earlier one (Eq
        // and Hash, through the set). The order alone would pass an Eq that
        // merges codes, and a ledger transaction's one-currency check needs
        // distinct codes (ADR-0004).
        let mut previous: Option<Currency> = None;
        let mut seen = HashSet::new();
        for a in b'A'..=b'Z' {
            for b in b'A'..=b'Z' {
                for c in b'A'..=b'Z' {
                    let code = String::from_utf8(vec![a, b, c]).expect("ASCII letters");
                    let currency = Currency::parse(&code).expect("three uppercase letters");
                    assert_eq!(currency.as_str(), code);
                    assert_eq!(currency.to_string(), code);
                    if let Some(previous) = previous {
                        let order = previous.cmp(&currency);
                        assert_eq!(order, Ordering::Less, "{previous}, {currency}");
                        assert_ne!(previous, currency);
                    }
                    assert!(seen.insert(currency), "{code} equals an earlier code");
                    previous = Some(currency);
                }
            }
        }
    }

    #[test]
    fn currency_allows_only_uppercase_ascii_letters_in_each_position() {
        // Every ASCII byte in each position of a valid code: only 'A' to 'Z'
        // keep it valid, so control bytes and punctuation are covered too.
        // The alphabet is spelled out rather than taken from
        // is_ascii_uppercase, the method under test.
        for position in 0..3 {
            for byte in 0..=127_u8 {
                let mut bytes = *b"EUR";
                bytes[position] = byte;
                let code = std::str::from_utf8(&bytes).expect("ASCII is valid UTF-8");
                let expected = if b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".contains(&byte) {
                    Ok(code.to_owned())
                } else {
                    Err(MoneyError::InvalidCurrency(code.to_owned()))
                };
                assert_eq!(Currency::parse(code).map(|c| c.to_string()), expected);
            }
        }
    }

    #[test]
    fn currency_rejects_the_wrong_length_and_non_ascii_bytes() {
        for code in [
            // The wrong length, rejected before any byte is looked at.
            "", "EU", "EURO", "ÉUR", " EUR",
            // Three bytes, not all of them ASCII. É is an uppercase letter in
            // Unicode, so a char::is_uppercase check would accept "ÉU".
            "ÉU", "€",
        ] {
            assert_eq!(
                Currency::parse(code),
                Err(MoneyError::InvalidCurrency(code.into())),
                "{code:?}"
            );
        }
    }

    #[test]
    fn currency_formats_as_its_code() {
        let eur = Currency::parse("EUR").expect("valid code");
        assert_eq!(format!("{eur:?}"), r#"Currency("EUR")"#);
        // Width and alignment apply as they do to a &str, which aligns left
        // by default.
        assert_eq!(format!("[{eur:5}|{eur:>5}]"), "[EUR  |  EUR]");
    }

    #[test]
    fn errors_name_the_value_and_the_rule() {
        for (error, text) in [
            (MoneyError::Zero, "an amount cannot be zero"),
            (
                MoneyError::OutOfRange(-BEYOND),
                "amount -1000000000000001 is beyond ±1000000000000000 minor units",
            ),
            (
                MoneyError::OutOfRange(BEYOND),
                "amount 1000000000000001 is beyond ±1000000000000000 minor units",
            ),
            (MoneyError::NotPositive(0), "amount 0 must be positive"),
            (
                MoneyError::NotPositive(-1050),
                "amount -1050 must be positive",
            ),
            // Quoted and escaped: a blank stays visible, and a newline in the
            // input cannot split a log line.
            (
                MoneyError::InvalidCurrency(" EU\n".into()),
                r#"currency " EU\n" must be three uppercase ASCII letters"#,
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    #[test]
    fn total_of_nothing_is_zero() {
        assert_eq!(total([]), 0);
    }

    /// Any i64, half of the time drawn uniformly from -BEYOND to BEYOND,
    /// because uniform sampling of all of i64 almost never reaches the valid
    /// range. Neither half lands on an edge in practice: the table test pins
    /// those.
    fn candidate_amounts() -> impl Strategy<Value = i64> {
        prop_oneof![-BEYOND..=BEYOND, any::<i64>()]
    }

    /// Any valid ledger amount, with both ends of each range drawn often.
    fn valid_amounts() -> impl Strategy<Value = i64> {
        prop_oneof![
            prop::sample::select(vec![-CAP, -1, 1, CAP]),
            -CAP..=-1,
            1..=CAP,
        ]
    }

    /// Any valid API amount, with both ends drawn often.
    fn positive_amounts() -> impl Strategy<Value = i64> {
        prop_oneof![prop::sample::select(vec![1, CAP]), 1..=CAP]
    }

    proptest! {
        #[test]
        fn constructors_accept_exactly_the_valid_values(value in candidate_amounts()) {
            // The rules restated with ranges, independently of the code.
            let amount = if value == 0 {
                Err(MoneyError::Zero)
            } else if (-CAP..=CAP).contains(&value) {
                Ok(value)
            } else {
                Err(MoneyError::OutOfRange(value))
            };
            let positive = if value < 1 {
                Err(MoneyError::NotPositive(value))
            } else if value <= CAP {
                Ok(value)
            } else {
                Err(MoneyError::OutOfRange(value))
            };
            prop_assert_eq!(Amount::new(value).map(Amount::get), amount.clone());
            prop_assert_eq!(Amount::try_from(value).map(Amount::get), amount);
            let get = PositiveAmount::get;
            prop_assert_eq!(PositiveAmount::new(value).map(get), positive.clone());
            prop_assert_eq!(PositiveAmount::try_from(value).map(get), positive);
        }

        #[test]
        fn negation_flips_the_sign_and_undoes_itself(value in valid_amounts()) {
            let amount = Amount::new(value).expect("valid amount");
            prop_assert_eq!(total([amount, amount.negate()]), 0);
            prop_assert_eq!(amount.negate().negate(), amount);
        }

        #[test]
        fn a_debit_and_a_credit_of_the_same_amount_sum_to_zero(value in positive_amounts()) {
            let amount = PositiveAmount::new(value).expect("valid amount");
            prop_assert_eq!(Ok(amount.debit()), Amount::new(value));
            prop_assert_eq!(total([amount.debit(), amount.credit()]), 0);
        }

        #[test]
        fn total_is_exact_beyond_the_range_of_i64(count in 0_usize..20_000, negative in any::<bool>()) {
            // 9,224 or more amounts at the cap overflow an i64 sum.
            let amount = Amount::new(if negative { -CAP } else { CAP }).expect("valid amount");
            let expected = i128::from(amount.get())
                .checked_mul(i128::try_from(count).expect("count fits in i128"))
                .expect("no overflow");
            prop_assert_eq!(total(std::iter::repeat_n(amount, count)), expected);
        }
    }
}
