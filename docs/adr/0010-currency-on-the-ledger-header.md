<!-- Author: Nicola Antonello -->

# ADR-0010: One currency per ledger transaction, declared on its header

- Status: accepted; supersedes part of ADR-0004
- Date: 2026-10-17

## Context

DESIGN.md requires each ledger transaction to use a single currency.
ADR-0004 enforced that in the deferred commit-time trigger, together with
completeness and balance, so a mixed-currency transaction was rejected only
at `COMMIT`.

"Every entry has the same currency" is a rule over a set of rows. A `CHECK`
sees one row, and PostgreSQL does not implement the SQL standard's
`CREATE ASSERTION`. But a set rule can be turned into row rules: give the
parent the single value, and make every child equal to it. Equality is
transitive, so all the children then agree. ADR-0004 already uses this move
twice: `entry_count` seals a ledger transaction, and `(account_id, currency)`
ties an entry to its account.

## Options

| Option | Why not |
|---|---|
| Keep the rule in the commit-time trigger (ADR-0004 as accepted) | The error arrives only at `COMMIT`, after the rest of the unit of work has run, and it does not name the statement that broke the rule |
| **Currency on the header, carried to every entry by the header's foreign key** | Chosen |
| Currency on the header, with a second foreign key `(transaction_id, currency)` | Names the currency rule separately, but adds a third unique index to the header, which every posting must update |
| An exclusion constraint on the entries (`btree_gist`: `transaction_id WITH =, currency WITH <>`) | A GiST index on the busiest table, and an extension to maintain |

## Decision

- `ledger_transactions` gets `currency currency_code NOT NULL`.
- `UNIQUE (id, entry_count, currency)` replaces `UNIQUE (id, entry_count)`.
  A foreign key must reference columns that form a primary key, a unique
  constraint or a non-partial unique index, so this wider key is declared
  although `id` alone is unique.
- The entries' key to their header becomes
  `CONSTRAINT ledger_entries_header_fkey FOREIGN KEY (transaction_id, entry_count, currency) REFERENCES ledger_transactions (id, entry_count, currency)`.
  It is not deferrable: the header is inserted before its entries
  (ADR-0004). Together with `(account_id, currency)`, an entry's currency
  must equal both its header's and its account's.
- `ledger_transactions.currency` has no foreign key to `currencies`. Every
  entry's account already references `currencies`, and the key would lock
  that currency's row in every posting.
- The commit-time trigger checks only completeness and balance, each with
  its own SQLSTATE (ADR-0013).
- In Rust, `23503` on `ledger_entries_header_fkey` maps to one internal
  error, "entry does not match its ledger transaction". When a row breaks
  two foreign keys, which one PostgreSQL reports depends on how the names of
  their internal triggers sort, and those names contain object IDs, so code
  never infers a rule from the key that was reported.

## Consequences

- A mixed-currency entry fails at the `INSERT` that writes it, with the
  standard `23503` and the constraint's name, and
  `SET CONSTRAINTS ALL DEFERRED` cannot postpone it.
- The header records its currency, so readers and outbox payloads need not
  look at the entries for it. In the header row, the short currency code
  fits into the alignment padding before `recorded_at` (ADR-0012).
- One constraint name covers three causes: no such header, a different
  `entry_count`, or a different currency. The error's DETAIL tells them
  apart.
- PostgreSQL implements foreign keys with internal triggers, which
  `session_replication_role = replica` skips, while a user trigger marked
  `ENABLE ALWAYS` still fires. So the currency rule, like the seal, no
  longer holds in replica mode, which only a superuser or a role granted
  `SET` on that setting can enter; the failure model excludes them
  (DESIGN.md). A table owner that is not a superuser can still drop the
  constraint, or since PostgreSQL 18 mark it `NOT ENFORCED`. Only
  `ALTER CONSTRAINT ... ENFORCED` checks the stored rows again; dropping the
  key and adding it back `NOT VALID` does not.
- The column was cheap to add only while the tables were empty. Added
  later, every existing header would have needed a currency computed from
  its entries, an `UPDATE` that the append-only trigger rejects.

## References

- PostgreSQL 18 manual, [Constraints](https://www.postgresql.org/docs/18/ddl-constraints.html#DDL-CONSTRAINTS-FK) (foreign keys).
- PostgreSQL 18 manual, [CREATE TABLE](https://www.postgresql.org/docs/18/sql-createtable.html) (`REFERENCES`, `DEFERRABLE`, `NOT ENFORCED`).
- PostgreSQL 18 manual, [SET CONSTRAINTS](https://www.postgresql.org/docs/18/sql-set-constraints.html).
- PostgreSQL 18 manual, [ALTER TABLE](https://www.postgresql.org/docs/18/sql-altertable.html) (`NOT VALID`; `ALTER CONSTRAINT ... ENFORCED`).
- PostgreSQL 18 manual, [Client Connection Defaults](https://www.postgresql.org/docs/18/runtime-config-client.html) (`session_replication_role`).
