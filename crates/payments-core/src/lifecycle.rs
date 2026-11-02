// Author: Nicola Antonello

//! The payment lifecycle (ADR-0016). Everything here is pure: the database's
//! copy of the rules (ADR-0017) and the commands that use them (ADR-0019) live
//! elsewhere.

use std::fmt;

/// A label that the database should never hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidLabel {
    #[error("{0:?} is not a payment status")]
    PaymentStatus(String),
    #[error("{0:?} is not a step kind")]
    StepKind(String),
}

/// Declares an enum together with `ALL`, the list of its variants, so that
/// the list cannot miss one. The tests iterate `ALL` to check every pair of
/// status and event, and a variant left out would go untested (ADR-0016).
macro_rules! enum_with_all {
    ($(#[$meta:meta])* $name:ident { $($(#[$variant_meta:meta])* $variant:ident,)+ }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];
        }
    };
}

enum_with_all! {
    /// Where a payment stands. It is the status its newest step moved it to,
    /// and no other copy of it exists (ADR-0017).
    PaymentStatus {
        /// The payment's terms are recorded, and the PSP has decided nothing
        /// yet.
        Created,
        /// The PSP holds an authorized amount, at most the requested one.
        Authorized,
        /// The PSP declined the authorization, and only a definite decline
        /// leads here. Final.
        Failed,
        /// No money will move: the payment was cancelled before authorization,
        /// or its authorization was voided or expired. Final.
        Voided,
        /// The PSP captured an amount, at most the authorized one, and
        /// released the rest of the hold.
        Captured,
        /// The PSP reported the capture as settled.
        Settled,
        /// Refunds have returned the whole captured amount. Final.
        Refunded,
    }
}

enum_with_all! {
    /// An outcome that the PSP reported or the caller decided. It is never
    /// stored: a step records its [`StepKind`].
    Event {
        Authorize,
        Decline,
        Cancel,
        Capture,
        Void,
        Expire,
        Settle,
        /// A refund of less than the refundable amount.
        PartialRefund,
        /// A refund of exactly the refundable amount.
        FinalRefund,
    }
}

enum_with_all! {
    /// What a step in a payment's history records, so a `voided` payment
    /// still shows whether it was cancelled, voided or expired.
    StepKind {
        /// A payment's first step.
        Create,
        Authorize,
        Decline,
        Cancel,
        Capture,
        Void,
        Expire,
        Settle,
        Refund,
    }
}

impl PaymentStatus {
    /// The label the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Authorized => "authorized",
            Self::Failed => "failed",
            Self::Voided => "voided",
            Self::Captured => "captured",
            Self::Settled => "settled",
            Self::Refunded => "refunded",
        }
    }

    /// Parses the label the database stores.
    ///
    /// # Errors
    ///
    /// [`InvalidLabel::PaymentStatus`] for anything else.
    pub fn parse(label: &str) -> Result<Self, InvalidLabel> {
        Self::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == label)
            .ok_or_else(|| InvalidLabel::PaymentStatus(label.to_owned()))
    }
}

impl StepKind {
    /// The label the database stores. SQL binds it rather than spelling it
    /// (ADR-0009).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Authorize => "authorize",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
            Self::Capture => "capture",
            Self::Void => "void",
            Self::Expire => "expire",
            Self::Settle => "settle",
            Self::Refund => "refund",
        }
    }

    /// Parses the label the database stores.
    ///
    /// # Errors
    ///
    /// [`InvalidLabel::StepKind`] for anything else.
    pub fn parse(label: &str) -> Result<Self, InvalidLabel> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == label)
            .ok_or_else(|| InvalidLabel::StepKind(label.to_owned()))
    }
}

impl fmt::Display for PaymentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// An event that a payment's status does not allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a payment that is {status} cannot take the event {event:?}")]
pub struct IllegalTransition {
    pub status: PaymentStatus,
    pub event: Event,
}

/// A legal step of a payment's history. Its fields are private, so only
/// [`next`] and [`Transition::genesis`] can make one, and the only function
/// that writes a step takes one: every step written comes from the table
/// (ADR-0016).
///
/// ```
/// use payments_core::lifecycle::{Event, PaymentStatus, StepKind, next};
///
/// let step = next(PaymentStatus::Authorized, Event::Capture).expect("a legal move");
/// assert_eq!(step.kind(), StepKind::Capture);
/// assert_eq!(step.to(), PaymentStatus::Captured);
/// ```
///
/// ```compile_fail
/// use payments_core::lifecycle::{PaymentStatus, StepKind, Transition};
///
/// let forged = Transition { // private fields
///     from: PaymentStatus::Created,
///     kind: StepKind::Capture,
///     to: PaymentStatus::Captured,
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    from: PaymentStatus,
    kind: StepKind,
    to: PaymentStatus,
}

