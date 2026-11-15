<!-- Author: Nicola Antonello -->

# ADR-0054: What phase 5 promises, and how it is tested

- Status: accepted
- Date: 2026-11-15

## Context

Each invariant of this service has a stated enforcement and a test that tries to break
it, and the guards stand twice, in Rust and in PostgreSQL, so that a test can break each
one alone (DESIGN.md). Phase 5 owes I6 its outbound event, and ADR-0049 to ADR-0053 make
two more promises possible: an event goes only to its own merchant, and it reaches each of
its endpoints at least once.

No sender can promise exactly once over HTTP. A lost answer looks the same whether or not
the delivery arrived, so only the receiver can drop the copy: Saltzer, Reed and Clark's
end-to-end argument. Standard Webhooks gives the receiver the means, an ID that "remains
the same no matter how many times a webhook that has failed is retried". ADR-0035 calls
that receiver the idempotent consumer, "the pattern a merchant's endpoint will need".

Phase 4's fake PSP set the shape of a fake: a library that a test drives through a
handle, over real HTTP, with a thin binary for local runs, sharing no code with the
server (ADR-0029).

## Options

The promise of delivery:

| Option | Why not |
|---|---|
| **At least once, to each endpoint enabled when the event was written, unless it is disabled first** | Chosen |
| Exactly once | No sender can know whether a lost answer followed a delivery. A merchant gets the effect of once by dropping copies by `webhook-id` |

The fake merchant:

| Option | Why not |
|---|---|
| **A crate of its own, `mock-merchant`, built like `mock-psp`** | Chosen |
| A module of `mock-psp` | One crate would fake two parties, and its name would mislead |
| A helper under `tests/` | It could reach the server's signing code and repeat its mistakes, which ADR-0029 keeps the fakes from doing |

## Decision

- **I6, completed:** "A payment state change, its ledger entries, and its outbound event
  commit atomically." The event and its deliveries join the step, the posting, the record
  and the fact in the catch-up's transaction (ADR-0049, ADR-0053). `LG008` refuses a fact
  without its event, and a unique key allows one event per fact.
- **I7, extended:** "A merchant can read or change only its own payments, and is told only
  about its own." In Rust, the fan-out selects the endpoints of the event's merchant. In
  PostgreSQL, composite foreign keys tie an event's payment and merchant to the payment's,
  and a delivery's event and endpoint each to the same merchant, so for any SQL that the
  runtime role runs, a delivery can only target an endpoint of its event's own merchant.
  The merchant's role has no privilege on endpoints, events or deliveries.
- **I10, new:** "Every event reaches every endpoint enabled when it was written, at least
  once, unless that endpoint is disabled first." In Rust, its guards are the deliveries
  written with their event (ADR-0053), the search above the mark (ADR-0052), and retries
  until a `2xx` or a disablement (ADR-0053). In PostgreSQL, each delivery's composite
  foreign key to its event's ID and transaction ID refuses a delivery written outside its
  event's transaction, below a mark that may have passed it (ADR-0052). That every enabled
  endpoint got its delivery has no guard in PostgreSQL: a check at `COMMIT` would read the
  endpoints again, and race with one registered in between, so the property test covers
  it.
- **The tests that try to break them:**
  - the backend killed before and after each new insert of the catch-up, and during its
    `COMMIT`;
  - `LG008` rolling back a fact written without its event, as the runtime role, and
    catalog tests that pin the new tables, keys, privileges and trigger, and the column of
    the transaction ID with its default and no privilege to insert into it;
  - each guard of I7 broken alone: a delivery to another merchant's endpoint written in raw
    SQL, refused by its foreign key, and the fan-out's filter on the merchant removed,
    caught by the property test;
  - an event whose transaction stays open while the mark moves past later ones, delivered
    once it commits, and a delivery written in a later transaction than its event's,
    refused by its foreign key (ADR-0052);
  - the relay stopped between a `2xx` and its record: the event arrives again, and never
    goes missing;
  - a property test that runs random interleavings of catch-ups, relay steps, crashes,
    answers from the endpoints (`2xx`, `5xx`, `410`, timeouts), registrations and
    disablements against a model: every delivery ends delivered or with its endpoint
    disabled, nothing reaches an endpoint of another merchant, and nothing is sent that no
    event caused;
  - the binaries end to end, the server, the fake PSP and the fake merchant, as in the
    quickstart.
- **The fake merchant.** `crates/mock-merchant` is a library with a thin binary, listed
  only under `payments-core`'s `[dev-dependencies]` (ADR-0029). A test starts one on a
  random local port and scripts each answer through its handle: a `2xx`, an error, a
  `410`, a delay past the timeout, a `Retry-After`. It verifies each signature with its
  own code, which the spec's test vector checks, and drops copies by `webhook-id`, as a
  merchant should. The handle returns what arrived. The binary answers everything with a
  `2xx` and logs it, for the quickstart.
- **Pages.** `docs/correctness/outbox.md` names the tests of each invariant when they
  land, and the README gains its rows.

## Consequences

- Phase 5 has three invariants to finish: I6's outbound event, I7's extension and I10.
  The README and `docs/DESIGN.md` mark them in progress until their tests land.
- What these tests do not show: TLS to real merchants, real networks, and clocks that
  disagree by more than a receiver's window.
- `payments-core` gains a second fake under `[dev-dependencies]`, and the workspace a third
  crate.

## References

- J. H. Saltzer, D. P. Reed and D. D. Clark, [End-to-End Arguments in System Design](https://doi.org/10.1145/357401.357402) (ACM Transactions on Computer Systems, 1984).
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): the webhook headers and the published test vector.
- Chris Richardson, [Pattern: Idempotent Consumer](https://microservices.io/patterns/communication-style/idempotent-consumer.html) (microservices.io).
