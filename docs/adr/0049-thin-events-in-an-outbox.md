<!-- Author: Nicola Antonello -->

# ADR-0049: Thin events in an outbox

- Status: accepted
- Date: 2026-11-15

## Context

A merchant learns the outcome of a request by polling it (ADR-0033), until phase 5
publishes events (ADR-0028). I6 promises that a payment's change and its outbound event
commit together. ADR-0035 left phase 5 two questions that this record answers: a separate
outbox table or a relay that reads the history, and how events evolve.

Fowler tells apart two patterns that share a name. An event notification tells another
system that something changed, and the receiver asks the sender for details when it needs
them; event-carried state transfer sends the new state along, so the receiver never has to
ask. Stripe offers both: a thin event carries its type and the resource's ID, a snapshot
event a copy of the resource, and Stripe recommends thin events wherever "Data integrity
is critical". Standard Webhooks allows either: a thin payload "will only include
identifiers to the affected entities". Phase 4 already chose the thin form for the PSP's
notifications, whose `data` holds the payment's ID alone, because a notification decides
nothing (ADR-0031, ADR-0038).

The outbox stores a message "in the database as part of the transaction that updates the
business entities" (Richardson), Helland's advice to enqueue messages in the same
transaction as the change. Event sourcing answers otherwise: the history is the record to
publish, and Kleppmann, Beresford and Svingen build consistency on such append-only logs
instead of on distributed transactions. Here the history is several tables, the steps,
the facts and the decisions, and none of them records the order in which its rows commit
(ADR-0005, ADR-0012).

## Options

What an event carries, and where it is written:

| Option | Why not |
|---|---|
| **A thin event in an outbox table, written with the fact that causes it** | Chosen |
| A snapshot event in an outbox table | A second public form of a payment, beside `docs/api.md`, to version for ever: evolving the events is the first of five challenges that Overeem et al. found in event-sourced systems. A late snapshot can also show an older state than one already delivered, so merchants would need a position from us to order them |
| No outbox table: the relay reads the history | The relay would merge several tables in commit order, and none of them records it. The event's shape would follow internal tables, so a change of schema would be a change of contract |

Its type:

| Option | Why not |
|---|---|
| **One type, `payment.updated`** | Chosen |
| One type per fact, such as `payment.captured` | It invites a merchant to act on the event rather than read the state, the reason ADR-0038 gave for the PSP's notifications |

## Decision

- **One event per fact.** When the catch-up applies a fact of the PSP's history, as a step
  or as a refusal that decides a request, it writes one event in the same transaction
  (ADR-0031, ADR-0041). Creating and cancelling a payment, which the merchant's own
  request answers at once, write none.
- **The row** holds the event's ID, a UUIDv7 made in Rust (`EventId`, ADR-0005), its
  merchant, its payment, the position of its fact, under a unique key and a foreign key to
  that fact, and `recorded_at`. ADR-0052 adds the column that the relay searches by. Every
  column is `NOT NULL` (ADR-0017): the request that a fact decides is found through the
  fact's decision, not stored again.
- **What a merchant receives** follows Standard Webhooks' payload structure:
  `{"type": "payment.updated", "timestamp": ..., "data": {"payment_id": ..., "request_id": ...}}`,
  with `request_id` only when the fact decides a request, and `timestamp` the event's
  `recorded_at`. The body is built from rows that never change, so every attempt sends the
  same bytes.
- **The merchant reads the state** through `GET /payments/{id}` and
  `GET /payments/{id}/requests/{request_id}` (ADR-0033). An event leaves only after its
  transaction commits, and there is one primary (DESIGN.md's non-goals), so the merchant's
  read shows at least the state that the event announces. Copies and a different order
  are harmless: each costs one more read.
- **Only the catch-up writes events,** inside the commands that take `PspWord` (ADR-0048),
  as `payments_app`. The merchant's role has no privilege on the table.
- **A deferred check, `LG008`,** refuses at `COMMIT` a fact without its event, as `LG007`
  refuses a step of the PSP's without its fact (ADR-0013, ADR-0048). It joins the codes
  that prove a rollback (ADR-0014).

## Consequences

- Merchants stop polling. Each event costs them one read, and they drop copies by its ID
  (ADR-0051).
- The catch-up's transaction writes one more row per fact, and the deliveries of
  ADR-0053.
- The event's content needs no version: the API that the merchant reads is the contract,
  documented in `docs/api.md`. Snapshot events, or a type per fact, could join later as
  new event types.
- I6's outbound event is this event (ADR-0054).
- Tests: one event per applied fact, and none for a creation or a cancellation; `LG008`
  rolling back a fact written alone; the same body at every attempt.

## Amendments

- 2026-11-16: "inside the commands that take `PspWord`" means in the catch-up, beside the
  fact that the event announces. A refusal and a settlement recorded without a step make
  no step, so no such command writes their facts (ADR-0057).
- 2026-11-16: the body's `timestamp` is written as the API writes its times: RFC 3339 in
  UTC with `Z`, with as many fractional digits as the instant needs (none, 3 or 6 for the
  database's microseconds), so every read of one event gives the same bytes.

## References

- Martin Fowler, [What do you mean by "Event-Driven"?](https://martinfowler.com/articles/201701-event-driven.html) (2017).
- Stripe, [How events work](https://docs.stripe.com/events/how-events-work) (docs).
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): the payload structure, and thin and full payloads.
- Chris Richardson, [Pattern: Transactional outbox](https://microservices.io/patterns/data/transactional-outbox.html) (microservices.io).
- Pat Helland, [Life beyond Distributed Transactions: an Apostate's Opinion](https://www.cidrdb.org/cidr2007/papers/cidr07p15.pdf) (CIDR 2007).
- Martin Kleppmann, Alastair R. Beresford and Boerge Svingen, [Online Event Processing: Achieving consistency where distributed transactions have failed](https://doi.org/10.1145/3312527) (Communications of the ACM, 2019).
- Michiel Overeem, Marten Spoor, Slinger Jansen and Sjaak Brinkkemper, [An empirical characterization of event sourced systems and their schema evolution — Lessons from industry](https://doi.org/10.1016/j.jss.2021.110970) (Journal of Systems and Software, 2021).
