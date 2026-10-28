<!-- Author: Nicola Antonello -->

# ADR-0018: Serializing a payment's commands

- Status: accepted
- Date: 2026-10-28
- Supersedes: in ADR-0002's Decision, the words "which coordinates by
  locking the payment row"

## Context

The history's keys keep I5 and the refund limit safe (ADR-0017), but on
their own they answer the second of two concurrent commands with a
duplicate-key error. A capture racing a void, two refunds that each fit
alone, and a retry racing its first attempt are expected events, not bugs:
each should get a domain answer from Rust. Phase 6 will also change many
payments in one database transaction, which should wait its turn rather
than abort.

ADR-0002 assumed that the payment layer would lock the payment row. Every
row-locking clause of `SELECT` needs the `UPDATE` privilege, which the
runtime role does not have (ADR-0017), and granting it would let any
runtime session lock a payment `FOR UPDATE` and so block every insert that
references it. ADR-0002's consequences already list the alternative: a
transaction-scoped advisory lock, which locks no row and needs no table
privilege.

The decision relies on these PostgreSQL facts, checked on 18.6 as the
runtime role:

- Every role may execute the advisory-lock functions. A transaction-level
  lock is released when its transaction ends; a session-level lock is held
  until it is unlocked or its session ends.
- `lock_timeout` ends a wait for an advisory lock with `55P03`, and the
  deadlock detector covers advisory locks (`40P01`).
- At READ COMMITTED each statement takes a new snapshot, so a read in a
  later statement than the lock sees what the previous holder committed. A
  read in the same statement, or any read at REPEATABLE READ, uses a
  snapshot taken before the wait.
- `idle_in_transaction_session_timeout` ends a session that is idle inside
  a transaction. Only `idle_session_timeout` ends one that is idle outside a
  transaction, and it is off by default.

## Options

| Option | Why not |
|---|---|
| **A transaction-level advisory lock on the payment, taken first, with reads in later statements at READ COMMITTED** | Chosen |
| `FOR NO KEY UPDATE` on the payment row, through an `UPDATE (status)` grant | Needs the mutable status column that ADR-0017 rejects, and a privilege that lets any runtime session lock payment rows `FOR UPDATE` |
| No lock: the history's primary key as the only point of order | A duplicate key becomes an expected outcome with a retry loop, a batch aborts on one collision, and in the race test of the design that proposed it, 7 of 10 refunds that still fitted were answered "contended" |
| `SERIALIZABLE` | In the lab, refunds of two unrelated payments failed with `40001`. It protects only against writers that are themselves `SERIALIZABLE`: a READ COMMITTED refund and a SERIALIZABLE one together refunded more than the capture. And it adds a second retry path |
| A session-level advisory lock (`pg_advisory_lock`) | Outlives its transaction: a session that holds it and then sits idle outside a transaction blocks the payment, and no default timeout ends that session |
| Revoking `EXECUTE` on the session-level lock functions from `PUBLIC` | The functions belong to the bootstrap superuser, so only a superuser can change their privileges, and each database keeps its own copy, the test databases included. The rule and the timeout below bound the same failure without touching built-in functions |

## Decision

- **One lock per payment, taken first.** Every command on a payment runs in
  one database transaction whose first statement is
  `SELECT pg_advisory_xact_lock($1)`, with `$1` the low 64 bits of the
  payment's ID, read in Rust as a big-endian `i64`. RFC 9562's variant bits
  are the top bits of that half, so every payment's key is negative; other
  uses of advisory locks take non-negative keys, as the regression test of
  ADR-0006 does with 1.
- **Read after the lock, at READ COMMITTED.** The command reads the payment
  only in statements after the lock, so it sees every step that the previous
  holder committed. Runtime connections use READ COMMITTED, the server's
  default: no code sets another level, and a test pins the level that a
  runtime connection gets. At REPEATABLE READ a queued command would read a
  stale state, and its insert would fail on the history's primary key: an
  error, never a wrong write.
- **Transaction-level locks only.** Application SQL calls
  `pg_advisory_xact_lock` and no other advisory-lock function, and a test
  fails if `pg_advisory_lock` or `pg_try_advisory_lock` appears in `src/`.
  In case hand-written SQL leaks a session-level lock anyway, `roles.sql`
  sets `idle_session_timeout = '30min'` on the runtime role. sqlx's pool
  closes a connection once it has been idle for `idle_timeout`, 10 minutes
  by default, and checks every `min(idle_timeout, max_lifetime)`, also 10
  minutes, so it keeps an idle connection for at most about 20 minutes. The
  server's limit sits above that and ends only sessions that the pool no
  longer manages.
