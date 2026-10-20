<!-- Author: Nicola Antonello -->

# ADR-0004: Enforcing the ledger invariants

- Status: accepted; partly superseded by ADR-0010
- Date: 2026-10-14

## Context

Invariant I1 (every ledger transaction sums to zero per currency) and
invariant I2 (entries are append-only) must hold even when application code
has a bug, or when a future code path writes to the ledger without going
through the domain layer. DESIGN.md also requires each ledger transaction to
use a single currency. Relying on one layer means one bug is enough to store
a bad transaction.

Two terms are kept apart below. A **ledger transaction** is a balanced group
of entries: one row in `ledger_transactions` plus its rows in
`ledger_entries`. A **database transaction** is one PostgreSQL `BEGIN` ...
`COMMIT`, which may write several ledger transactions.

## Options

Balanced ledger transactions (I1):

| Option | Why not |
|---|---|
| Rust domain type only | A bug, a manual SQL session or a future service can still write unbalanced rows |
| **Rust domain type plus a commit-time check in PostgreSQL** | Chosen |
| A `CHECK` constraint | Sees only the row being written; no declarative constraint can check a sum over rows (the SQL standard's `CREATE ASSERTION` is not implemented in PostgreSQL) |
| Writes allowed only through a PL/pgSQL function | Moves the whole write path (building and inserting entries) into PL/pgSQL and duplicates the Rust domain type; the chosen design keeps only small checks there |
| Transfer rows that are balanced by construction | Every transaction with more than two entries would have to be split into pairs (see ADR-0003) |

Complete ledger transactions:

| Option | Why not |
|---|---|
| **The header declares its number of entries, and entries are numbered within it** | Chosen |
| Record the creating database transaction on the header, and reject entries written by any other | More trigger logic, and running the check early with `SET CONSTRAINTS ... IMMEDIATE` lets later entries in the same database transaction through |
| Rely on the domain type alone | Nothing in the database stops entries being appended to a committed ledger transaction, or an empty one committing |

Append-only rows (I2):

| Option | Why not |
|---|---|
| **An append-only trigger, plus a runtime role without `UPDATE`, `DELETE` and `TRUNCATE`** | Chosen |
| Privileges only | Nothing stops the owner, or a role given the privileges by mistake, from rewriting entries |
| A trigger only, with the application connecting as the owner | The application could switch the trigger off with `ALTER TABLE ... DISABLE TRIGGER` |

## Decision

- **In Rust**, a ledger transaction can only be built through a constructor
  that checks its entries: at least two, one currency, no zero amounts, each
  within the cap from ADR-0003, and a sum of zero. An invalid transaction
  cannot be represented, so the rest of the code never needs to re-check it
  ("parse, don't validate").
- **Sealing.** Each `ledger_transactions` row declares its number of entries
  in `entry_count` (at least 2), with `UNIQUE (id, entry_count)`. Every entry
  repeats the count through a composite foreign key
  `(transaction_id, entry_count)` to `ledger_transactions (id, entry_count)`,
  and `CHECK (line_no BETWEEN 1 AND entry_count)`. With the primary key
  `(transaction_id, line_no)` (ADR-0005), a ledger transaction can never hold
  more entries than it declared.
- **Commit-time check.** A deferred constraint trigger on
  `ledger_transactions` checks, at `COMMIT`, that each new ledger transaction
  has exactly `entry_count` entries, all in one currency, summing to zero. A
  constraint trigger is an `AFTER ROW` trigger whose firing can be deferred
  like a constraint; `DEFERRABLE INITIALLY DEFERRED` runs it at `COMMIT`.
  Without deferral it would run at the end of the statement that inserted
  the header, before any entry exists. Because every slot must be filled and
  the slots are fixed, no entry can be added once the check has passed, even
  if `SET CONSTRAINTS ALL IMMEDIATE` runs it early, and a header with no
  entries cannot commit.
- **Append-only tables.** `accounts`, `ledger_transactions` and
  `ledger_entries`, and the reference table `currencies` (ADR-0003), never
  change after insert: a trigger rejects `UPDATE`, `DELETE` and `TRUNCATE`
  on all four. Mistakes are corrected by posting a reversing ledger
  transaction; an account that needs different attributes is replaced by a
  new account.
- **Triggers always fire.** Both triggers are marked `ENABLE ALWAYS`.
  `session_replication_role = replica`, a setting meant for replication
  tools, otherwise stops ordinary triggers from firing.
- **Pinned search path.** The trigger functions are declared with
  `SET search_path = pg_catalog, public, pg_temp`. Otherwise PostgreSQL looks
  up their table names in the session's temporary schema first, and a
  temporary table named `ledger_entries` would be what the commit-time check
  reads.
- **Roles.** Migrations run as a role that owns the tables. The application
  connects as a runtime role that is not a superuser, does not own the
  tables and is not a member of the owning role, because a member can use
  the owner's right to disable triggers. It has `SELECT` on `currencies`,
  and `SELECT` and `INSERT` on `accounts`, `ledger_transactions` and
  `ledger_entries`.
- **Tests** write bad rows with raw SQL as the runtime role, bypassing the
  domain type, and expect the write or the `COMMIT` to fail: an unbalanced,
  mixed-currency or incomplete ledger transaction, an empty header, an entry
  appended to a committed ledger transaction, an extra entry after
  `SET CONSTRAINTS ALL IMMEDIATE`, and an unbalanced transaction next to a
  temporary table named `ledger_entries`. `#[sqlx::test]` connects as a
  superuser (ADR-0006), so these tests switch to the runtime role with
  `SET ROLE` first. For I2, `UPDATE`, `DELETE` and `TRUNCATE` must fail with
  a permission error as the runtime role, and with the trigger's error as
  the owner role, so each guard is tested on its own.

## Consequences

- Two independent layers must both fail before an unbalanced, incomplete or
  mixed-currency ledger transaction is stored.
- The two triggers repeat a small, fixed set of rules in PL/pgSQL on
  purpose, as a second layer; all posting logic stays in Rust.
- Triggers can still be switched off on purpose: a superuser or the table
  owner can run `ALTER TABLE ... DISABLE TRIGGER`, and
  `session_replication_role = replica` (settable by superusers and by roles
  granted `SET` on it) still stops foreign-key checks, which PostgreSQL
  implements as triggers. The role split takes that power away from the
  application; only superusers and the owner role that runs migrations keep
  it. The failure model in DESIGN.md assumes nobody uses it on purpose.
- I2 has three guards: the domain layer has no update path, the append-only
  trigger rejects changes, and the runtime role lacks the privileges.
- The official PostgreSQL image creates its default user as a superuser, so
  local and CI setup must create the two roles explicitly.
- An unbalanced ledger transaction is reported when `COMMIT` fails, not at
  `INSERT`. The trigger raises a dedicated error code, which the application
  maps to a domain error. Any error PostgreSQL returns for `COMMIT` means the
  database transaction rolled back. If the connection drops before `COMMIT`
  returns, the outcome is unknown, and it is settled as described in
  ADR-0005, never by re-posting with new IDs.
- The check runs once per ledger transaction, from its header row, so large
  settlement batches (phase 6) cost linear time. Skipping ledger transactions
  that were already checked was rejected: the marker would sit in a session
  setting or a temporary table, which any session can write.
- The header must be inserted before its entries, because the entries'
  foreign key refers to it.

## Amendments

- 2026-10-17: ADR-0010 supersedes three parts of this decision. The header
  declares its currency, its key `UNIQUE (id, entry_count)` becomes
  `UNIQUE (id, entry_count, currency)`, the entries' key to the header
  carries the currency as well, and the commit-time check no longer tests
  the currency: a mixed-currency entry now fails at `INSERT`.
- 2026-10-17: the runtime role's `INSERT` on `accounts` and
  `ledger_transactions` covers every column except `recorded_at`, which only
  the database sets (ADR-0012).
- 2026-10-17: the "dedicated error code" is one code per rule: `LG002` for
  an incomplete ledger transaction and `LG003` for an unbalanced one
  (ADR-0013).
- 2026-10-18: on a table that a foreign key references, the owner-role test
  runs `TRUNCATE ... CASCADE`. A plain `TRUNCATE` fails with `0A000` before
  any `BEFORE TRUNCATE` trigger runs, so it would never reach the guard;
  with `CASCADE`, the named table's trigger fires first. The append-only
  error names its trigger, table and schema in their own fields (ADR-0013),
  so the test can tell which trigger fired.

## References

- Alexis King, [Parse, don't validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/), 2019.
- PostgreSQL 18 manual, [CREATE TRIGGER](https://www.postgresql.org/docs/18/sql-createtrigger.html) (constraint triggers).
- PostgreSQL 18 manual, [ALTER TABLE](https://www.postgresql.org/docs/18/sql-altertable.html) (`ENABLE ALWAYS TRIGGER`).
- PostgreSQL 18 manual, [Client Connection Defaults](https://www.postgresql.org/docs/18/runtime-config-client.html) (`session_replication_role`).
- PostgreSQL 18 manual, [Schemas](https://www.postgresql.org/docs/18/ddl-schemas.html) (the schema search path).
