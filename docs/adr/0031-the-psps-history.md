<!-- Author: Nicola Antonello -->

# ADR-0031: Duplicates and order: reading the PSP's history

- Status: accepted
- Date: 2026-11-07

## Context

The failure model assumes that the PSP's notifications arrive late, twice, out of
order, or never. Duplicates come in two kinds. The same event can be delivered again,
after a `2xx` was lost; CloudEvents states the rule that ADR-0005 follows: "Producers
MUST ensure that `source` + `id` is unique for each distinct event", and "Consumers MAY
assume that Events with identical `source` and `id` are duplicates". And one fact can
arrive in two different events: Stripe warns that "two separate Event objects are
generated and sent", to be matched by the object's ID and the event's type.

Order is the harder part, because the table of moves (ADR-0016) is not indifferent to
it. From `captured`, both `Settle` and `FinalRefund` are legal, and after a final refund
nothing is. ADR-0016 also decides what a settlement after a full refund means: the
payment "stays `refunded`, and phase 6 records the settlement". So suppose that the PSP
settles a payment and then refunds it in full, and the refund's notification arrives
first. Applying events as they come records the refund at once, since it is legal, and
then reads the settlement as one that followed a full refund. Our history would invert
the PSP's order and lose its `settle` step. No rule that waits for an event to become
legal can catch this, because both events are legal when they arrive. Every other pair
of events legal from one status either excludes the other at the PSP, as authorize and
decline do, or gives the same result in either order, as refunds do, since the domain
picks partial or final from what is left when one applies.

This is the CALM theorem in miniature: "the programs that have consistent,
coordination-free distributed implementations are exactly the programs that can be
expressed in monotonic logic". A final refund closes a door, which is not monotonic, so
something must supply the order, and only the PSP knows it.

## Options

| Option | Why not |
|---|---|
| **Thin notifications; read the PSP's history of the payment, and apply what we lack, in the PSP's order** | Chosen |
| Apply each event when the table of moves allows it, and park it otherwise | Wrong when a settlement and a full refund cross, as above. Bolt-on causal consistency would need each event to name what it depends on: "explicit causality relationships" |
| A sequence number per payment, with events applied strictly in order, as a Resequencer or Kafka's idempotent producer does | Right, but a missing event blocks every later one until it is delivered again, and one lost after the PSP stops retrying (three days at Stripe, 30 at Adyen) blocks the payment for good, unless we can fetch it, which is the chosen option's read |
| Answer non-`2xx` until an event applies, and let the PSP's retries hold it | The first option's flaw, since the refund applies at once, with our endpoint tied to the PSP's schedule, and Standard Webhooks advises disabling an endpoint that keeps failing |

## Decision

- **Notifications are thin.** One holds its event ID, its type and our payment's ID, and
  no amount or status. Stripe: "For new integrations, use thin events", and use them when
  "Data integrity is critical". Fowler calls the pattern Event Notification: "often just
  some id information and a link back to the sender that can be queried for more
  information". A notification is recorded once, by `(psp, event_id)` (ADR-0005), in an
  append-only table. It may name a payment that we do not know, so it does not reference
  `payments`.
- **The PSP's history is the source of truth.** It lists a payment's facts in the PSP's
  order, read by our payment's ID, which the authorization request carries as its
  reference. Each fact has the PSP's own ID, a position (1, 2, 3, ...), a kind, and what
  it decides: the `RequestId` it answers, its amounts, and the PSP's business times, such
  as an authorization's `expires_at` (ADR-0022).
- **Catching up** is the one way a PSP fact reaches a payment:
  1. read the history over HTTP, outside any transaction (ADR-0026);
  2. take the payment's lock (ADR-0018);
  3. check that the facts we hold are the PSP's first facts, ID for ID, and stop the
     payment for an operator if they are not;
  4. apply each fact after our last position, in order;
  5. commit through the one function of ADR-0014.

  It runs after a notification is recorded (ADR-0032), in the sender's lookup
  (ADR-0028), and in a sweep over the payments that wait for the PSP: one with a request
  not yet decided, an authorization past its `expires_at`, or a capture not yet settled.
  That sweep is the one ADR-0016 meant by "a sweep that asks it". Its cadence for each
  case is set in the implementation and measured in phase 8.
