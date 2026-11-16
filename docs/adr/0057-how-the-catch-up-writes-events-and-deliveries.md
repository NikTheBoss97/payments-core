<!-- Author: Nicola Antonello -->

# ADR-0057: How the catch-up writes events and deliveries

- Status: accepted
- Date: 2026-11-16

## Context

ADR-0049 has the catch-up write one event for each fact that it applies, in the fact's
transaction, "inside the commands that take `PspWord`". ADR-0053 has the same transaction
write one delivery for each endpoint of the merchant "that is enabled when its statement
runs", each with a `DeliveryId` made in Rust. ADR-0055 made the tables, and left `LG008`
until the catch-up and the test fixtures write events.

The catch-up applies each new fact in one place: it makes the fact's step through the
command of its kind, which takes `PspWord` (ADR-0048), then writes the fact, the link from
the step to the fact, and the decision of the request that the fact answers. Three kinds
of fact make no step: a refused capture, void or refund, which closes its request
(ADR-0037), and a settlement after a full refund (ADR-0016). No command writes those
facts, so no command could write their events.

IDs are UUIDv7s made in Rust by one generator, and the database has no default for them
(ADR-0005). A fan-out writes as many deliveries as the merchant has enabled endpoints, a
number that only the database knows. PostgreSQL's `unnest` with several arrays "Expands
multiple arrays (possibly of different data types) into a set of rows", in a query's
`FROM` clause, so one `INSERT` can write rows whose IDs Rust made. sqlx binds a `&[Uuid]`
as `uuid[]`: its `PgHasArrayType` for `Uuid` names that type, read in its source at the
pinned commit. PostgreSQL 18 can also make the IDs itself: `uuidv7()` "Generates a version
7 (time-ordered) UUID".

Facts are also written with raw SQL by the test fixtures and the schema tests. A first
draft of `LG007` failed 36 tests that committed a step without its fact, until the
fixtures wrote each step's fact (ADR-0048); `LG008` meets every fact that those write.

## Options

Where the event is written:

| Option | Why not |
|---|---|
| **Beside its fact, by the catch-up, for every fact that it writes** | Chosen |
| In the commands that take `PspWord`, as ADR-0049 words it | The facts that make no step pass through no command, so their events would need a second place, and a command does not know its fact's position |

How the deliveries get their IDs:

| Option | Why not |
|---|---|
| **Read the enabled endpoints, then write an event's deliveries in one `INSERT` from two arrays, with IDs made in Rust** | Chosen |
| One `INSERT` per endpoint | A round trip for each endpoint, while the payment's lock is held |
| One `INSERT ... SELECT` that reads the endpoints and calls `uuidv7()` | One statement, but a second generator of IDs, on the database server's clock, beside the one of ADR-0005 |

## Decision

- **One event per fact written.** After the rows of each fact that it applies (the fact,
  its step's link and its request's decision), the catch-up writes the fact's event,
  whatever the fact made: a step, a refusal, or a settlement recorded without a step. The
  event holds an `EventId`, the payment's merchant, read under the lock with the payment's
  stop, and the fact's position. A fact that stops the payment is not written, so it has
  no event; creating and cancelling write none (ADR-0049). The code that writes it is
  private to the catch-up's module, as `PspWord`'s field is.
- **The endpoints are read once per catch-up,** under the payment's lock, after the checks
  that find a new fact and before the first one is applied: the merchant's endpoints that
  have no disablement. Every event of that catch-up goes to them.
- **Each event's deliveries are one statement,**
  `INSERT ... SELECT ... FROM unnest($1::uuid[], $2::uuid[])`: a new `DeliveryId` for each
  endpoint, beside the endpoints, with the event's ID and merchant bound once. Both arrays
  come from one list, so they always have one length, and `unnest` never pads one with
  `NULL`. A merchant without endpoints gets its events and no delivery.
- **"Enabled when its statement runs" is enabled when the read runs.** An endpoint
  registered after the read gets none of the catch-up's events, as one registered after
  its commit gets none (ADR-0053). One disabled after the read gets its deliveries, which
  its disablement finishes. The read locks no endpoint, so registering or disabling one
  never waits for a payment.
- **`EventId` and `DeliveryId`** join the ID types, from the one generator (ADR-0005).
  Both are `Copy`, as `PaymentId` is: a reused one fails on its primary key, and no code
  reads a repeated one as the same command, which is why `TransitionId` is not (ADR-0019).
  `EndpointId` joins them with the first code that takes one.
- **`LG008` comes last,** once the catch-up and the fixtures write each fact's event: a
  constraint trigger, `payment_facts_event_check`, `DEFERRABLE INITIALLY DEFERRED` and
  `ENABLE ALWAYS`, with a function that pins its search path (ADR-0004), as
  `payment_transitions_fact_check` checks a step's fact (ADR-0048). It joins the codes
  that prove a rollback (ADR-0014).
- **No handover yet.** The catch-up hands its deliveries to the relay after the commit
  once the relay's task exists (ADR-0052). Until then they wait in the table, where the
  relay's search finds them anyway.

## Consequences

- I6's outbound event commits with its fact, and `LG008` refuses a fact without it.
- I7's guard in Rust is the read's filter on the payment's merchant, and I10's fan-out is
  fixed by the deliveries written with each event (ADR-0054).
- A catch-up that applies facts runs one more statement, the read, and two more per fact.
- Nothing in PostgreSQL proves that the read found every enabled endpoint (ADR-0054).
- Tests: one event per applied fact, of every kind; none for a stopped fact, a creation or
  a cancellation; a delivery to each enabled endpoint of the merchant, and none to a
  disabled one, to another merchant's, without endpoints, or to an endpoint registered
  after the event; `LG008` refusing at `COMMIT` a fact written without its event,
  classified as a rollback; the catalog pinning the trigger and its function.

## References

- PostgreSQL 18 manual, [Array Functions and Operators](https://www.postgresql.org/docs/18/functions-array.html): `unnest` with several arrays.
- PostgreSQL 18 manual, [UUID Functions](https://www.postgresql.org/docs/18/functions-uuid.html): `uuidv7()`.