impl Transition {
    /// A new payment's first step, the one move that [`next`] cannot make,
    /// because no status comes before it.
    #[must_use]
    pub fn genesis() -> Self {
        Self {
            from: PaymentStatus::Created,
            kind: StepKind::Create,
            to: PaymentStatus::Created,
        }
    }

    /// The status the step starts from.
    pub fn from(self) -> PaymentStatus {
        self.from
    }

    /// What the step records.
    pub fn kind(self) -> StepKind {
        self.kind
    }

    /// The status the step moves the payment to.
    pub fn to(self) -> PaymentStatus {
        self.to
    }
}

/// The table of moves (ADR-0016): where a payment in `status` goes when `event`
/// happens. The match has no catch-all arm, so a new status or event fails to
/// compile (error E0004) until every pair it creates is decided.
///
/// # Errors
///
/// [`IllegalTransition`] when the status does not allow the event.
pub fn next(status: PaymentStatus, event: Event) -> Result<Transition, IllegalTransition> {
    use Event as E;
    use PaymentStatus as S;
    use StepKind as K;
    let (kind, to) = match (status, event) {
        (S::Created, E::Authorize) => (K::Authorize, S::Authorized),
        (S::Created, E::Decline) => (K::Decline, S::Failed),
        (S::Created, E::Cancel) => (K::Cancel, S::Voided),
        (S::Authorized, E::Capture) => (K::Capture, S::Captured),
        (S::Authorized, E::Void) => (K::Void, S::Voided),
        (S::Authorized, E::Expire) => (K::Expire, S::Voided),
        (S::Captured, E::Settle) => (K::Settle, S::Settled),
        (S::Captured, E::PartialRefund) => (K::Refund, S::Captured),
        (S::Settled, E::PartialRefund) => (K::Refund, S::Settled),
        (S::Captured | S::Settled, E::FinalRefund) => (K::Refund, S::Refunded),
        (
            S::Created,
            E::Capture | E::Void | E::Expire | E::Settle | E::PartialRefund | E::FinalRefund,
        )
        | (
            S::Authorized,
            E::Authorize | E::Decline | E::Cancel | E::Settle | E::PartialRefund | E::FinalRefund,
        )
        | (S::Captured, E::Authorize | E::Decline | E::Cancel | E::Capture | E::Void | E::Expire)
        | (
            S::Settled,
            E::Authorize | E::Decline | E::Cancel | E::Capture | E::Void | E::Expire | E::Settle,
        )
        | (
            S::Failed | S::Voided | S::Refunded,
            E::Authorize
            | E::Decline
            | E::Cancel
            | E::Capture
            | E::Void
            | E::Expire
            | E::Settle
            | E::PartialRefund
            | E::FinalRefund,
        ) => return Err(IllegalTransition { status, event }),
    };
    Ok(Transition {
        from: status,
        kind,
        to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_has_its_label() {
        // Written out, not taken from as_str: the database's CHECKs spell the
        // same labels.
        let labels = [
            (PaymentStatus::Created, "created"),
            (PaymentStatus::Authorized, "authorized"),
            (PaymentStatus::Failed, "failed"),
            (PaymentStatus::Voided, "voided"),
            (PaymentStatus::Captured, "captured"),
            (PaymentStatus::Settled, "settled"),
            (PaymentStatus::Refunded, "refunded"),
        ];
        assert_eq!(PaymentStatus::ALL.len(), labels.len());
        for (status, label) in labels {
            assert_eq!(status.as_str(), label);
            assert_eq!(PaymentStatus::parse(label), Ok(status));
        }
    }

    #[test]
    fn every_step_kind_has_its_label() {
        let labels = [
            (StepKind::Create, "create"),
            (StepKind::Authorize, "authorize"),
            (StepKind::Decline, "decline"),
            (StepKind::Cancel, "cancel"),
            (StepKind::Capture, "capture"),
            (StepKind::Void, "void"),
            (StepKind::Expire, "expire"),
            (StepKind::Settle, "settle"),
            (StepKind::Refund, "refund"),
        ];
        assert_eq!(StepKind::ALL.len(), labels.len());
        for (kind, label) in labels {
            assert_eq!(kind.as_str(), label);
            assert_eq!(StepKind::parse(label), Ok(kind));
        }
    }

    #[test]
    fn the_lists_hold_each_variant_once() {
        assert_eq!(PaymentStatus::ALL.len(), 7);
        assert_eq!(StepKind::ALL.len(), 9);
        assert_eq!(
            Event::ALL,
            [
                Event::Authorize,
                Event::Decline,
                Event::Cancel,
                Event::Capture,
                Event::Void,
                Event::Expire,
                Event::Settle,
                Event::PartialRefund,
                Event::FinalRefund,
            ]
        );
    }

    #[test]
    fn other_labels_are_refused() {
        for label in ["", "Created", "created ", "pending", "refund"] {
            assert_eq!(
                PaymentStatus::parse(label),
                Err(InvalidLabel::PaymentStatus(label.into())),
                "{label:?}"
            );
        }
        for label in ["", "Capture", "capture ", "captured", "partial_refund"] {
            assert_eq!(
                StepKind::parse(label),
                Err(InvalidLabel::StepKind(label.into())),
                "{label:?}"
            );
        }
    }

    #[test]
    fn errors_name_the_value_and_the_rule() {
        for (error, text) in [
            (
                InvalidLabel::PaymentStatus("pending".into()),
                r#""pending" is not a payment status"#,
            ),
            (
                InvalidLabel::StepKind("captured\n".into()),
                r#""captured\n" is not a step kind"#,
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
        let illegal = IllegalTransition {
            status: PaymentStatus::Refunded,
            event: Event::PartialRefund,
        };
        assert_eq!(
            illegal.to_string(),
            "a payment that is refunded cannot take the event PartialRefund"
        );
    }

    /// ADR-0016's moves, written out independently of `next`: from, event,
    /// step kind, to. Frozen: later phases add moves and never change these,
    /// so a new move joins the table test through a list of its own.
    const PHASE_2_EDGES: [(PaymentStatus, Event, StepKind, PaymentStatus); 11] = {
        use Event as E;
        use PaymentStatus as S;
        use StepKind as K;
        [
            (S::Created, E::Authorize, K::Authorize, S::Authorized),
            (S::Created, E::Decline, K::Decline, S::Failed),
            (S::Created, E::Cancel, K::Cancel, S::Voided),
            (S::Authorized, E::Capture, K::Capture, S::Captured),
            (S::Authorized, E::Void, K::Void, S::Voided),
            (S::Authorized, E::Expire, K::Expire, S::Voided),
            (S::Captured, E::Settle, K::Settle, S::Settled),
            (S::Captured, E::PartialRefund, K::Refund, S::Captured),
            (S::Captured, E::FinalRefund, K::Refund, S::Refunded),
            (S::Settled, E::PartialRefund, K::Refund, S::Settled),
            (S::Settled, E::FinalRefund, K::Refund, S::Refunded),
        ]
    };

    #[test]
    fn every_pair_of_status_and_event_follows_the_table() {
        let mut pairs = 0;
        for &status in PaymentStatus::ALL {
            for &event in Event::ALL {
                pairs += 1;
                let edge = PHASE_2_EDGES
                    .iter()
                    .find(|edge| (edge.0, edge.1) == (status, event));
                match (next(status, event), edge) {
                    (Ok(step), Some(&(from, _, kind, to))) => {
                        assert_eq!((step.from(), step.kind(), step.to()), (from, kind, to));
                    }
                    (Err(illegal), None) => {
                        assert_eq!(illegal, IllegalTransition { status, event });
                    }
                    (got, want) => panic!("{status:?}, {event:?}: {got:?}, expected {want:?}"),
                }
            }
        }
        assert_eq!(pairs, 63);
    }

    #[test]
    fn the_phase_2_edges_stay_legal() {
        for (from, event, kind, to) in PHASE_2_EDGES {
            let step = next(from, event).expect("a phase-2 edge stays legal");
            assert_eq!((step.from(), step.kind(), step.to()), (from, kind, to));
        }
    }

    #[test]
    fn a_final_status_takes_no_event() {
        for status in [
            PaymentStatus::Failed,
            PaymentStatus::Voided,
            PaymentStatus::Refunded,
        ] {
            for &event in Event::ALL {
                assert_eq!(
                    next(status, event),
                    Err(IllegalTransition { status, event })
                );
            }
        }
    }

    #[test]
    fn a_payment_starts_created() {
        let first = Transition::genesis();
        assert_eq!(first.kind(), StepKind::Create);
        assert_eq!(first.from(), PaymentStatus::Created);
        assert_eq!(first.to(), PaymentStatus::Created);
    }
}
