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
    /// A rate above 10,000 basis points (100%).
    RateOutOfRange(u16),
    /// Rates for a split that do not add up to 10,000 basis points.
    SplitNotWhole(u64),
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
            Self::RateOutOfRange(bps) => {
                let whole = BasisPoints::WHOLE.get();
                write!(f, "rate {bps} is beyond {whole} basis points")
            }
            Self::SplitNotWhole(sum) => {
                let whole = BasisPoints::WHOLE.get();
                write!(
                    f,
                    "a split's rates add up to {sum} basis points instead of {whole}"
                )
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

/// A rate in basis points, hundredths of a percent: 2.9% is 290. It runs
/// from 0 to [`WHOLE`](Self::WHOLE), 100%, so a fee can never exceed the
/// amount it is taken from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasisPoints(u16);

impl BasisPoints {
    /// The whole: 10,000 basis points, 100%.
    pub const WHOLE: Self = Self(10_000);

    /// Checks that `bps` is at most 10,000.
    ///
    /// # Errors
    ///
    /// [`MoneyError::RateOutOfRange`] above 10,000.
    pub fn new(bps: u16) -> Result<Self, MoneyError> {
        if bps > Self::WHOLE.0 {
            Err(MoneyError::RateOutOfRange(bps))
        } else {
            Ok(Self(bps))
        }
    }

    /// The rate in basis points.
    pub fn get(self) -> u16 {
        self.0
    }
}

/// Splits `total` into one share per rate, by ADR-0003's rule: each share
/// gets the floor of its exact part, `total * rate / 10_000`, and the units
/// left over go one each to the shares with the largest remainders, ties to
/// the share listed first. The shares add up to `total` exactly, and each is
/// within one unit of its exact part.
///
/// # Errors
///
/// [`MoneyError::SplitNotWhole`] when the rates do not add up to 10,000.
pub fn allocate<const N: usize>(
    total: PositiveAmount,
    rates: [BasisPoints; N],
) -> Result<[i64; N], MoneyError> {
    let sum = rates.iter().fold(0_u64, |sum, rate| {
        sum.checked_add(u64::from(rate.get()))
            .expect("overflowing a u64 would take more than 10^15 rates")
    });
    if sum != u64::from(BasisPoints::WHOLE.get()) {
        return Err(MoneyError::SplitNotWhole(sum));
    }
    let whole = i128::from(BasisPoints::WHOLE.get());
    let total = i128::from(total.get());
    // Each exact part as a floor and a remainder. A total of at most 10^15
    // times a rate of at most 10^4 fits in an i128, though not in an i64.
    let parts = rates.map(|rate| {
        let scaled = total
            .checked_mul(i128::from(rate.get()))
            .expect("the cap times a rate fits in an i128");
        let floor = scaled.checked_div(whole).expect("the whole is not zero");
        let remainder = scaled.checked_rem(whole).expect("the whole is not zero");
        (floor, remainder)
    });
    let floors = parts.iter().fold(0_i128, |sum, (floor, _)| {
        sum.checked_add(*floor)
            .expect("the floors add up to at most the total")
    });
    // The remainders add up to the whole times the units left, and each is
    // below the whole, so fewer units are left than there are shares.
    let left = total
        .checked_sub(floors)
        .expect("the floors add up to at most the total");
    let left = usize::try_from(left).expect("fewer units are left than there are shares");
    let mut order: [usize; N] = std::array::from_fn(|place| place);
    order.sort_by_key(|&place| (std::cmp::Reverse(parts[place].1), place));
    let mut shares = parts.map(|(floor, _)| floor);
    for &place in order.iter().take(left) {
        shares[place] = shares[place]
            .checked_add(1)
            .expect("a share is at most the total");
    }
    Ok(shares.map(|share| i64::try_from(share).expect("a share is at most the total")))
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
            (
                MoneyError::RateOutOfRange(10_001),
                "rate 10001 is beyond 10000 basis points",
            ),
            (
                MoneyError::SplitNotWhole(9_999),
                "a split's rates add up to 9999 basis points instead of 10000",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    #[test]
    fn total_of_nothing_is_zero() {
        assert_eq!(total([]), 0);
    }

    #[test]
    fn basis_points_run_from_0_to_10000() {
        // A u16 has only 65,536 values, so check every one. The bound is
        // written out rather than taken from the code under test.
        for value in 0..=u16::MAX {
            let expected = if value <= 10_000 {
                Ok(value)
            } else {
                Err(MoneyError::RateOutOfRange(value))
            };
            assert_eq!(BasisPoints::new(value).map(BasisPoints::get), expected);
        }
        assert_eq!(BasisPoints::WHOLE.get(), 10_000);
    }

    fn bp(rate: u16) -> BasisPoints {
        BasisPoints::new(rate).expect("valid rate")
    }

    fn positive(minor: i64) -> PositiveAmount {
        PositiveAmount::new(minor).expect("valid amount")
    }

    #[test]
    fn allocate_follows_the_largest_remainders() {
        // ADR-0003's example: 5 cents split 70/30. The exact parts are 3.5
        // and 1.5, the floors 3 and 1 leave one unit, and the remainders
        // tie, so the unit goes to the share listed first.
        assert_eq!(allocate(positive(5), [bp(7000), bp(3000)]), Ok([4, 1]));
        // One unit split in half: the tie goes to the first share.
        assert_eq!(allocate(positive(1), [bp(5000), bp(5000)]), Ok([1, 0]));
        // 33.34, 33.33 and 33.33: the one unit left goes to the largest
        // remainder, 0.34.
        let thirds = [bp(3334), bp(3333), bp(3333)];
        assert_eq!(allocate(positive(100), thirds), Ok([34, 33, 33]));
        // 0.6666, 0.6666 and 0.6668: two units are left. The largest
        // remainder takes one, and the earlier of the tied pair the other.
        let thirds = [bp(3333), bp(3333), bp(3334)];
        assert_eq!(allocate(positive(2), thirds), Ok([1, 0, 1]));
        // One share, or a share of nothing, at both ends of the range.
        assert_eq!(allocate(positive(7), [BasisPoints::WHOLE]), Ok([7]));
        assert_eq!(allocate(positive(7), [bp(0), bp(10_000)]), Ok([0, 7]));
        assert_eq!(allocate(positive(CAP), [BasisPoints::WHOLE]), Ok([CAP]));
        let tiny = [bp(1), bp(9999)];
        let expected = [100_000_000_000, 999_900_000_000_000];
        assert_eq!(allocate(positive(CAP), tiny), Ok(expected));
    }

    #[test]
    fn allocate_refuses_rates_that_are_not_the_whole() {
        let amount = positive(100);
        assert_eq!(allocate(amount, []), Err(MoneyError::SplitNotWhole(0)));
        let half = [bp(5000)];
        assert_eq!(allocate(amount, half), Err(MoneyError::SplitNotWhole(5000)));
        let over = [bp(7000), bp(3001)];
        assert_eq!(
            allocate(amount, over),
            Err(MoneyError::SplitNotWhole(10_001))
        );
        let twice = [BasisPoints::WHOLE, BasisPoints::WHOLE];
        assert_eq!(
            allocate(amount, twice),
            Err(MoneyError::SplitNotWhole(20_000))
        );
    }

    /// Rates that make the whole: the gaps between the sorted `cuts`, which
    /// lie in 0..=10,000, so there is one more rate than there are cuts.
    fn rates_from_cuts<const N: usize>(mut cuts: Vec<u16>) -> [BasisPoints; N] {
        cuts.sort_unstable();
        cuts.insert(0, 0);
        cuts.push(10_000);
        let rates: Vec<BasisPoints> = cuts
            .windows(2)
            .map(|pair| bp(pair[1].checked_sub(pair[0]).expect("sorted cuts")))
            .collect();
        rates.try_into().expect("N - 1 cuts make N rates")
    }

    /// Checks one split against ADR-0003's rule, worked out on its own: each
    /// share is the floor of its exact part or one more, the shares add up
    /// to the total, and a share that got a unit has a larger remainder than
    /// a share that did not, or the same remainder and an earlier place.
    fn check_allocation<const N: usize>(
        total: i64,
        rates: [BasisPoints; N],
    ) -> Result<(), TestCaseError> {
        let shares = allocate(positive(total), rates).expect("the rates make the whole");
        let mut sum = 0_i128;
        // For each share: whether it got a unit, its remainder, its place.
        let mut units = Vec::new();
        for (place, (rate, share)) in rates.iter().zip(shares).enumerate() {
            let scaled = i128::from(total)
                .checked_mul(i128::from(rate.get()))
                .expect("fits in i128");
            let floor = scaled.checked_div(10_000).expect("non-zero divisor");
            let remainder = scaled.checked_rem(10_000).expect("non-zero divisor");
            let unit = i128::from(share).checked_sub(floor).expect("small numbers");
            prop_assert!(
                unit == 0 || unit == 1,
                "share {place} is {share}, its floor {floor}"
            );
            sum = sum.checked_add(i128::from(share)).expect("small numbers");
            units.push((unit == 1, remainder, place));
        }
        prop_assert_eq!(sum, i128::from(total));
        for &(got, remainder, place) in &units {
            for &(other_got, other_remainder, other_place) in &units {
                if got && !other_got {
                    prop_assert!(
                        remainder > other_remainder
                            || (remainder == other_remainder && place < other_place),
                        "share {place} got a unit before share {other_place}"
                    );
                }
            }
        }
        Ok(())
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

        #[test]
        fn allocate_splits_in_two_by_the_rule(total in positive_amounts(), cut in 0_u16..=10_000) {
            check_allocation::<2>(total, rates_from_cuts(vec![cut]))?;
        }

        #[test]
        fn allocate_splits_in_three_by_the_rule(
            total in positive_amounts(),
            cuts in prop::collection::vec(0_u16..=10_000, 2),
        ) {
            check_allocation::<3>(total, rates_from_cuts(cuts))?;
        }

        #[test]
        fn allocate_splits_in_five_by_the_rule(
            total in prop_oneof![1_i64..=20, positive_amounts()],
            cuts in prop::collection::vec(0_u16..=10_000, 4),
        ) {
            // Small totals leave several units over, so their order counts.
            check_allocation::<5>(total, rates_from_cuts(cuts))?;
        }
    }
}
