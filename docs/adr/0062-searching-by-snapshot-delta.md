<!-- Author: Nicola Antonello -->

# ADR-0062: Searching by snapshot delta

- Status: accepted
- Date: 2026-11-18
- Supersedes: in ADR-0052's Decision, "The search", "The mark", "A long transaction" and "A
  restart", and its amendment of 2026-11-17

## Context

ADR-0052's search reads, at each step, the deliveries not finished from a mark up, and the
mark cannot pass the oldest unfinished delivery, nor the server's `xmin`. A search walks the
index from the mark, so it visits every delivery written since, finished or not. One endpoint
that keeps failing holds the mark for up to ADR-0053's three days, and one write transaction
left open anywhere on the server holds it for as long as it stays open. At a steady 10
deliveries a second, three days are about 2.6 million rows, which every search visits, each
second. ADR-0052 accepted the cost ("Each step reads from the oldest unfinished delivery on"),
which grows with how long anything stays stuck, not with the work.

A snapshot names the transactions that may still change after it is taken. PostgreSQL's
`pg_current_snapshot()` gives its `xmax`, "One past the highest completed transaction ID", at
or above which no transaction had completed when the snapshot was taken, and its list of the
transactions then in progress. A transaction between its `xmin` and its `xmax` that is not in
the list "was already completed at the time of the snapshot". So every search that begins
after a snapshot sees all the rows of every transaction that the snapshot counts as
completed.

## Options

| Option | Why not |
|---|---|
| ADR-0052 as accepted, measured in phase 8 | Each search grows with how long an endpoint stays failing or a transaction stays open |
| The same search, every few seconds | The cost falls by that factor and still grows the same way, and a dropped handover waits that long |
| A cursor from the previous search's `xmin` | A transaction left open anywhere holds `xmin` back, and every search reads everything since it began |
| **Each search reads the transactions not completed at the previous search's snapshot** | Chosen |

## Decision

- **Each search** first reads the snapshot's `xmax` and its list of transactions in
  progress, in a statement of its own. A later statement reads the deliveries not finished,
  with no success and an endpoint not disabled, whose transaction had not completed at the
  previous search's snapshot: one in its list, or one at or above its `xmax`. The relay keeps
  the new snapshot for the next search.
- **The first search** after a start has no snapshot before it, so it reads every delivery
  not finished. One that it finds and that the relay does not know is a retry, due after a
  first retry's delay and costing a token: it may have been attempted before the restart, and
  a backlog drains at the bucket's pace, as ADR-0042 treats a request that was not handed
  over.
- **A later search** finds only deliveries whose transaction completed after the relay
  started, so one that it does not know was never attempted: its handover was dropped. It is
  a first attempt, due at once and free, as a handover's is.
- **The relay forgets a delivery** only when it is finished: by its success, by its
  endpoint's disablement that the relay records, or by an attempt that finds its endpoint
  disabled. Each attempt reads its endpoint's disablement with its URL, so the operator's
  disablement ends the endpoint's deliveries at their next attempt, and nothing is recorded.
- **No mark.** The relay reads neither `xmin` nor the transaction IDs of the deliveries it
  finds.

## Consequences

- A search reads the transactions that completed since the previous one, and probes one
  index entry for each transaction still in progress. A transaction left open, or an
  endpoint failing, makes it read nothing more.
- A delivery leaves at once through the handover, or at the next step through a search when
  its handover was dropped.
- The relay holds every delivery not finished that it knows until it finishes it, as it did,
  and ADR-0053's three days bound them. A delivery of an endpoint that the operator disabled
  stays in memory until its next attempt.
- After a restart, the first search reads every delivery ever written, as ADR-0052's did.
- What lasts is still which deliveries succeeded and which endpoints are disabled. The index
  on `webhook_deliveries (xact_id)` serves a range and a list of IDs.
- Tests: the relay's rule under random histories of PostgreSQL's transactions, whose
  snapshots have an `xmax` below transactions still open; a delivery that a later search
  finds, attempted at once and free, and one that the first search finds, as a retry; an
  attempt that finds its endpoint disabled; ADR-0052's two tests of transactions left open,
  with this record's search.

## References

- PostgreSQL 18 manual, [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html): `pg_current_snapshot()` and the components of a snapshot.
- Oskar Dudycz, [How Postgres sequences issues can impact your messaging guarantees](https://event-driven.io/en/ordering_in_postgres_outbox/) (event-driven.io, 2022).