- **What a fact does,** decided under the lock:
  - it moves the payment: a step through the table of moves, with its record and
    posting (ADR-0017, ADR-0020), by the library's commands, including `decline`,
    `expire` and `settle`, which ADR-0023 kept for this phase;
  - it closes a request without a step: a refusal, such as a capture refused because the
    authorization lapsed;
  - it is recorded without a step, as ADR-0016 decides for a settlement after a full
    refund;
  - or it stops the payment at its position, flagged for an operator, when the model has
    no place for it or it contradicts what we hold: a capture that fails after it
    succeeded (ADR-0028), or a decision on a request that we never sent. It is never
    skipped, since skipping would break the prefix below.
- **What we hold is a prefix of the PSP's history,** and every catch-up checks it. Each
  recorded fact carries its position, under the primary key `(payment_id, position)` and
  a foreign key to `(payment_id, position - 1)`, as each step names its predecessor
  (ADR-0017). The PSP's ID for a fact is unique per PSP. Facts are append-only (`LG001`) and written by
  `payments_app` (ADR-0025). A fact that moves a payment is tied to its step by a row keyed
  by the step, as ADR-0017 planned for "Later facts about a step", and ADR-0034 checks
  that every step of the PSP's has one.
- **One decision per request,** under a unique key on the request that a fact answers.
- **Acknowledgements stay apart** (ADR-0028), per request and outside the chain, because
  they move nothing.
- **The test of order** is a property test. It builds a legal history of the PSP by
  walking the table of moves, delivers its notifications duplicated, reordered and partly
  lost, and runs sweeps. At every point our facts are a prefix of the history, and at the
  end they equal it.

## Consequences

- One read of the PSP per notification or sweep, which phase 8 measures.
- While the PSP cannot be read, facts wait; notifications are still recorded and answered
  (ADR-0032).
- The fake serves an ordered history per payment (ADR-0029), and that is a modelling
  choice. Stripe's thin events point to "the latest state of the related resource"; what
  Stripe's or Adyen's APIs return for one payment, and whether either lists its history in
  order, is not verified.
- A notification decides nothing, which shrinks what a forged one can do (ADR-0030).
- I8 rests on this record (ADR-0034).

## References

- Stripe, [How events work](https://docs.stripe.com/events/how-events-work), and [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks).
- Adyen, [Handle webhook events](https://docs.adyen.com/development-resources/webhooks/handle-webhook-events) (docs).
- [CloudEvents specification](https://github.com/cloudevents/spec/blob/v1.0.2/cloudevents/spec.md) (v1.0.2), the `id` attribute.
- Martin Fowler, [What do you mean by "Event-Driven"?](https://martinfowler.com/articles/201701-event-driven.html) (2017).
- Joseph M. Hellerstein and Peter Alvaro, [Keeping CALM: When Distributed Consistency Is Easy](https://doi.org/10.1145/3369736) (Communications of the ACM, 2020).
- Peter Bailis, Ali Ghodsi, Joseph M. Hellerstein and Ion Stoica, [Bolt-on Causal Consistency](https://doi.org/10.1145/2463676.2465279) (SIGMOD, 2013).
- Gregor Hohpe and Bobby Woolf, [Resequencer](https://www.enterpriseintegrationpatterns.com/patterns/messaging/Resequencer.html) (Enterprise Integration Patterns).
- Guozhang Wang et al., [Consistency and Completeness: Rethinking Distributed Stream Processing in Apache Kafka](https://doi.org/10.1145/3448016.3457556) (SIGMOD, 2021), and [KIP-98: Exactly Once Delivery and Transactional Messaging](https://cwiki.apache.org/confluence/display/KAFKA/KIP-98+-+Exactly+Once+Delivery+and+Transactional+Messaging) (Apache Kafka).
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): thin payloads, and disabling an endpoint.
