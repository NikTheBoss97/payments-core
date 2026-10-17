<!-- Author: Nicola Antonello -->

# ADR-0012: Recording times

- Status: accepted
- Date: 2026-10-17

## Context

A fact has two times. Its recording time (also called system or transaction
time) is when our database stored it: nobody chooses it, and audits need to
trust it. Its effective time (also called valid or business time) is when it
happened in the world: whoever knows it supplies it, it can be backdated, and
it may come from another party in another time zone. SQL:2011 standardised
the split, and there the database assigns system time; users cannot set it.

PostgreSQL has three clocks. `now()`, its clearer synonym
`transaction_timestamp()` and the SQL syntax `CURRENT_TIMESTAMP` return the
start of the current transaction, and stay the same until it ends.
`statement_timestamp()` returns the time the current command message
arrived. `clock_timestamp()` changes on every call. None of them is the
commit time: rows become visible at `COMMIT`, and a transaction that began
earlier can commit later.

The ledger tables are append-only (ADR-0004), so a time column added later
could give the rows already stored only the migration's time or a sentinel,
never their real recording time.

## Options

| Option | Why not |
|---|---|
| No time column yet | Rows written before the column exists never get a true recording time; a UUIDv7's embedded time is the service's clock, read before `BEGIN`, and reused by the retry after a lost `COMMIT` (ADR-0005) |
| **`recorded_at`, stamped by PostgreSQL and not writable by the runtime role** | Chosen |
| A time supplied by the service | Each instance has its own clock, and the failure model assumes no clock agreement between services (DESIGN.md) |
| `recorded_at` plus an `effective_at` supplied by the domain (bitemporal) | No invariant or matching rule in phases 2 to 6 needs it yet; it needs rules nobody can set yet (how far back, which time zone, a date or an instant); and a wrong value on an immutable row costs a reversing transaction |

## Decision

- `recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp()`, as the
  last column of `accounts` and `ledger_transactions`, and of later tables
  that record facts, such as payments, idempotency records and outbox events.
  Not on `ledger_entries`, which commit with their header, nor on
  `currencies`, which only migrations write.
- `timestamptz` without a precision: `timestamptz(0)` rounds, up to half a
  second into the future.
- The runtime role's `INSERT` privilege lists every column except
  `recorded_at`, so it cannot set the column to a chosen value: naming it,
  even as `DEFAULT`, fails with `42501`. Each new column is granted
  explicitly. A table-level `INSERT` grant would reopen `recorded_at`, and a
  column-level `REVOKE` cannot undo a table-level grant. `#[sqlx::test]` runs
  as a superuser, which skips privilege checks, so a catalog test pins the
  runtime role's exact privileges.
- What `recorded_at` means: the start of the database transaction that
  inserted the row. It is never later than the write, so it is a lower bound
  on the insert and commit times. It is neither unique nor monotonic, and it
  is not commit order. It is never an exact cut-off, cursor or watermark, and
  never used to choose among rows. Ordering for display, and approximate age
  checks with a margin far larger than the longest transaction, are fine.
- Business times are typed columns, supplied by the domain, on the records
  that own them. Effective time is deferred to phase 6, on the condition that
  phase 2 links every money-moving ledger transaction to the business record
  that holds its business time (ADR-0011).
- Rust reads nothing of `recorded_at` yet. The first query that returns it
  enables exactly one of sqlx's `time` or `chrono` features.

## Consequences

- One clock serves every service instance. The stamp stays true after a
  lost `COMMIT`, because the retry that succeeds is stamped when it ran, and
  every row of one unit of work shares the same instant.
- A session that keeps its transaction open stamps its rows early, by as
  long as it waited.
- ADR-0005's "no database defaults" covers identities, which the service
  must know before any I/O. A recording time is an observation the database
  makes, so it is not an exception to that rule.
- When a later phase needs commit order, it uses PostgreSQL's transaction
  IDs (ADR-0005), not timestamps.

## References

- Krishna Kulkarni and Jan-Eike Michels, [Temporal features in SQL:2011](https://sigmodrecord.org/publications/sigmodRecord/1209/pdfs/07.industry.kulkarni.pdf) (SIGMOD Record, 2012).
- Martin Fowler, [Bitemporal History](https://martinfowler.com/articles/bitemporal-history.html) (2021).
- PostgreSQL 18 manual, [Date/Time Functions and Operators](https://www.postgresql.org/docs/18/functions-datetime.html#FUNCTIONS-DATETIME-CURRENT) (current date and time).
- PostgreSQL 18 manual, [Privileges](https://www.postgresql.org/docs/18/ddl-priv.html) and [GRANT](https://www.postgresql.org/docs/18/sql-grant.html) (column privileges).
- PostgreSQL wiki, [Don't Do This](https://wiki.postgresql.org/wiki/Don%27t_Do_This) (`timestamp(0)` and `timestamptz(0)`).
- TigerBeetle, [Time](https://docs.tigerbeetle.com/coding/time/).
