// Author: Nicola Antonello

//! Account keys (ADR-0009).
//!
//! An account is found by its business key: its kind, the party it is kept
//! for, and its currency. The types here accept exactly what the database's
//! CHECKs accept, so a key built from them is never rejected for its
//! spelling. Phase 2's typed account references become the only producers of
//! keys, and tie each kind to its party (ADR-0009).

use std::fmt;

use uuid::Uuid;

use crate::money::Currency;

/// Why a value cannot describe an account.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidAccount {
    /// Not a lowercase label.
    #[error(
        "kind {0:?} must be a lowercase label: a letter, then up to 62 letters, digits or underscores"
    )]
    Kind(String),
    /// Not a lowercase label, or a label that spells a UUID without hyphens.
    #[error("party label {0:?} must be a lowercase label that is not 32 hexadecimal digits")]
    Party(String),
    /// Neither `debit` nor `credit`.
    #[error(r#"normal side {0:?} must be "debit" or "credit""#)]
    NormalSide(String),
}

/// What an account is: its place in the chart of accounts, such as
/// `fee_revenue` or `merchant_payable`. A lowercase label: a letter, then up
/// to 62 letters, digits or underscores.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountKind(String);

impl AccountKind {
    /// Checks that `kind` is a lowercase label.
    ///
    /// # Errors
    ///
    /// [`InvalidAccount::Kind`] for anything else.
    pub fn parse(kind: &str) -> Result<Self, InvalidAccount> {
        if is_label(kind) {
            Ok(Self(kind.to_owned()))
        } else {
            Err(InvalidAccount::Kind(kind.to_owned()))
        }
    }

    /// The kind as the database stores it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Whom an account is kept for: a lowercase label, such as `platform` for
/// the business's own accounts or a PSP's name, or a merchant's ID. Each
/// party has one spelling: a label never contains a hyphen, which a UUID
/// always has, and is never 32 hexadecimal digits, a UUID without its
/// hyphens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Party(String);

impl Party {
    /// A party named by a lowercase label.
    ///
    /// # Errors
    ///
    /// [`InvalidAccount::Party`] for anything that is not a lowercase label,
    /// or that is 32 hexadecimal digits. A merchant is named by
    /// [`merchant`](Self::merchant) instead.
    pub fn label(label: &str) -> Result<Self, InvalidAccount> {
        if is_label(label) && !is_hyphenless_uuid(label) {
            Ok(Self(label.to_owned()))
        } else {
            Err(InvalidAccount::Party(label.to_owned()))
        }
    }

    /// The party for a merchant, spelled as a canonical lowercase,
    /// hyphenated UUID.
    pub fn merchant(id: Uuid) -> Self {
        Self(id.hyphenated().to_string())
    }

    /// The party as the database stores it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Party {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// An account's business key (ADR-0009): how code finds an account, and
/// what makes opening one idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AccountKey {
    pub kind: AccountKind,
    pub party: Party,
    pub currency: Currency,
}

/// The side on which an account's balance increases (ADR-0003). For display
/// only: it never decides the sign of an entry (ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormalSide {
    Debit,
    Credit,
}

impl NormalSide {
    /// The label the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debit => "debit",
            Self::Credit => "credit",
        }
    }

    /// Parses the label the database stores.
    ///
    /// # Errors
    ///
    /// [`InvalidAccount::NormalSide`] for anything but `debit` or `credit`.
    pub fn parse(side: &str) -> Result<Self, InvalidAccount> {
        match side {
            "debit" => Ok(Self::Debit),
            "credit" => Ok(Self::Credit),
            _ => Err(InvalidAccount::NormalSide(side.to_owned())),
        }
    }
}

