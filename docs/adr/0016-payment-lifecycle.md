<!-- Author: Nicola Antonello -->

# ADR-0016: Payment lifecycle

- Status: accepted
- Date: 2026-10-28

## Context

Phase 2 puts payments on top of the ledger. CLAUDE.md names their seven
statuses (created, authorized, captured, settled, failed, voided and
refunded) and invariant I5: a payment never makes an invalid state
transition. What each status means, which events move a payment between
them and which partial operations exist are product decisions, fixed here
before any code.

The card payment lifecycles of Stripe, Adyen, Braintree and Checkout.com,
with PayPal's API as a cross-check, share a core:

- An authorization asks the customer's bank to hold an amount. It is
  approved, possibly for less than requested, or declined. An approved hold
  lapses after a deadline that the PSP reports, but card schemes may expire
  an authorization earlier or later, and a capture made after the scheme's
  deadline can fail days later.
- A capture claims all or part of the held amount, and the rest is
  released. One capture per payment is the default at Stripe and Adyen, and
  the only option for cards at Braintree.
- Refunds return captured money, in one or several parts, each a separate
  record. Stripe, Adyen and Checkout.com refund before settlement.
- Settlement, when the PSP pays out what it collected, is learned later
  from batch reports.

Phase 2 talks to no PSP: the mock PSP and its webhooks arrive in phase 4.
Until then the caller, in practice the tests, reports what the PSP decided.

## Options

| Option | Why not |
|---|---|
| **Seven statuses, nine events, eleven legal moves** | Chosen |
| The same, plus a settle step from `refunded` to `refunded` | One more move, and a partial unique index to allow it once, for a fact that phase 6 records for each settled movement anyway |
| Eleven internal states, such as "captured and partly refunded", mapped onto the seven statuses | No single place lists the legal moves, and settling each refund on its own books settlement in phase 2, ahead of phase 6 |
| A separate `expired` status, as at Adyen and Braintree | Changes the seven statuses that CLAUDE.md names. `voided` with the step kind `expire` records the same fact, as Stripe and Checkout.com do |
| Several authorization attempts per payment, as at Stripe | `failed` would describe an attempt instead of the payment. With one attempt, as at Adyen and Braintree, `failed` is final and a new attempt is a new payment |
| One Rust type per status (typestate) | A payment's status comes from a database row at run time, so a typed value is a snapshot that may already be stale |

## Decision

- **Statuses.**
  - `created`: the payment's terms are recorded, and the PSP has decided
    nothing yet.
  - `authorized`: the PSP holds an authorized amount A, at least 1 and at
    most the requested amount (a partial authorization).
  - `failed`: the PSP declined the authorization. Only a definite decline
    leads here: an unknown outcome leaves the payment `created`.
  - `voided`: no money will move. The payment was cancelled before
    authorization, or its authorization was voided or expired.
  - `captured`: the PSP captured an amount C, at least 1 and at most A,
    and released the rest of the hold.
  - `settled`: the PSP reported the capture as settled. Phase 6 books the
    settlement; phase 2 records only the status.
  - `refunded`: refunds have returned the whole captured amount.

  `failed`, `voided` and `refunded` are final: they accept no event.
- **Moves.** A legal event appends a step to the payment's history
  (ADR-0017). The step's kind records what happened, so a `voided` payment
  still shows whether it was cancelled, voided or expired. Postings are
  decided in ADR-0020.

| From | Event | Step kind | To | Posts | Amount rule |
|---|---|---|---|---|---|
| none | Create | `create` | created | no | |
| created | Authorize | `authorize` | authorized | no | 1 ≤ A ≤ requested |
| created | Decline | `decline` | failed | no | |
| created | Cancel | `cancel` | voided | no | |
| authorized | Capture | `capture` | captured | yes | 1 ≤ C ≤ A |
| authorized | Void | `void` | voided | no | |
| authorized | Expire | `expire` | voided | no | |
| captured | Settle | `settle` | settled | no | |
| captured | PartialRefund | `refund` | captured | yes | 1 ≤ R < refundable |
| captured | FinalRefund | `refund` | refunded | yes | R = refundable |
| settled | PartialRefund | `refund` | settled | yes | 1 ≤ R < refundable |
| settled | FinalRefund | `refund` | refunded | yes | R = refundable |

- **Refunds.** The refundable amount starts at C and falls with each
  refund. A refund command carries only R: the domain compares R with the
  refundable amount to choose PartialRefund or FinalRefund, and rejects a
  larger R, so the moves themselves hold no amounts. 11 of the 63 pairs of
  status and event are legal.
