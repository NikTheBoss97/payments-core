<!-- Author: Nicola Antonello -->

# ADR-0011: The ledger records how money moved, not why

- Status: accepted
- Date: 2026-10-17

## Context

Every ledger transaction has a cause: a payment's capture or refund
(phase 2), or a line of a settlement file (phase 6). Something must link the
two, and the link decides which layer depends on which. Ledger rows are
append-only (ADR-0004), so whatever they carry is frozen, and every ledger
column is `NOT NULL` (ADR-0003).

A foreign key is a dependency. Pointing from the ledger to a business table
makes the lowest layer depend on a higher one, and each new kind of cause
then needs another column. Pointing from a business table to the ledger
keeps the ledger independent and lets each business table state its own
cardinality.

## Options

| Option | Why not |
|---|---|
| **Nothing on ledger rows; business tables reference them** | Chosen |
| The header references its cause, such as a nullable `payment_id`, later joined by one column per new cause | Nullable ledger columns, and a single `payment_id` cannot describe a settlement batch that covers many payments |
| A `(cause_type, cause_id)` pair on the header | No foreign key can check it: the Polymorphic Associations antipattern |
| A description or `jsonb` metadata on the header | Frozen by the append-only trigger, opaque to constraints (a JSON `null` passes `NOT NULL`), a place where personal data could never be erased, and a second copy of the cause that can drift from the real link |
| A closed reason code on the header, such as capture, refund or settlement | Its vocabulary belongs to phase 2, and as a foreign key to a table of codes it would lock the same row in every posting of a kind, the hot-row problem of ADR-0002 |

## Decision

- No ledger table has a foreign key to a business table, and no ledger row
  carries a description, metadata or reason code.
- From phase 2, a business table that causes postings references
  `ledger_transactions (id)` through a `NOT NULL` foreign key, written in the
  same database transaction, after the ledger rows it references. When one
  posting covers several business objects, such as a settlement batch, each
  object references its own entry, `ledger_entries (transaction_id, line_no)`.
- Facts learned later about ledger rows go into new tables keyed by ledger
  IDs.
- The function that writes a posting takes the caller's database
  transaction (`&mut Transaction<'_, Postgres>`) and never calls `begin()` or
  `commit()`. A thin `post()` wrapper owns `BEGIN` and `COMMIT` for phase 1.
  Phase 2 can then write a payment's state change, its postings and the link
  in one database transaction (invariant I6) without the nested `begin()`
  that ADR-0006 rules out.

## Consequences

- The ledger schema can stay fixed through phases 2 to 6, and PostgreSQL
  checks every link with a typed, `NOT NULL` foreign key to one table.
- Asking why money moved, starting from the ledger, needs a join from the
  business side.
- Until phase 2, a posting can exist without a recorded cause. Phase 2
  decides whether to detect or refuse that, how many causes a posting may
  have, and whether a reason code is needed.
- This departs from Stripe's balance transactions (`source`), Modern
  Treasury's ledger transactions (`ledgerable_type` and `ledgerable_id`) and
  TigerBeetle's transfers (`code` and `user_data`), which point from the
  ledger record to its cause. Modern Treasury and TigerBeetle run apart from
  their users' business data, where no foreign key could check a link in
  either direction; here the ledger and the business tables share one
  PostgreSQL database.
- A column added later to a populated append-only table could give the
  existing rows only an invented default, a NULL hidden behind
  PostgreSQL 18's `NOT NULL ... NOT VALID`, or values written with the
  append-only trigger switched off, and the failure model and the `NOT NULL`
  rule exclude all three. Growing by new tables avoids the problem.
- After a lost `COMMIT`, the comparison described in ADR-0005 belongs to
  whoever owns the unit of work; in phase 2 it includes the link row.

## Amendments

- 2026-10-28: ADR-0021 answers the three questions left to phase 2. A
  posting without a cause is refused on the payment side (`LG004`,
  `LG006`) and detected on the ledger side by a query. A capture or refund
  posting has exactly one cause, and a phase-6 batch posting one per entry.
  No reason code is added. The link row of the last consequence is the
  capture or refund record (ADR-0019).

## References

- Bill Karwin, [SQL Antipatterns, Volume 1: Avoiding the Pitfalls of Database Programming](https://pragprog.com/titles/bksap1/sql-antipatterns-volume-1/) (Pragmatic Bookshelf, 2022), chapter "Polymorphic Associations".
- PostgreSQL 18 manual, [JSON Types](https://www.postgresql.org/docs/18/datatype-json.html) (a JSON `null` is not an SQL NULL).
- Stripe, [The Balance Transaction object](https://docs.stripe.com/api/balance_transactions/object).
- Modern Treasury, [Ledger Transaction object](https://docs.moderntreasury.com/platform/reference/ledger-transaction-object).
- TigerBeetle, [Transfer](https://docs.tigerbeetle.com/reference/transfer/).
- sqlx issue [#4445](https://github.com/transact-rs/sqlx/issues/4445) (`begin()` on a connection already in a transaction).
