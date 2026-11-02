// Author: Nicola Antonello

//! The payment lifecycle (ADR-0016). Everything here is pure: the database's
//! copy of the rules (ADR-0017) and the commands that use them (ADR-0019) live
//! elsewhere.

use std::fmt;

use crate::money::PositiveAmount;

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

/// Why a move with an amount is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The status does not allow the event, whatever the amount.
    #[error(transparent)]
    Illegal(#[from] IllegalTransition),
    /// The amount is above its limit.
    #[error("amount {amount} is above its limit of {limit}")]
    AboveLimit { amount: i64, limit: i64 },
}

/// The move for an authorization of `authorized`, which may be less than the
/// `requested` amount but not more.
///
/// # Errors
///
/// See [`Refused`].
pub fn authorize(
    status: PaymentStatus,
    authorized: PositiveAmount,
    requested: PositiveAmount,
) -> Result<Transition, Refused> {
    within_limit(status, Event::Authorize, authorized, requested.get())
}

/// The move for a capture of `captured`, which may be less than the
/// `authorized` amount but not more.
///
/// # Errors
///
/// See [`Refused`].
pub fn capture(
    status: PaymentStatus,
    captured: PositiveAmount,
    authorized: PositiveAmount,
) -> Result<Transition, Refused> {
    within_limit(status, Event::Capture, captured, authorized.get())
}

/// The move for a refund of `amount`: partial below the `refundable` amount,
/// final at it. An amount above it is tried as a final refund, so that a
/// status that takes no refund is refused for that reason.
///
/// # Errors
///
/// See [`Refused`].
pub fn refund(
    status: PaymentStatus,
    amount: PositiveAmount,
    refundable: i64,
) -> Result<Transition, Refused> {
    let event = if amount.get() < refundable {
        Event::PartialRefund
    } else {
        Event::FinalRefund
    };
    within_limit(status, event, amount, refundable)
}

