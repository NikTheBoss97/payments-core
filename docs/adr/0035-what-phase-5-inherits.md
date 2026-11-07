<!-- Author: Nicola Antonello -->

# ADR-0035: What phase 5 inherits

- Status: accepted
- Date: 2026-11-07

## Context

Phase 5 builds the transactional outbox, which publishes the service's events reliably:
I6's "outbound event" commits with the state change that causes it. The pattern stores
a message "in the database as part of the transaction that updates the business
entities", and a relay sends it. The relay "might publish a message more than once", so
"a message consumer must be idempotent", for example by recording "the IDs of processed
messages in the database".

Phase 4 already builds two specialised versions of that pattern. The table of requests
and the sender are an outbox for commands to the PSP, relayed by polling (ADR-0028). The
recorded notifications and the catch-up are an idempotent consumer, whose processed IDs
are `(psp, event_id)` and the positions of facts (ADR-0031).

Event sourcing answers the same question another way, and the outbox pattern calls it
"an alternative solution". It makes the events the only record: "Capture all changes to
an application state as a sequence of events" (Fowler), and "Since saving an event is a
single operation, it is inherently atomic". The service already uses its core idea where
it counts: balances are derived from entries (ADR-0002), a payment's status from its
history (ADR-0017), and in phase 4 a request's state from its rows (ADR-0033), while
PostgreSQL's constraints check every append.

## Options

| Option | Why not |
|---|---|
| **Build only phase 4's pieces, and name what phase 5 may reuse** | Chosen |
| A generic outbox now: one table of messages with a destination and a payload, one relay, and requests to the PSP as its first destination | Commands and events ask different things of a relay, so a generic table would carry columns and states that only one kind uses. It would also start phase 5 before phase 4's invariant tests are done |

## Decision

- **Phase 5 may reuse** the shape of the polling relay, one step that sends what is due
  at time *t* (ADR-0029); full-jitter backoff and the token bucket (ADR-0032); and the
  idempotent consumer, as the pattern a merchant's endpoint will need.
- **Phase 5 must not copy** what belongs to commands sent to a system with a state of its
  own: the acknowledgement, the lookup before a resend, and one decision per request.
  Events go to every subscriber, and nothing comes back.
- **Phase 5 decides, and weighs there:**
  - a separate outbox table, written in the same transaction as a deliberate, versioned
    contract, or a relay that reads the service's history directly, which is event
    sourcing's answer;
  - how the relay finds what is new. Rows commit in another order than they are
    inserted, so "everything after the last row sent" can skip a row that commits late,
    which is why `recorded_at` is never a cursor (ADR-0012). Records of what was sent, or
    PostgreSQL's logical decoding, are the usual answers;
  - how events evolve. In a study of 19 event-sourced systems, "event system evolution"
    comes first among five challenges, met by tactics such as "versioned events" and
    "upcasting".

## Consequences

- Phase 4 stays inside its phase, and phase 5 starts from named pieces rather than a
  half-built framework.
- The question of event sourcing is recorded where it will be decided, with what the
  service already derives from append-only facts.

## References

- Chris Richardson, [Pattern: Transactional outbox](https://microservices.io/patterns/data/transactional-outbox.html), [Pattern: Idempotent Consumer](https://microservices.io/patterns/communication-style/idempotent-consumer.html) and [Pattern: Event sourcing](https://microservices.io/patterns/data/event-sourcing.html) (microservices.io).
- Chris Richardson, [Microservices Patterns](https://www.manning.com/books/microservices-patterns) (Manning, 2018), chapter 3.
- Martin Fowler, [Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html) (2005).
- Michiel Overeem, Marten Spoor, Slinger Jansen and Sjaak Brinkkemper, [An empirical characterization of event sourced systems and their schema evolution — Lessons from industry](https://doi.org/10.1016/j.jss.2021.110970) (Journal of Systems and Software, 2021).
