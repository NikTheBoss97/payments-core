<!-- Author: Nicola Antonello -->

# ADR-0015: Posting errors and idempotent posting

- Status: accepted
- Date: 2026-10-20

## Context

The posting path needs an error model: the types that opening an account and
posting a ledger transaction return, and how each database outcome maps to
them. ADR-0013 fixes how database errors are recognised: `LG` codes are bugs,
and class-`23` errors are keyed by the pair (code, constraint name). ADR-0014
fixes what a failed `COMMIT` proves. ADR-0005 settles an unknown outcome by
sending the identical insert again, with the same IDs: if the first attempt
committed, the retry hits a duplicate key, and the stored rows are compared
with the intended ones.

`ledger_transactions` has two unique keys that contain `id`: its primary key
and the `(id, entry_count, currency)` key that the entries reference
(ADR-0010). A retry of a committed posting violates both. The executor
checks a table's indexes in OID order and reports the first violation, so
which name the error carries depends on index OIDs. After
`REINDEX INDEX CONCURRENTLY ledger_transactions_pkey`, which builds a new
index to replace the old one, the same retry reported
`ledger_transactions_id_entry_count_currency_key`.

## Options

Shape of the errors:

| Option | Why not |
|---|---|
| **One error enum per operation, classified in one place** | Chosen |
| One ledger-wide enum for every operation | Each signature admits variants that its operation never returns, so callers need catch-all arms, and "only this variant may have committed" lives in prose |
| Return `sqlx::Error` and offer helper functions | Nothing maps database errors to domain errors (ADR-0004, ADR-0010), and an unknown outcome looks like any other error, which invites a re-post with new IDs |

Settling an unknown outcome:

| Option | Why not |
|---|---|
| **`post()` is idempotent: calling it again with the same value settles it** | Chosen |
| A separate `recover()`, called only after an unknown outcome, while `post()` treats any duplicate ID as a bug | It would flag an ID reused by mistake on a first attempt, at the cost of a second entry point and of callers tracking which attempt they make. ADR-0005's amendment of 2026-10-20 already makes such reuse hard to write |

Implementing `Display` and `Error`:

| Option | Why not |
|---|---|
| **Derive them with `thiserror`** | Chosen |
| Write them by hand, as `money.rs` does | About three times the code, and a variant left out of `source()` drops the cause from every log without a compiler warning |

## Decision

- `post(pool, &LedgerTransaction)` returns `Applied` or `AlreadyApplied`.
  It is idempotent by transaction ID: calling it again with the same value
  is how an unknown outcome is settled (ADR-0005, ADR-0014).
- A duplicate-key error (`23505`) from the header insert, on either
  `ledger_transactions_pkey` or
  `ledger_transactions_id_entry_count_currency_key`, means that the ID
  exists. `post()` then rolls back and compares the stored entries with the
  intended ones: equal is `AlreadyApplied`, different is a bug.
- Its error, `PostError`, has four variants:
  - `AccountNotInCurrency`: `23503` on `ledger_entries_account_fkey`. The
    key reports both a missing account and an account in another currency,
    so the variant is named after the key;
  - `Invariant`: any `LG` code, or any other class-`23` error. The database
    caught what the domain types should have prevented: a bug, never
    retried;
  - `Mismatch`: the ID exists with different content, also a bug;
  - `NotConfirmed`: everything else. The posting may or may not be stored,
    and the caller calls `post()` again later with the same value.
- Opening and finding accounts, and the statement-level `post_in`
  (ADR-0011), have error types of their own, holding only what each can
  return.
- One pure function maps (stage, SQLSTATE, constraint name) to an outcome,
  with `NotConfirmed` as the catch-all. A thin adapter reads the code and
  the constraint name through sqlx's `DatabaseError` trait, so unit tests
  cover every branch of the mapping without a database.
- The error enums derive `thiserror::Error`. A variant that wraps a
  `sqlx::Error` keeps it as `#[source]`, never `#[error(transparent)]`,
  which forwards `source()` to the wrapped error and so hides it from the
  source chain. No variant converts from `sqlx::Error` with `#[from]`, so a
  `?` cannot skip the classification. The enums are not `#[non_exhaustive]`:
  the crate is not published, and an exhaustive `match` turns a new variant
  into a compile error at every caller.

## Consequences

- Callers follow one rule: after `NotConfirmed`, call `post()` again with
  the same value. Opening an account converges the same way, by its business
  key (ADR-0009).
- A `TransactionId` reused with identical content would read as
  `AlreadyApplied` rather than as a bug. IDs come from
  `TransactionId::now_v7()`, and the type is neither `Copy` nor `Clone`, so
  any reuse has to be written out (ADR-0005).
- Every posting may be a retry, so the lock timeout that bounds a retry's
  wait applies to every posting.
- `thiserror` 2 is already compiled as part of sqlx, so it adds no crate to
  the build. `money.rs` keeps its hand-written impls, which have no source
  error to forward.
- The SQLSTATE, constraint and DETAIL that ADR-0013 logs are read from the
  `#[source]` error, not copied into the variants.

## References

- PostgreSQL 18.6 source: `RelationGetIndexList()` in [relcache.c](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/cache/relcache.c) ("sorted in order by OID").
- PostgreSQL 18 manual, [REINDEX](https://www.postgresql.org/docs/18/sql-reindex.html) (`CONCURRENTLY`).
- [thiserror 2.0.21 documentation](https://docs.rs/thiserror/2.0.21/thiserror/) (`#[source]`, `#[from]`, `transparent`).
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests) (a retry returns the saved response; different parameters are an error).
