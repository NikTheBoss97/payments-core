<!-- Author: Nicola Antonello -->

# ADR-0052: How the relay finds what to send

- Status: accepted
- Date: 2026-11-15

## Context

An event's ID is fixed before its transaction commits, and transactions commit in an
order of their own. A relay that remembers the last row it sent and asks for everything
after it can skip, for good, a row whose transaction commits late. So nothing pages by ID
(ADR-0005) or by `recorded_at` (ADR-0012).

PostgreSQL offers a safe cut-off. A snapshot's `xmin` is the "Lowest transaction ID that
was still active", and "All transaction IDs less than `xmin` are either committed and
visible, or rolled back and dead". A transaction gets its ID when it first writes, and
`pg_current_xact_id()` returns it as `xid8`, which "does not wrap around during the life
of an installation"; a transaction that only reads has none to hold the cut-off back.
Dudycz builds an outbox reader on this: each row stores its transaction's ID, the reader
takes only rows below the snapshot's `xmin`, and "Our queries may be a bit delayed if our
transactions last long". ADR-0005 named the same cut-off for this relay.

How far the delay reaches was measured on PostgreSQL 18.6 on 2026-11-15. While a
transaction holding ID 7100149 stayed open in the database `payments`, a reader in the
database `postgres` saw an `xmin` of 7100149, and 7100150 once it had committed. The
cut-off is the server's, not the database's: any write transaction left open, in any
database, holds it back.

Phase 4's sender learns of its work twice: a handover after the commit, and a search at
each step for requests with neither an answer nor a decision, over every request ever
recorded (ADR-0042).

## Options

| Option | Why not |
|---|---|
| **A handover, and a search at each step above a low-water mark that the cut-off moves** | Chosen |
| A handover, and a search at each step over the whole history (ADR-0042's shape) | Each step would read every event ever written, and nothing is ever deleted |
| A cursor below the snapshot's `xmin` (Dudycz) | Every delivery would wait for every older write transaction left open anywhere on the server. The tests, about 800 of them in their own databases on one server, would wait for each other |
| Logical decoding, Richardson's transaction log tailing | "Each slot streams a sequence of changes from a single database", and a slot holds back WAL and catalog rows while its consumer is away: "In extreme cases this could cause the database to shut down to prevent transaction ID wraparound". Every test database would need a slot of its own |

## Decision

- **Each event records its transaction.** A column holds `pg_current_xact_id()` as its
  default, stamped by the database as `recorded_at` is (ADR-0012). No role may insert into
  it, so no code can write a wrong value. An event's deliveries must be written in the
  same transaction (ADR-0053), and the database holds them to it: each delivery records
  its own transaction's ID the same way, under a composite foreign key to its event's ID
  and transaction ID, so a delivery written in any later transaction is refused.
- **The handover.** After the catch-up's commit, the server tries once to pass the new
  deliveries to the relay, through a bounded channel of 1,024, as ADR-0043 passes requests
  to the sender. A full or closed channel drops them, and the search finds them.
- **The search,** at each step, reads the deliveries that are not finished and whose
  event's transaction ID is at or above the mark, oldest first. A delivery is finished
  once a success is recorded for it or its endpoint is disabled (ADR-0053). One that the
  relay does not know may have been attempted before a restart, so it is due after a
  jittered delay.
- **The mark** lives in memory and starts at zero. Before each search the relay reads the
  snapshot's `xmin` in a statement of its own, so every transaction below it has ended
  before the search's snapshot is taken. After the search, the mark rises to the lower of
  that `xmin` and the smallest transaction ID among the unfinished deliveries found. Below
  the mark, every transaction has ended, so every event that will ever exist there is
  visible already, and every delivery there is finished: nothing below it can need
  sending.
- **A long transaction** only stops the mark from rising, so the search reads a little
  more. No delivery waits for it.
- **A restart** starts the mark at zero again: one full search, then bounded again, as
  ADR-0042 keeps the sweep's place in memory.

## Consequences

- A delivery leaves at once through the handover, or at the next step through the search.
- Each step reads from the oldest unfinished delivery on, which ADR-0053 bounds by three
  days, not the whole history.
- The relay holds no position in the database. What lasts is which deliveries succeeded
  and which endpoints are disabled.
- Tests: an event whose transaction stays open while later ones are delivered and the mark
  moves, delivered once it commits; the mark held back by a transaction in another
  database while deliveries go on; a dropped handover found by the search; a restart's
  full search; the mark's rule as a pure function under random inputs; a delivery written
  in a later transaction than its event's, as `payments_app`, refused by its foreign key.

## Amendments

- 2026-11-17: "One that the relay does not know ... is due after a jittered delay" reads
  as a retry: due after ADR-0053's delay for a first retry, and costing a token, as
  ADR-0042 treats a request that was not handed over. After a restart every unfinished
  delivery is one, so a backlog drains at the bucket's pace and never reaches a dead
  endpoint outside it. A delivery handed over while a search runs is kept when that search
  misses it, and one that a search no longer returns is finished and forgotten. The mark
  never falls: it becomes the higher of itself and the lower of that `xmin` and the
  smallest transaction ID among the unfinished deliveries found.

## References

- PostgreSQL 18, [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html): transaction ID and snapshot information functions.
- PostgreSQL 18, [Logical Decoding Concepts](https://www.postgresql.org/docs/18/logicaldecoding-explanation.html).
- Oskar Dudycz, [How Postgres sequences issues can impact your messaging guarantees](https://event-driven.io/en/ordering_in_postgres_outbox/) (event-driven.io, 2022).
- Chris Richardson, [Pattern: Transactional outbox](https://microservices.io/patterns/data/transactional-outbox.html) (microservices.io): the polling publisher and transaction log tailing.