/// The database's label rule, `^[a-z][a-z0-9_]{0,62}$`. Every character it
/// allows is ASCII, so it is checked on bytes, and a byte length of 63 is 63
/// characters.
fn is_label(s: &str) -> bool {
    let bytes = s.as_bytes();
    matches!(bytes.first(), Some(b'a'..=b'z'))
        && bytes.len() <= 63
        && bytes
            .iter()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// The database's `^[0-9a-f]{32}$`: a UUID without its hyphens.
fn is_hyphenless_uuid(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use uuid::uuid;

    /// A merchant's ID whose hyphenless form begins with a letter, so it also
    /// passes the label rule except for the 32-digit exclusion.
    const MERCHANT: Uuid = uuid!("d1b2c3d4-e5f6-7a7b-8c9d-0e1f2a3b4c5d");

    /// The longest label (63 characters) and one character more.
    fn longest() -> String {
        format!("a{}", "b".repeat(62))
    }

    fn too_long() -> String {
        format!("a{}", "b".repeat(63))
    }

    /// Strings that break the label rule, each in one way.
    fn not_labels() -> Vec<String> {
        [
            "",
            "Fee",
            "fee_Revenue",
            "1fee",
            "_fee",
            "fee-revenue",
            "fee revenue",
            "fee\n",
            "fée",
            "fee.revenue",
        ]
        .into_iter()
        .map(String::from)
        .chain([too_long()])
        .collect()
    }

    #[test]
    fn a_kind_is_a_lowercase_label() {
        for kind in ["fee_revenue", "merchant_payable", "a", "a1_", "psp2"]
            .into_iter()
            .map(String::from)
            .chain([longest()])
        {
            assert_eq!(
                AccountKind::parse(&kind).map(|k| k.to_string()),
                Ok(kind.clone())
            );
        }
        for kind in not_labels() {
            assert_eq!(
                AccountKind::parse(&kind),
                Err(InvalidAccount::Kind(kind.clone())),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_party_label_is_a_lowercase_label_but_not_a_uuid() {
        for label in ["platform", "mock_psp", "psp2"]
            .into_iter()
            .map(String::from)
            .chain([longest()])
        {
            assert_eq!(
                Party::label(&label).map(|p| p.to_string()),
                Ok(label.clone())
            );
        }
        let hyphenless = MERCHANT.simple().to_string();
        assert_eq!(hyphenless, "d1b2c3d4e5f67a7b8c9d0e1f2a3b4c5d");
        for label in not_labels()
            .into_iter()
            .chain([hyphenless, MERCHANT.hyphenated().to_string()])
        {
            assert_eq!(
                Party::label(&label),
                Err(InvalidAccount::Party(label.clone())),
                "{label:?}"
            );
        }
    }

    #[test]
    fn a_merchant_is_spelled_as_a_canonical_uuid() {
        let party = Party::merchant(MERCHANT);
        assert_eq!(party.as_str(), "d1b2c3d4-e5f6-7a7b-8c9d-0e1f2a3b4c5d");
        // The same ID read from an uppercase or braced spelling.
        let shouted = Uuid::parse_str("{D1B2C3D4-E5F6-7A7B-8C9D-0E1F2A3B4C5D}").expect("a UUID");
        assert_eq!(Party::merchant(shouted), party);
    }

    #[test]
    fn the_normal_side_round_trips_its_label() {
        for (side, label) in [(NormalSide::Debit, "debit"), (NormalSide::Credit, "credit")] {
            assert_eq!(side.as_str(), label);
            assert_eq!(NormalSide::parse(label), Ok(side));
        }
        for label in ["", "Debit", "CREDIT", "debit ", "dr", "cr"] {
            assert_eq!(
                NormalSide::parse(label),
                Err(InvalidAccount::NormalSide(label.into())),
                "{label:?}"
            );
        }
    }

    #[test]
    fn errors_name_the_value_and_the_rule() {
        for (error, text) in [
            (
                InvalidAccount::Kind("Fee".into()),
                r#"kind "Fee" must be a lowercase label: a letter, then up to 62 letters, digits or underscores"#,
            ),
            (
                InvalidAccount::Party("mock-psp".into()),
                r#"party label "mock-psp" must be a lowercase label that is not 32 hexadecimal digits"#,
            ),
            (
                InvalidAccount::NormalSide("dr\n".into()),
                r#"normal side "dr\n" must be "debit" or "credit""#,
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    /// Valid labels, built from the rule: a letter, then up to 62 letters,
    /// digits or underscores.
    fn labels() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_]{0,62}"
    }

    /// A valid label broken in one way: a first character that is not a
    /// letter, a character outside the alphabet, or one character too many.
    fn broken_labels() -> impl Strategy<Value = String> {
        prop_oneof![
            ("[0-9_A-Z-]", "[a-z0-9_]{0,62}").prop_map(|(first, rest)| first + &rest),
            (labels(), "[^a-z0-9_]", any::<prop::sample::Index>()).prop_map(|(label, bad, at)| {
                let mut chars: Vec<char> = label.chars().collect();
                let at = at.index(chars.len());
                chars.insert(at.max(1), bad.chars().next().expect("one char"));
                chars.into_iter().collect()
            }),
            "[a-z][a-z0-9_]{63,70}",
        ]
    }

    proptest! {
        #[test]
        fn valid_labels_are_accepted(label in labels()) {
            prop_assert_eq!(AccountKind::parse(&label).map(|k| k.as_str().to_owned()), Ok(label.clone()));
            let hex = label.len() == 32 && label.bytes().all(|b| b.is_ascii_hexdigit());
            prop_assert_eq!(Party::label(&label).is_ok(), !hex);
        }

        #[test]
        fn broken_labels_are_rejected(label in broken_labels()) {
            prop_assert_eq!(AccountKind::parse(&label), Err(InvalidAccount::Kind(label.clone())));
            prop_assert_eq!(Party::label(&label), Err(InvalidAccount::Party(label.clone())));
        }

        #[test]
        fn no_label_spells_a_merchant(bits in any::<u128>()) {
            // Neither spelling of a UUID passes as a label, so a merchant's
            // party never collides with a labelled one.
            let id = Uuid::from_u128(bits);
            let simple = id.simple().to_string();
            prop_assert_eq!(Party::label(&simple), Err(InvalidAccount::Party(simple.clone())));
            let canonical = Party::merchant(id);
            prop_assert_eq!(canonical.as_str(), id.hyphenated().to_string());
            prop_assert!(Party::label(canonical.as_str()).is_err());
        }
    }
}
