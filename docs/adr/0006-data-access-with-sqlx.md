<!-- Author: Nicola Antonello -->

# ADR-0006: Data access with compile-time-checked SQL

- Status: accepted
- Date: 2026-10-14

## Context

sqlx is the project's PostgreSQL driver (see the stack in the README). Two
things remain open: how SQL is written and checked, and which sqlx version
to use. Ledger SQL must be exact and explainable (locks, isolation, inserts
of several rows at once), and tests must really commit so that the
commit-time check of ADR-0004 runs.

## Options

How SQL is written and checked:

| Option | Why not |
|---|---|
| **sqlx macros with a committed offline cache** | Chosen |
| sqlx macros against a live database only | Every build, including a fresh clone, needs a migrated database |
| sqlx runtime queries (`query`, `QueryBuilder`) | SQL errors only appear at run time; kept as an escape hatch |
| Diesel (with diesel-async) | Replaces sqlx, has no CTEs in its query language, and checks a Rust model of the schema rather than the database |
| SeaORM | Generates the SQL, hiding the locks we must be able to explain |
| sea-query | Builds SQL in Rust but checks nothing against the schema |
| tokio-postgres | Replaces sqlx, with no compile-time checks and no test harness |
| Cornucopia | Also checks against a real database, but replaces sqlx and adds tooling |
| PL/pgSQL functions as the API | Moves the write path itself into PL/pgSQL, whose SQL is checked only when it runs, not when the Rust compiles; the chosen design keeps only small invariant checks there (ADR-0004) |

Which sqlx version:

