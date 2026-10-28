<!-- Author: Nicola Antonello -->

# ADR-0017: Payments and their append-only history

- Status: accepted
- Date: 2026-10-28

## Context

The lifecycle of ADR-0016 must hold in PostgreSQL as well as in Rust. The
failure model (DESIGN.md) includes application bugs and manual SQL run as
the runtime role, so invariant I5 and the refund limit must hold for any
statement that role can run, at any isolation level, as the ledger's rules
do (ADR-0004).

Three complete designs were written and judged for correctness, fit with
the earlier decisions, and simplicity:

- A: a mutable `status` column on the payment, changed by compare-and-set,
  plus a history of steps;
- B: no status column, and an append-only history whose newest row is the
  state;
- C: one append-only table per decision point (authorization, resolution,
  movements, settlement), with the state machine spread over their keys.

A and B tied, for separable reasons: A for how it serializes commands, B
for how it stores the state. This ADR takes B's storage; ADR-0018 takes
A's serialization.

## Options

| Option | Why not |
|---|---|
| **Append-only payments, a history chained by constraints, and one record table per step kind that carries data (design B)** | Chosen |
| A mutable status column guarded by a trigger, plus the history (design A) | Two copies of the state, reconciled at `COMMIT`. The `UPDATE` privilege it needs lets any runtime session lock a payment row `FOR UPDATE`, which blocks every insert that references the row |
| One table per decision point (design C) | The rules spread over 5 tables and 44 constraints, one resolution per payment rules out a later recapture, and the reviewers found two ways to link a fact to the wrong posting |
| A history validated by a trigger that replays it on every insert (Geisendörfer, 2017) | Safe only under `SERIALIZABLE` or with an explicit lock, as its author notes, and the rule lives in PL/pgSQL instead of in constraints |
| A history whose newest row carries a `most_recent` flag (GoCardless's Statesman) | Each step updates the previous row, which an append-only table forbids, and concurrent steps are settled by unique-key collisions and retries |

## Decision

- **Merchants.** `merchants` holds one row per merchant, its ID and
  `recorded_at`, and nothing else: names and other display data are not
  stored here, as for accounts (ADR-0009). Registering a merchant is
  `INSERT ... ON CONFLICT (id) DO NOTHING`. A payment references its
  merchant through a foreign key, so a payment for an unknown merchant is
  refused.
- **Payments.** `payments` holds a payment's immutable terms: `id`,
  `merchant_id`, `psp`, `currency`, `amount` (the requested amount, from 1
  to the cap of ADR-0003), `fee_bps` (the fee rate, 0 to 10,000 basis
  points, ADR-0020) and `recorded_at`. `psp` is a closed set of labels,
  today only `mock`. `currency` has no foreign key to `currencies`, like
  `ledger_transactions`: the payment's accounts are opened before the
  payment, and their key checks the currency (ADR-0019), while a key here
  would share-lock the currency's one row on every new payment.
- **The history.** `payment_transitions` has one row per step: `payment_id`,
  `seq`, `prev_seq`, `id` (the step's `TransitionId`), `kind`,
  `from_status`, `to_status`, `amount`, `prev_refundable`, `refundable` and
  `recorded_at`. The row with the highest `seq` is the payment's state, and
  no other copy of the status exists.
  - The primary key is `(payment_id, seq)`. `seq` is 0 for the first row,
    the genesis step of kind `create`, and `prev_seq + 1` after it.
  - `payment_transitions_prev_fkey`, on
    `(payment_id, prev_seq, from_status, prev_refundable)`, references
    `(payment_id, seq, to_status, refundable)`: every row starts from its
    predecessor's status and refundable amount. The genesis row points to
    itself, so every column stays `NOT NULL`.
  - The primary key and that foreign key let exactly one row follow any
    state, because a second row after the same predecessor has the same
    `(payment_id, seq)`.
  - `payment_transitions_edge_check` lists the legal triples
    (`from_status`, `kind`, `to_status`) of ADR-0016, and allows the
    genesis triple (`created`, `create`, `created`) at `seq` 0 only. Every
    illegal move fails with one pair: (`23514`,
    `payment_transitions_edge_check`).
  - `payment_transitions_money_check` states each kind's amount rule:
    - `create`: `amount`, `prev_refundable` and `refundable` are 0;
    - `authorize`: `amount`, the authorized amount, is at least 1, and
      `refundable` equals `prev_refundable`;
    - `capture`: `amount` is at least 1, `prev_refundable` is 0, and
      `refundable` equals `amount`;
    - `refund`: `amount` is between 1 and `prev_refundable`, `refundable`
      equals `prev_refundable - amount`, and `to_status` is `refunded`
      exactly when `refundable` is 0;
    - every other kind: `amount` is 0, and `refundable` equals
      `prev_refundable`.
  - `id` is unique across the steps of all payments, so a retry finds its
    step by `TransitionId` alone (ADR-0019).
- **Records.** A step whose kind carries data has one record, written in the
  same database transaction:
  - `payment_authorizations`: the authorized amount; the requested amount,
    copied through a foreign key to `payments (id, amount)` and checked to
    be no smaller; and `expires_at`;
  - `payment_captures`: the captured amount; the authorized amount, copied
    through a foreign key to the authorization and checked to be no
    smaller; the fee, from 0 to the captured amount; `captured_at`; and
    `ledger_transaction_id`;
  - `payment_refunds`: the refunded amount, `refunded_at` and
    `ledger_transaction_id`.

  A record's primary key is its step's ID, and a foreign key on
  `(transition_id, payment_id, kind, amount)` ties it to a step of the
  right kind with the same amount. `kind` is a `STORED` generated column
  that holds the table's one kind, so no caller writes it.
  `ledger_transaction_id` is unique and references `ledger_transactions
  (id)` (ADR-0011). Records carry no `recorded_at`: they commit with their
  step, whose `recorded_at` is the same instant (ADR-0012).
- **Deferred checks.** Three checks run at `COMMIT` as constraint triggers,
  `DEFERRABLE INITIALLY DEFERRED`, each with its own code (ADR-0013):
  - `LG004`, `payment_transitions_commit_check`: a step of kind
    `authorize`, `capture` or `refund` has its record;
  - `LG005`, `payments_commit_check`: a payment has its genesis step;
  - `LG006`, `payment_captures_commit_check` and
    `payment_refunds_commit_check`: the record's posting was written by this
    database transaction (its header row's
    `xmin = pg_current_xact_id()::xid`), and its entries are exactly the
    record's lines, no more and no fewer (ADR-0020, ADR-0021).

  Like the ledger's triggers, they are `ENABLE ALWAYS`, pin their
  `search_path`, and compare domain values as `text` (ADR-0004).
- **Append-only.** `merchants`, `payments`, `payment_transitions` and the
  three record tables use the ledger's `reject_change()` trigger, which
  raises `LG001` on `UPDATE`, `DELETE` and `TRUNCATE`.
- **Privileges.** The runtime role has `SELECT` on every payment table, and
  `INSERT` on every column except `recorded_at` and the generated `kind`
  columns, each granted by name. It has no `UPDATE`, `DELETE` or
  `TRUNCATE`, so it cannot lock a row either: every locking clause of
  `SELECT` needs `UPDATE` on the table.
- **Why `refundable` is not a stored balance.** ADR-0002 rejects stored
  balances because they drift and because every writer to an account must
  queue on one row. Neither applies here:
  - `refundable` is written once and never updated. The foreign key and
    the money check fix each row's value from its predecessor's, so by
    induction from the genesis row it always equals the captured amount
    minus the refunds.
  - Only one payment's commands write its history, and they are already
    serialized (ADR-0018).

  The stored value is what lets PostgreSQL refuse an over-refund as a row
  constraint, at any isolation level; a check that summed the refunds would
  be safe only under a lock or `SERIALIZABLE`. It is a business quantity of
  one payment, not an account balance: balances still come only from the
  ledger's entries.
- **`NOT NULL` everywhere.** Every column of every table is `NOT NULL`, the
  scope that the catalog test already enforces.

## Consequences

- I5 and the refund limit hold for any SQL the runtime role runs, whether
  or not it takes the payment's lock: an illegal move fails the edge check,
  a second step after the same state fails the primary key, and a refund
  above the refundable amount fails the money check.
- No index holds the current status. Finding the payments that are
  currently `captured` is an anti-join over the history, about 15 ms at
  40,000 rows in a single lab run, so phase-4 sweeps start from records,
  such as `expires_at`.
- The history has four unique indexes, a write cost for phase 8 to measure.
- An append-only table whose columns are all `NOT NULL` never gains a column
  (ADR-0011). Later facts about a step, such as a PSP's reference, go into
  new tables keyed by the step.
- A new move changes the edge check, in a migration that takes an
  `ACCESS EXCLUSIVE` lock on the history while it runs.
- `LG006` repeats the posting rule of ADR-0020 in PL/pgSQL, on purpose, as
  ADR-0004 repeats the ledger's rules in the database. Rows written inside
  a savepoint carry the savepoint's own transaction ID, so a posting written
  there fails the `xmin` test. Application code uses no savepoints
  (ADR-0006), and tests must not use them for postings either.
- A superuser session with `session_replication_role = replica` skips
  foreign keys, the history's chain among them. That is outside the failure
  model, and the `ENABLE ALWAYS` triggers still fire.
- Catalog tests pin the tables, their columns and types, every constraint
  and trigger name, and the runtime role's privileges.
- Each new payment takes a `FOR KEY SHARE` lock on its merchant's row: the
  MultiXact traffic that ADR-0002 describes for a busy account.

## References

- PostgreSQL 18 manual, [Constraints](https://www.postgresql.org/docs/18/ddl-constraints.html) (check constraints; foreign keys over several columns).
- PostgreSQL 18 manual, [Generated Columns](https://www.postgresql.org/docs/18/ddl-generated-columns.html).
- PostgreSQL 18 manual, [CREATE TRIGGER](https://www.postgresql.org/docs/18/sql-createtrigger.html) (constraint triggers, `DEFERRABLE`).
- PostgreSQL 18 manual, [System Columns](https://www.postgresql.org/docs/18/ddl-system-columns.html) (`xmin`) and [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html) (`pg_current_xact_id`).
- PostgreSQL 18 manual, [SELECT](https://www.postgresql.org/docs/18/sql-select.html#SQL-FOR-UPDATE-SHARE) (the locking clause needs `UPDATE` privilege).
- Felix Geisendörfer, [Implementing State Machines in PostgreSQL](https://felixge.de/2017/07/27/implementing-state-machines-in-postgresql/) (2017).
- GoCardless, [Statesman](https://github.com/gocardless/statesman) (a state machine whose transitions are stored as rows).
- Pat Helland, [Immutability Changes Everything](https://www.cidrdb.org/cidr2015/Papers/CIDR15_Paper16.pdf) (CIDR 2015).
