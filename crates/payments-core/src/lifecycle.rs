// Author: Nicola Antonello

//! The payment lifecycle (ADR-0016). Everything here is pure: the database's
//! copy of the rules (ADR-0017) and the commands that use them (ADR-0019) live
//! elsewhere.

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
    }
}