- **Outcomes, not requests.** A step records a decision the PSP has made:
  a `capture` step means that the PSP confirmed the capture. Requests in
  flight at the PSP belong to phase 4 and arrive as additions, under the
  extension rule below.
- **Expiry is the PSP's call.** The authorization stores the deadline the
  PSP reports, `expires_at` (ADR-0022), but the library never refuses a
  capture by its own clock: only the PSP knows whether a late capture
  succeeds. An `Expire` event comes from the PSP, or in phase 4 from a
  sweep that asks it.
- **Settlement after a full refund.** `settle` is legal only from
  `captured`. A capture refunded in full before the PSP settles it stays
  `refunded`, and phase 6 records the settlement of each captured and
  refunded amount on rows of its own.
- **The Rust model.**
  - `PaymentStatus`, `Event` and `StepKind` are enums stored as text labels
    and parsed once, like `NormalSide` (ADR-0009).
  - `next(status, event)` is a `match` on the pair with no catch-all arm,
    so a new variant fails to compile (error E0004) until every pair it
    creates is decided.
  - `next` returns a `Transition` whose fields are private, and the only
    function that writes a step takes a `Transition`. A `compile_fail`
    doctest checks that code outside the module cannot build one, so every
    step written comes from the table.
  - The amount rules are small pure functions beside `next`, applied only
    where `next` allows the event: a refund of a `refunded` payment is an
    illegal transition, not an amount error.
  - Each enum's `ALL` list, which the tests iterate, is kept complete by a
    convention that review enforces: every arm of the enum's exhaustive
    `match` carries a `const` assertion that its variant is in `ALL`. Only
    the exhaustive `match` is a compiler guarantee.
- **Extension rule.** Later phases add statuses, events, step kinds and
  moves, and never remove or change one. A test,
  `the_phase_2_edges_stay_legal`, lists today's legal moves and fails if one
  disappears. No key makes "one capture per payment" permanent, so a
  capture reversal, a recapture after the PSP reports a failed capture, and
  multicapture can all be added later.
- **Not in phase 2,** each named so that its absence is a decision:
  chargebacks and disputes; merchant payouts; PSP fees and settlement
  postings (phase 6); multicapture, incremental authorization and
  overcapture; voiding a captured payment; settlement reversal; requests in
  flight at the PSP (phase 4); a fixed fee per payment; currency
  conversion; HTTP endpoints (phase 3, with idempotency keys).

## Consequences

- PostgreSQL holds a second copy of the moves, as a `CHECK` on each step's
  (from, kind, to) (ADR-0017). Tests tie the two together: one checks all
  63 pairs against an expected table, and one tries every one of the 441
  triples of status, step kind and status against the database as the
  runtime role.
- Property tests run random sequences of commands against a small model of
  the lifecycle and compare the outcomes.
- A payment abandoned before authorization can be cancelled, so none has to
  stay `created` forever.
- The history of a payment refunded before settlement does not show the
  later settlement; phase 6's settlement rows do.
- Until phase 4, nothing records a request that is in flight at the PSP,
  because phase 2 sends none.

## References

- David Harel, [Statecharts: A Visual Formalism for Complex Systems](https://doi.org/10.1016/0167-6423%2887%2990035-9) (Science of Computer Programming, 1987).
- Simon Van Mierlo and Hans Vangheluwe, [Statecharts: A Formalism to Model, Simulate and Synthesize Reactive and Autonomous Timed Systems](https://doi.org/10.1007/978-3-030-43946-0_6) (Springer, 2020).
- Ross Anderson, [Security Engineering, 3rd edition](https://www.cl.cam.ac.uk/archive/rja14/book.html) (Wiley, 2020), chapter 12 (the Clark-Wilson model; authorization and settlement).
- Committee on Payments and Market Infrastructures, [CPMI glossary](https://www.bis.org/committees/cpmi/glossary) (authorisation, settlement).
- Adyen, [Payments lifecycle](https://docs.adyen.com/account/payments-lifecycle) and [Authorisation validity](https://docs.adyen.com/online-payments/adjust-authorisation#validity).
- Stripe, [Place a hold on a payment method](https://docs.stripe.com/payments/place-a-hold-on-a-payment-method) (authorization expiry, partial capture).
- Matt Noonan, [Ghosts of Departed Proofs (Functional Pearl)](https://doi.org/10.1145/3242744.3242755) (Haskell Symposium, 2018).
- Rust compiler error index, [E0004](https://doc.rust-lang.org/error_codes/E0004.html) (non-exhaustive patterns).
- The rustdoc book, [Documentation tests](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html) (`compile_fail`).