- **Several payments in one transaction.** A unit of work that changes
  several payments, such as a phase-6 settlement batch, locks all of them
  before anything else, one statement per key, in ascending key order, so
  that two such units cannot deadlock. Locking the same key twice is
  harmless. Each lock takes a slot in a table that all sessions share,
  sized for `max_locks_per_transaction` (64) locks per server process, so a
  batch's size is bounded, and "out of shared memory" (`53200`) is a bug,
  not a reason to retry.
- **Waits are bounded on the server.** The runtime role's `lock_timeout` of
  35 seconds bounds the wait for the lock, and a timeout (`55P03`) is an
  unknown outcome. Its `idle_in_transaction_session_timeout` of 30 seconds
  ends a first attempt that stalled inside its transaction (ADR-0005). No
  client-side timeout wraps a command (ADR-0014).
- **The history's keys stay the backstop.** The lock puts a payment's
  commands in order; it is not what keeps the data correct. A writer that
  skips it is still refused by the history's constraints (ADR-0017).

## Consequences

- A command that loses a race waits, then reads the winner's step and gets
  a domain answer, such as "already applied", an illegal transition or a
  refund above the limit, with no database error. In the lab, a refund that
  waited 2 seconds behind another one then saw it.
- A batch waits behind the payments it touches and then commits. While it
  runs, their commands wait in turn, so phase 8 measures batch sizes.
- Liveness depends on a convention, and safety does not. A writer that skips
  the lock and commits first makes a locked command fail on the primary
  key, which is reported as a bug (ADR-0019), never written wrongly.
- Two payments whose IDs share their low 64 bits wait for each other:
  latency, never a wrong result.
- An active session that stalls while holding a payment's lock delays each
  command on that payment by up to 35 seconds, then answers "not
  confirmed". A leaked session-level lock blocks the payment for up to 30
  minutes.
- ADR-0002's rule for guarding a limit (lock first, then read in a later
  statement, at READ COMMITTED) now governs every payment command.

## References

- PostgreSQL 18 manual, [Advisory Locks](https://www.postgresql.org/docs/18/explicit-locking.html#ADVISORY-LOCKS) and [Advisory Lock Functions](https://www.postgresql.org/docs/18/functions-admin.html#FUNCTIONS-ADVISORY-LOCKS).
- PostgreSQL 18 manual, [Read Committed Isolation Level](https://www.postgresql.org/docs/18/transaction-iso.html#XACT-READ-COMMITTED).
- PostgreSQL 18 manual, [Client Connection Defaults](https://www.postgresql.org/docs/18/runtime-config-client.html) (`lock_timeout`, `idle_in_transaction_session_timeout`, `idle_session_timeout`).
- PostgreSQL 18 manual, [Lock Management](https://www.postgresql.org/docs/18/runtime-config-locks.html) (`max_locks_per_transaction`).
- sqlx `f1e94ec`: [sqlx-core/src/pool/options.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/pool/options.rs) (pool defaults) and [sqlx-core/src/pool/inner.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/pool/inner.rs) (`spawn_maintenance_tasks`).
- K. P. Eswaran, J. N. Gray, R. A. Lorie and I. L. Traiger, [The Notions of Consistency and Predicate Locks in a Database System](https://doi.org/10.1145/360363.360369) (Communications of the ACM, 1976).
- Chuzhe Tang et al., [Ad Hoc Transactions in Web Applications: The Good, the Bad, and the Ugly](https://doi.org/10.1145/3514221.3526120) (SIGMOD, 2022).
- Dan R. K. Ports and Kevin Grittner, [Serializable Snapshot Isolation in PostgreSQL](https://vldb.org/pvldb/vol5/p1850_danrkports_vldb2012.pdf) (PVLDB, 2012).
- Natacha Crooks, Youer Pu, Lorenzo Alvisi and Allen Clement, [Seeing is Believing: A Client-Centric Specification of Database Isolation](https://doi.org/10.1145/3087801.3087802) (PODC, 2017).
- Felix Geisendörfer, [Implementing State Machines in PostgreSQL](https://felixge.de/2017/07/27/implementing-state-machines-in-postgresql/) (2017), footnote on concurrency.
- K. Davis, B. Peabody and P. Leach, [RFC 9562: Universally Unique IDentifiers (UUIDs)](https://www.rfc-editor.org/rfc/rfc9562.html) (IETF, 2024), section 4.1 (the variant field).