/// The status first, then the amount, which matters only where the status
/// allows the event (ADR-0016).
fn within_limit(
    status: PaymentStatus,
    event: Event,
    amount: PositiveAmount,
    limit: i64,
) -> Result<Transition, Refused> {
    let step = next(status, event)?;
    let amount = amount.get();
    if amount > limit {
        return Err(Refused::AboveLimit { amount, limit });
    }
    Ok(step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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

    fn amount(minor: i64) -> PositiveAmount {
        PositiveAmount::new(minor).expect("valid amount")
    }

    #[test]
    fn an_authorization_is_at_most_the_request() {
        let status = PaymentStatus::Created;
        for authorized in [1, 99, 100] {
            let step = authorize(status, amount(authorized), amount(100)).expect("within limit");
            assert_eq!(step.kind(), StepKind::Authorize);
            assert_eq!(step.to(), PaymentStatus::Authorized);
        }
        assert_eq!(
            authorize(status, amount(101), amount(100)),
            Err(Refused::AboveLimit {
                amount: 101,
                limit: 100
            })
        );
    }

    #[test]
    fn a_capture_is_at_most_the_authorization() {
        let status = PaymentStatus::Authorized;
        for captured in [1, 99, 100] {
            let step = capture(status, amount(captured), amount(100)).expect("within limit");
            assert_eq!(step.kind(), StepKind::Capture);
            assert_eq!(step.to(), PaymentStatus::Captured);
        }
        assert_eq!(
            capture(status, amount(101), amount(100)),
            Err(Refused::AboveLimit {
                amount: 101,
                limit: 100
            })
        );
    }

    #[test]
    fn a_refund_keeps_the_status_until_it_reaches_the_limit() {
        for status in [PaymentStatus::Captured, PaymentStatus::Settled] {
            for partial in [1, 99] {
                let step = refund(status, amount(partial), 100).expect("within limit");
                assert_eq!((step.kind(), step.to()), (StepKind::Refund, status));
            }
            let last = refund(status, amount(100), 100).expect("at the limit");
            assert_eq!(
                (last.kind(), last.to()),
                (StepKind::Refund, PaymentStatus::Refunded)
            );
            assert_eq!(
                refund(status, amount(101), 100),
                Err(Refused::AboveLimit {
                    amount: 101,
                    limit: 100
                })
            );
        }
    }

    #[test]
    fn the_status_is_judged_before_the_amount() {
        // Each amount is above its limit, and each status refuses the event
        // whatever the amount: the answer is the illegal move (ADR-0016).
        let illegal = |status, event| Err(Refused::Illegal(IllegalTransition { status, event }));
        assert_eq!(
            refund(PaymentStatus::Refunded, amount(1), 0),
            illegal(PaymentStatus::Refunded, Event::FinalRefund)
        );
        assert_eq!(
            refund(PaymentStatus::Created, amount(1), 0),
            illegal(PaymentStatus::Created, Event::FinalRefund)
        );
        assert_eq!(
            capture(PaymentStatus::Created, amount(2), amount(1)),
            illegal(PaymentStatus::Created, Event::Capture)
        );
        assert_eq!(
            authorize(PaymentStatus::Authorized, amount(2), amount(1)),
            illegal(PaymentStatus::Authorized, Event::Authorize)
        );
    }

    #[test]
    fn a_refusal_names_the_amount_and_its_limit() {
        let above = Refused::AboveLimit {
            amount: 101,
            limit: 100,
        };
        assert_eq!(above.to_string(), "amount 101 is above its limit of 100");
        let illegal = Refused::Illegal(IllegalTransition {
            status: PaymentStatus::Voided,
            event: Event::Capture,
        });
        assert_eq!(
            illegal.to_string(),
            "a payment that is voided cannot take the event Capture"
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum Command {
        Authorize(i64),
        Decline,
        Cancel,
        Capture(i64),
        Void,
        Expire,
        Settle,
        Refund(i64),
    }

    /// What a command should do, according to the model.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        Applied,
        Illegal,
        AboveLimit,
    }

    /// ADR-0016's rules stated independently of `next`: what has happened to a
    /// payment, as facts. It has no table of moves, and it derives a payment's
    /// status and what the payment may do from these facts alone.
    #[derive(Debug, Default)]
    struct Facts {
        authorized: Option<i64>,
        declined: bool,
        released: bool,
        captured: Option<i64>,
        settled: bool,
        refunded: i64,
    }

    impl Facts {
        fn undecided(&self) -> bool {
            self.authorized.is_none() && !self.declined && !self.released
        }

        fn holding(&self) -> bool {
            self.authorized.is_some() && self.captured.is_none() && !self.released
        }

        /// What is left to refund, if anything.
        fn refundable(&self) -> Option<i64> {
            self.captured
                .map(|captured| captured.checked_sub(self.refunded).expect("small amounts"))
                .filter(|left| *left > 0)
        }

        fn status(&self) -> PaymentStatus {
            if self.declined {
                PaymentStatus::Failed
            } else if self.released {
                PaymentStatus::Voided
            } else if self.captured.is_some() {
                match (self.refundable(), self.settled) {
                    (None, _) => PaymentStatus::Refunded,
                    (Some(_), true) => PaymentStatus::Settled,
                    (Some(_), false) => PaymentStatus::Captured,
                }
            } else if self.authorized.is_some() {
                PaymentStatus::Authorized
            } else {
                PaymentStatus::Created
            }
        }

        fn apply(&mut self, command: Command, requested: i64) -> Outcome {
            let (allowed, within_limit) = match command {
                Command::Authorize(minor) => (self.undecided(), minor <= requested),
                Command::Decline | Command::Cancel => (self.undecided(), true),
                Command::Capture(minor) => (
                    self.holding(),
                    self.authorized.is_some_and(|limit| minor <= limit),
                ),
                Command::Void | Command::Expire => (self.holding(), true),
                Command::Settle => (self.refundable().is_some() && !self.settled, true),
                Command::Refund(minor) => (
                    self.refundable().is_some(),
                    self.refundable().is_some_and(|limit| minor <= limit),
                ),
            };
            if !allowed {
                return Outcome::Illegal;
            }
            if !within_limit {
                return Outcome::AboveLimit;
            }
            match command {
                Command::Authorize(minor) => self.authorized = Some(minor),
                Command::Decline => self.declined = true,
                Command::Cancel | Command::Void | Command::Expire => self.released = true,
                Command::Capture(minor) => self.captured = Some(minor),
                Command::Settle => self.settled = true,
                Command::Refund(minor) => {
                    self.refunded = self.refunded.checked_add(minor).expect("small amounts");
                }
            }
            Outcome::Applied
        }
    }

    /// A payment driven through the functions under test, keeping what the
    /// history and its records would hold.
    struct Machine {
        status: PaymentStatus,
        requested: i64,
        authorized: Option<i64>,
        refundable: i64,
    }

    impl Machine {
        fn apply(&mut self, command: Command) -> Result<Transition, Refused> {
            let status = self.status;
            let step = match command {
                Command::Authorize(minor) => {
                    authorize(status, amount(minor), amount(self.requested))?
                }
                Command::Decline => next(status, Event::Decline)?,
                Command::Cancel => next(status, Event::Cancel)?,
                // Before an authorization the status refuses the capture, so
                // this limit is never read.
                Command::Capture(minor) => capture(
                    status,
                    amount(minor),
                    amount(self.authorized.unwrap_or(self.requested)),
                )?,
                Command::Void => next(status, Event::Void)?,
                Command::Expire => next(status, Event::Expire)?,
                Command::Settle => next(status, Event::Settle)?,
                Command::Refund(minor) => refund(status, amount(minor), self.refundable)?,
            };
            match command {
                Command::Authorize(minor) => self.authorized = Some(minor),
                Command::Capture(minor) => self.refundable = minor,
                Command::Refund(minor) => {
                    self.refundable = self.refundable.checked_sub(minor).expect("within limit");
                }
                _ => {}
            }
            self.status = step.to();
            Ok(step)
        }
    }

    /// Commands with amounts around a small request, so that limits are reached
    /// and exact refunds are common. The moves that end a payment are rare,
    /// so that most sequences get far along the lifecycle.
    fn commands() -> impl Strategy<Value = Command> {
        prop_oneof![
            4 => (1_i64..=8).prop_map(Command::Authorize),
            1 => Just(Command::Decline),
            1 => Just(Command::Cancel),
            4 => (1_i64..=8).prop_map(Command::Capture),
            1 => Just(Command::Void),
            1 => Just(Command::Expire),
            2 => Just(Command::Settle),
            6 => (1_i64..=8).prop_map(Command::Refund),
        ]
    }

    proptest! {
        #[test]
        fn random_commands_match_the_model(
            requested in 1_i64..=6,
            commands in prop::collection::vec(commands(), 0..40),
        ) {
            let mut machine = Machine {
                status: Transition::genesis().to(),
                requested,
                authorized: None,
                refundable: 0,
            };
            let mut facts = Facts::default();
            for command in commands {
                let before = machine.status;
                let got = machine.apply(command);
                let want = facts.apply(command, requested);
                match (&got, want) {
                    (Ok(step), Outcome::Applied) => prop_assert_eq!(step.from(), before),
                    (Err(Refused::Illegal(_)), Outcome::Illegal)
                    | (Err(Refused::AboveLimit { .. }), Outcome::AboveLimit) => {
                        prop_assert_eq!(machine.status, before);
                    }
                    _ => prop_assert!(
                        false,
                        "{command:?} from {before}: {got:?}, but the model says {want:?}"
                    ),
                }
                prop_assert_eq!(machine.status, facts.status(), "after {:?}", command);
                prop_assert_eq!(machine.refundable, facts.refundable().unwrap_or(0));
            }
        }
    }
}
