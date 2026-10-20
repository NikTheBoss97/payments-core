// Author: Nicola Antonello

//! Identifiers (ADR-0005).
//!
//! An ID is a UUIDv7 that the caller creates and passes into the ledger's
//! functions, so tests can pass fixed ones (ADR-0005, amendment of
//! 2026-10-20). [`AccountId::now_v7`] and [`TransactionId::now_v7`] are the
//! only places that read the clock. Converting a [`Uuid`] checks version 7
//! and the RFC 9562 variant, exactly what the database's CHECK accepts, so an
//! ID that converts is never rejected by it.
//!
//! The types have no `Ord`, because nothing may rely on the order of IDs
//! (ADR-0005). SQL binds and reads the inner [`Uuid`], through `get` and
//! `TryFrom<Uuid>`, never through a sqlx type override.

use std::fmt;

use uuid::{Uuid, Variant};

/// A UUID that is not a valid ID: not version 7, or not of the RFC 9562
/// variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a UUIDv7 of the RFC 9562 variant")]
pub struct InvalidId(Uuid);

/// The ID of an account. `Copy`, because many entries refer to the same
/// account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId(Uuid);

impl AccountId {
    /// A new ID from the system clock, through [`Uuid::now_v7`].
    #[must_use]
    pub fn now_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// The inner UUID, to bind in SQL.
    pub fn get(self) -> Uuid {
        self.0
    }
}

impl TryFrom<Uuid> for AccountId {
    type Error = InvalidId;

    fn try_from(id: Uuid) -> Result<Self, Self::Error> {
        check(id).map(Self)
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The ID of a ledger transaction. Neither `Copy` nor `Clone`: each posting
/// gets its own ID, and a reused ID whose posting has the same content reads
/// as already applied (ADR-0015), so reusing one must be written out, through
/// `TryFrom<Uuid>`.
///
/// ```
/// use payments_core::ids::TransactionId;
///
/// let id = TransactionId::now_v7();
/// let same = TransactionId::try_from(id.get()).expect("a UUIDv7");
/// assert_eq!(same, id);
/// ```
///
/// ```compile_fail
/// use payments_core::ids::TransactionId;
///
/// let id = TransactionId::now_v7();
/// let copy = id.clone(); // no Clone
/// ```
///
/// ```compile_fail
/// use payments_core::ids::TransactionId;
///
/// let id = TransactionId::now_v7();
/// let moved = id;
/// let again = id; // no Copy: `id` has moved
/// ```
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TransactionId(Uuid);

impl TransactionId {
    /// A new ID from the system clock, through [`Uuid::now_v7`].
    #[must_use]
    pub fn now_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// The inner UUID, to bind in SQL.
    pub fn get(&self) -> Uuid {
        self.0
    }
}

impl TryFrom<Uuid> for TransactionId {
    type Error = InvalidId;

    fn try_from(id: Uuid) -> Result<Self, Self::Error> {
        check(id).map(Self)
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The database's rule, `uuid_extract_version(id) IS NOT DISTINCT FROM 7`:
/// `uuid_extract_version` returns NULL for any variant other than RFC 9562's.
/// `get_version_num` alone reads the version nibble whatever the variant.
fn check(id: Uuid) -> Result<Uuid, InvalidId> {
    if id.get_version_num() == 7 && id.get_variant() == Variant::RFC4122 {
        Ok(id)
    } else {
        Err(InvalidId(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;
    use uuid::uuid;

    /// A UUIDv7 of the RFC 9562 variant, written out.
    const V7: Uuid = uuid!("019b76da-a800-7000-8000-000000000001");

    #[test]
    fn a_uuidv7_of_the_rfc_variant_converts_and_round_trips() {
        assert_eq!(AccountId::try_from(V7).map(AccountId::get), Ok(V7));
        assert_eq!(TransactionId::try_from(V7).map(|id| id.get()), Ok(V7));
    }

    #[test]
    fn other_versions_and_variants_are_rejected() {
        for uuid in [
            Uuid::nil(),
            Uuid::max(),
            // Version 7 with the variant bits of NCS (0), Microsoft (110)
            // and future use (111). uuid's get_version_num reads the version
            // nibble whatever the variant, so it alone would accept these.
            uuid!("019b76da-a800-7000-0000-000000000001"),
            uuid!("019b76da-a800-7000-c000-000000000001"),
            uuid!("019b76da-a800-7000-e000-000000000001"),
            // The RFC 9562 variant with versions 0, 4 and 8.
            uuid!("019b76da-a800-0000-8000-000000000001"),
            uuid!("019b76da-a800-4000-8000-000000000001"),
            uuid!("019b76da-a800-8000-8000-000000000001"),
        ] {
            assert_eq!(AccountId::try_from(uuid), Err(InvalidId(uuid)), "{uuid}");
            assert_eq!(
                TransactionId::try_from(uuid),
                Err(InvalidId(uuid)),
                "{uuid}"
            );
        }
    }

    #[test]
    fn the_clock_gives_valid_distinct_ids() {
        let accounts: Vec<Uuid> = (0..1000).map(|_| AccountId::now_v7().get()).collect();
        let transactions: Vec<Uuid> = (0..1000).map(|_| TransactionId::now_v7().get()).collect();
        for id in accounts.iter().chain(&transactions) {
            assert_eq!(AccountId::try_from(*id).map(AccountId::get), Ok(*id));
            assert_eq!(TransactionId::try_from(*id).map(|t| t.get()), Ok(*id));
        }
        // One process-wide generator serves both types (ADR-0005), so no ID
        // repeats across them either.
        let distinct: HashSet<&Uuid> = accounts.iter().chain(&transactions).collect();
        assert_eq!(distinct.len(), 2000);
    }

    #[test]
    fn ids_display_in_canonical_form() {
        let text = "019b76da-a800-7000-8000-000000000001";
        let account = AccountId::try_from(V7).expect("valid ID");
        let transaction = TransactionId::try_from(V7).expect("valid ID");
        assert_eq!(account.to_string(), text);
        assert_eq!(transaction.to_string(), text);
        assert_eq!(
            InvalidId(Uuid::nil()).to_string(),
            "00000000-0000-0000-0000-000000000000 is not a UUIDv7 of the RFC 9562 variant"
        );
    }

    /// Any 128 bits whose version nibble is 7, and whose variant bits are
    /// 0b10, about half of the time each, so that valid and nearly valid IDs
    /// both come up often. Uniform bits would give a valid one once in 64.
    fn candidate_bits() -> impl Strategy<Value = u128> {
        let version = prop_oneof![Just(7_u128), 0_u128..16];
        let variant = prop_oneof![Just(0b10_u128), 0_u128..4];
        (any::<u128>(), version, variant).prop_map(|(bits, version, variant)| {
            (bits & !(0xf << 76) & !(0b11 << 62)) | (version << 76) | (variant << 62)
        })
    }

    proptest! {
        #[test]
        fn conversion_accepts_exactly_version_7_of_the_rfc_variant(bits in candidate_bits()) {
            // The rule restated with masks, independently of uuid's getters:
            // the version is the top nibble of byte 6, and the variant the
            // top two bits of byte 8 (RFC 9562, section 4).
            let valid = (bits >> 76) & 0xf == 7 && (bits >> 62) & 0b11 == 0b10;
            let uuid = Uuid::from_u128(bits);
            prop_assert_eq!(AccountId::try_from(uuid).is_ok(), valid);
            prop_assert_eq!(TransactionId::try_from(uuid).is_ok(), valid);
        }
    }
}