| Option | Why not |
|---|---|
| **The main-branch commit that merged the fix, then 0.9.1** | Chosen |
| sqlx 0.8.6 | Has the same cancelled-`begin()` bug |
| sqlx 0.9.0 with workarounds (each unit of work in a spawned task, no timeout around `begin()`) | Safe only while every call site follows the rule; one missed path can lose writes |
| A fork with the fix cherry-picked (Windmill's approach) | A fork to maintain, for code that main already has |

## Decision

- Static SQL goes through `query!`, `query_as!` and `query_scalar!`. Each
  macro asks PostgreSQL to prepare its statement without running it, and
  PostgreSQL reports the parameter and result types, so syntax errors,
  unknown columns and type mismatches fail the build. This happens when the
  offline cache is generated (`cargo sqlx prepare`), or in a build that has
  `DATABASE_URL` set and `SQLX_OFFLINE` unset; other builds check against
  the stored results. sqlx assumes that a column computed by an expression,
  such as `SUM(...)`, may be NULL, which is why such columns carry
  annotations (below).
- The offline cache (`.sqlx/`: one JSON file per query, holding the types
  PostgreSQL reported for it) is committed in the crate, in the same commit
  as the query or migration change that produced it, so every commit builds
  offline and can be bisected. `.env.example` and the CI workflow set
  `SQLX_OFFLINE=true`, so builds read the cache and need no database; with
  only `DATABASE_URL` set, the macros would query that database instead. A
  required CI job runs `cargo sqlx prepare --check -- --all-targets` against
  a migrated database: it regenerates the cache, including queries in tests
  (`--all-targets`), and fails if any entry is missing or differs from the
  committed one.
- Dynamic SQL, if it is ever needed, uses `QueryBuilder::push_bind`, which
  sends values as bind parameters and never splices them into the SQL text,
  in one module covered by integration tests.
- Expression columns carry sqlx's not-null override in their alias:
  `AS "balance!"` tells sqlx the column is never NULL, which the `COALESCE`
  makes true.
- Parameters are bound as primitive values, such as the `Uuid` inside an
  `AccountId`, never with the macro's `id as AccountId` override, because
  that override turns off the type check for the parameter.
- All entries of a ledger transaction are inserted in one statement
  (`INSERT ... SELECT ... FROM UNNEST(...)`). Each column is sent as one
  array and `UNNEST` turns the arrays into rows, so the SQL text is the same
  for any number of entries and the macro can check it. If the arrays differ
  in length, `UNNEST` pads with NULLs, which the `NOT NULL` columns
  (ADR-0003) reject.
- **Version.** sqlx and sqlx-cli are pinned to commit
  `f1e94ec34dd5f11ad6488a3a8ac7988439a3f757` of transact-rs/sqlx, the merge
  of [#4394](https://github.com/transact-rs/sqlx/pull/4394) on 2026-09-09,
  until 0.9.1 is released. A later main commit may replace it only after its
  diff from `v0.9.0` has been read, and its full hash is then recorded here.
  The pin goes in `[patch.crates-io]` for the sqlx crates, so no crates.io
  copy of sqlx 0.9.0 can enter the dependency graph.
  - Why: sqlx 0.9.0, the only 0.9 release, leaves a pooled connection inside
    an open transaction when `begin()` is cancelled partway, that is, when
    its future is dropped before it finishes, as a request timeout or
    `tokio::select!` can do ([#4393](https://github.com/transact-rs/sqlx/issues/4393)).
    The next request on that connection then runs inside the stale
    transaction: statements it runs outside its own transaction are not
    committed when they return, and are lost if the connection closes first,
    and after one error every query on that connection fails until the pool
    recycles it.
  - The pinned commit also contains the earlier fixes for `.env` files read
    from directories above the workspace
    ([#4282](https://github.com/transact-rs/sqlx/pull/4282)), which can switch
    `DATABASE_URL` or `SQLX_OFFLINE`, or break the build, on a reviewer's
    machine, and for the missing `TCP_NODELAY` socket option
    ([#4336](https://github.com/transact-rs/sqlx/pull/4336)), whose absence
    can add stalls of about 40 ms and would distort the phase-8 latency
    numbers.
  - Not fixed by the pin: in
    [#4445](https://github.com/transact-rs/sqlx/issues/4445) (open, and
    reproduced on main), a nested `begin()` (a savepoint) that fails or is
    cancelled rolls back the enclosing transaction or savepoint, later
    statements run outside any transaction, and the outer `commit()` still
    returns `Ok`. Until a release fixes it, no code calls `begin()` on a
    connection that is already in a transaction.
  - The sqlx repository has no `Cargo.lock`, so sqlx-cli's own dependencies
    cannot be locked; CI caches the built binary under the commit hash.
  - A regression test cancels a `begin_with` whose statement waits on an
    advisory lock held by another connection (for example
    `BEGIN; SELECT pg_advisory_xact_lock(1)`), releases the lock, and checks
    that the backend ends up `idle`, not `idle in transaction`. The test is
    first seen to fail on 0.9.0.
- The workspace `rust-version` becomes 1.94, sqlx 0.9's minimum.

## Consequences

- PostgreSQL itself validates every statement that moves money before the
  code can run.
- The cache is keyed by the text of each query, so a migration that changes
  a column without changing any query text goes unnoticed by offline
  builds. The macros also skip run-time type checks for typed columns, so a
  stale cache could decode a changed column into a wrong value. The CI check
  is the only guard, and it must stay required.
- Changing a query or a migration means regenerating the cache, which needs
  sqlx-cli and a running database.
- A git dependency on unreleased code is a deliberate, documented exception,
  revisited when 0.9.1 ships and when #4445 is fixed.
- `#[sqlx::test]` gives each test its own database with migrations applied.
  It connects as a superuser, so tests that must run as the runtime role
  switch to it (ADR-0004). Its pool has at most five connections, so
  concurrency tests build their own larger pool.
- Crates with many query macros compile more slowly.

## References

- sqlx, [`query!` macro documentation](https://docs.rs/sqlx/latest/sqlx/macro.query.html) (offline mode).
- sqlx issues [#4393](https://github.com/transact-rs/sqlx/issues/4393) and [#4445](https://github.com/transact-rs/sqlx/issues/4445), and fix [#4394](https://github.com/transact-rs/sqlx/pull/4394).
