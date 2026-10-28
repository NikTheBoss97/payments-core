<!-- Author: Nicola Antonello -->

# ADR-0014: What a failed COMMIT proves

- Status: accepted
- Date: 2026-10-20
- Supersedes: the last classification bullet of ADR-0013, and ADR-0004's
  sentence "Any error PostgreSQL returns for `COMMIT` means the database
  transaction rolled back"

## Context

ADR-0004 and ADR-0013 read an error returned for `COMMIT` as proof that the
database transaction rolled back, and only a missing reply as an unknown
outcome. The rule decides whether a retry is safe: a posting wrongly read as
rolled back may be rebuilt with new IDs and booked twice.

The rule holds for errors raised before the commit is recorded.
`CommitTransaction()` fires the deferred triggers, among them the ledger's
commit-time check (`LG002`, `LG003`), before it writes the commit record.

It fails for signals. `CommitTransaction()` holds cancel and terminate
interrupts while it commits, so a signal that arrives then waits for the next
interrupt check. When the server logs statement durations
(`log_min_duration_statement`, `log_duration` or their sampling variants),
that check comes after the `COMMIT` has been confirmed with `CommandComplete`
and before `ReadyForQuery`: writing the duration line ends in
`CHECK_FOR_INTERRUPTS()`. The client then receives `ERROR 57014` after
`pg_cancel_backend`, or `FATAL 57P01` after `pg_terminate_backend`, for a
transaction that committed, and sqlx returns it as the error of `commit()`.
This was reproduced on PostgreSQL 18.6, with `commit_delay` to widen the
window and a signal sent 20 ms into the `COMMIT`: the client received the
error, and another session found the posting stored. With duration logging
off, a cancel during `COMMIT` has no effect and a terminate surfaces only on
the next command. Those logging settings are superuser-only and may be turned
on to log slow statements, so the application cannot assume them off.

Two facts about sqlx 0.9 (rev `f1e94ec`) bear on the same question:

- sqlx never sends PostgreSQL's cancel request. A `commit()` future dropped
  by a timeout, a `select!` or an aborted task leaves the server committing,
  and the reply is drained unread when the connection is pinged on its way
  back to the pool.
- `commit()` ignores the reply's command tag. After a statement in the
  transaction has failed, PostgreSQL answers `COMMIT` with `ROLLBACK`, and
  `commit()` still returns `Ok`.

## Options

| Option | Why not |
|---|---|
| Keep "a SQLSTATE returned for `COMMIT` means rolled back" | Wrong for a held cancel or terminate, which reports `57014` or `57P01` after the commit |
| A list of further codes that prove a rollback, such as `40001` | Each code needs its own argument about when it can be raised, for little gain: an unknown outcome costs one identical retry |
| **Only the ledger's deferred-check codes prove a rollback** | Chosen |
| Ask PostgreSQL afterwards with `pg_xact_status(xid8)`, which the manual offers for a connection lost during `COMMIT` | Needs the transaction ID fetched before every `COMMIT`, a round trip per posting, and still answers "in progress" while a first attempt sits idle in its transaction. The identical retry (ADR-0005) checks and applies in one step |

## Decision

- An error returned for `COMMIT` proves a rollback only when its SQLSTATE is
  one that the ledger's deferred checks raise, today `LG002` and `LG003`
  (ADR-0013). Interrupt handling never raises an `LG` code. Any other
  `COMMIT` error, with or without a SQLSTATE and at any severity, leaves the
  outcome unknown. It is settled as ADR-0005 describes, by sending the
  identical insert again with the same IDs, never by rebuilding the posting
  with new IDs.
- Every `COMMIT` the application sends goes through one function that
  applies this rule, so phase 2's units of work (ADR-0011) inherit it.
- No client-side timeout, `select!` or task abort wraps a posting or its
  `commit()`. Waits are bounded on the server instead (ADR-0005).
- After any failed statement, the code drops the transaction instead of
  committing it, because `commit()` would return `Ok` for the rollback.

## Consequences

- A few rolled-back transactions are reported as unknown, for example one
  whose session was killed before its commit record. Each costs one
  identical retry, which then succeeds.
- Two tests cover the rule. A deferred trigger that terminates its own
  session makes the classification deterministic. A second test turns on
  duration logging and `commit_delay` in its superuser session, cancels the
  backend during `COMMIT`, and expects an unknown outcome with the posting
  stored.
- The failure model (DESIGN.md) gains a `COMMIT` answered with an error after
  it committed.
- Nothing bounds how long a client takes to notice a lost `COMMIT`: sqlx sets
  no TCP keepalive or read timeout on its sockets. That is left to phase 8.

## Amendments

- 2026-10-28: "the ledger's deferred checks" means every deferred check in
  the registry of ADR-0013, now `LG002` to `LG006`, the last three added
  for payments (ADR-0017). Each is raised by a constraint trigger that
  `CommitTransaction()` fires before it writes the commit record, so each
  proves a rollback, and the one function through which every `COMMIT` goes
  lists all five.

## References

- PostgreSQL 18 manual, [Message Flow](https://www.postgresql.org/docs/18/protocol-flow.html) (`CommandComplete`, `ReadyForQuery`, canceling requests in progress).
- PostgreSQL 18 manual, [Server Signaling Functions](https://www.postgresql.org/docs/18/functions-admin.html#FUNCTIONS-ADMIN-SIGNAL).
- PostgreSQL 18 manual, [Error Reporting and Logging](https://www.postgresql.org/docs/18/runtime-config-logging.html) (`log_min_duration_statement`).
- PostgreSQL 18 manual, [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html) (`pg_xact_status`).
- PostgreSQL 18.6 source: `CommitTransaction()` in [xact.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/transam/xact.c), `exec_simple_query()` in [postgres.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tcop/postgres.c), `errfinish()` in [elog.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/error/elog.c).
- sqlx `f1e94ec`: [sqlx-postgres/src/transaction.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-postgres/src/transaction.rs) (`commit()`), [sqlx-core/src/pool/connection.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/pool/connection.rs) (the ping on release).
