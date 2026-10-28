<!-- Author: Nicola Antonello -->

# ADR-0021: What causes a posting

- Status: accepted
- Date: 2026-10-28

## Context

ADR-0011 keeps causes off the ledger: business tables reference the ledger
rows they caused, and nothing points the other way. It left three questions
to phase 2:

1. Is a posting without a recorded cause detected, or refused?
2. How many causes may one posting have?
3. Is a reason code needed?

Two attacks, run against the design by its reviewers as the runtime role,
shaped the answers:

- one posting, linked as one payment's capture and as another payment's
  refund, committed when the database checked only each record's own PSP
  line;
- a refund linked to an older posting that nothing had caused, and that
  debited a different merchant, committed although every checked amount
  matched.

## Options

| Option | Why not |
|---|---|
| **Refuse on the payment side, detect on the ledger side** | Chosen |
| Require only that a link exists, through a unique `NOT NULL` foreign key | Both attacks commit, and so does a refund of 50 linked to a posting of 1 |
| Also check the PSP's line of the linked posting | One posting can still serve two records, and the merchant and fee lines are checked by Rust tests alone |
| Refuse at the ledger, with a deferred check on `ledger_transactions` that looks for a cause | Reverses ADR-0011's direction of dependency, breaks `post()` and the phase-1 tests, and blocks phase 6 until its cause tables exist |

## Decision

- **Refused on the payment side.** A capture or refund step without its
  record fails `LG004` at `COMMIT`, and a record with a wrong posting fails
  `LG006` (ADR-0017). A record's posting is right only when:
  - its header row was written by the same database transaction, which
    its `xmin` proves: `xmin = pg_current_xact_id()::xid`; and
  - its entries are exactly the record's lines of ADR-0020, with the same
    accounts, signs and amounts, and no line missing or added.
- **One cause per posting.** Each record table holds a posting's ID at most
  once, under a unique key. A capture's posting debits the PSP's
  receivable while a refund's credits it, so no posting matches both a
  capture and a refund. A phase-6 batch posting will have one cause per
  entry: each settlement row references `ledger_entries (transaction_id,
  line_no)` under a unique key of its own, with an `LG006`-style check of
  the entry (ADR-0011).
- **Detected on the ledger side.** A query lists the ledger transactions
  that no business record references. Phase-2 tests run it after each
  payment flow and expect no rows, and phase 6's reconciliation runs it as
  well.
- **No reason code.** The table that holds the record already says why the
  money moved.
- **`post()` stays public and records no cause,** so the ledger stays
  independent of the business tables, and the phase-1 tests do not change.

## Consequences

- Both attacks fail with `LG006` at `COMMIT`, while an honest capture or
  refund, written in one database transaction, commits.
- `LG006` repeats the posting rule in PL/pgSQL, about 25 more lines, on
  purpose, as ADR-0004 repeats the ledger's rules.
- A posting written inside a savepoint fails the `xmin` test, because rows
  written there carry the savepoint's own transaction ID (ADR-0017).
- A posting made with `post()` outside a payment flow is reported by the
  detection query, not refused: the ledger accepts it, as in phase 1.
- ADR-0011's comparison after a lost `COMMIT` includes the record, the row
  that links the posting to its cause (ADR-0019).

## References

- Ross Anderson, [Security Engineering, 3rd edition](https://www.cl.cam.ac.uk/archive/rja14/book.html) (Wiley, 2020), chapter 12 (the Clark-Wilson model: data changes only through well-formed transactions, each leaving a record).
- Ilya Ganelin, [Ledger: Stripe's system for tracking and validating money movement](https://stripe.dev/blog/ledger-stripe-system-for-tracking-and-validating-money-movement) (Stripe, 2024).
- Bill Karwin, [SQL Antipatterns, Volume 1: Avoiding the Pitfalls of Database Programming](https://pragprog.com/titles/bksap1/sql-antipatterns-volume-1/) (Pragmatic Bookshelf, 2022), chapter "Polymorphic Associations".
- PostgreSQL 18 manual, [System Columns](https://www.postgresql.org/docs/18/ddl-system-columns.html) (`xmin`) and [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html) (`pg_current_xact_id`).
