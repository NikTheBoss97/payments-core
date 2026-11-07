<!-- Author: Nicola Antonello -->

# payments-core: Design

This is a living document. Decisions behind it are recorded in [`adr/`](adr/).

## Goals

payments-core is a payment processing backend built around a double-entry ledger.
Its main property is **correctness under failure**: money is never lost,
duplicated, or created, even when:

- the process crashes between any two steps,
- clients retry requests, including concurrently,
- the payment provider (PSP) delivers webhooks late, twice, or out of order,
- many requests touch the same account at the same time.

Features matter less than that guarantee. Each invariant below has a stated
enforcement mechanism and a test that tries to break it.

## Non-goals

- **Real card data or PCI scope.** No PAN or CVV is ever accepted. The PSP is a mock service.
- **Currency conversion.** A single ledger transaction is in one currency. FX is out of scope.
- **Fractions of a minor unit, and crypto assets.** Every amount is a whole number of minor units of an ISO 4217 currency ([ADR-0003](adr/0003-money-representation.md)).
- **User accounts, OAuth and per-key permissions.** A merchant authenticates with an API key, and all of its keys can do the same ([ADR-0024](adr/0024-authentication.md)).
- **Captures and refunds that fail after the PSP confirmed them.** Such a fact stops its payment for an operator ([ADR-0028](adr/0028-requests-to-the-psp.md), [ADR-0031](adr/0031-the-psps-history.md)).
- **Horizontal database scaling.** One PostgreSQL primary. Sharding and multi-region are out of scope.
- **A UI.**

## Failure model

| Assumed to happen                                    | Assumed not to happen                        |
|------------------------------------------------------|----------------------------------------------|
| Process crash at any instruction                     | Loss of committed PostgreSQL data            |
| Client timeouts and retries with the same key        | Clients reusing a key for a different request (detected and rejected, not merged) |
| Concurrent requests on the same payment or account   | Clock agreement between services             |
| Webhooks duplicated, delayed, reordered, lost or forged ([ADR-0031](adr/0031-the-psps-history.md)) | A PSP whose history is false, or changes what it already reported |
| PSP timeouts with unknown outcome                    |                                              |
| A PSP that forgets an idempotency key after a day, while keeping its records ([ADR-0028](adr/0028-requests-to-the-psp.md)) |              |
| A PSP that is down, or answers 429 and 503, for hours ([ADR-0032](adr/0032-answers-and-retries.md)) |              |
| Application bugs and manual SQL run as the runtime role, including code that bypasses the domain layer or skips a payment's lock | A superuser or the migration role (the tables' owner) deliberately disabling the ledger's triggers or foreign-key checks |
| A session that stalls while holding a lock, or leaks a session-level advisory lock ([ADR-0018](adr/0018-serializing-a-payments-commands.md)) |                                              |
| Lost `COMMIT` acknowledgements (the service cannot tell whether PostgreSQL committed) |              |
| A `COMMIT` answered with an error after it committed, when a cancel or terminate signal arrives during the commit ([ADR-0014](adr/0014-what-a-failed-commit-proves.md)) |              |
| A client that hangs up mid-request ([ADR-0023](adr/0023-the-http-api.md)) |              |
| Requests with a missing, forged or revoked API key, and merchants asking for each other's payments | A stolen API key used before it is revoked, which nothing can tell from its merchant ([ADR-0024](adr/0024-authentication.md)) |
| A code path that forgets to name its merchant, or names the wrong one | SQL written on purpose, as the tenant role, that names another merchant ([ADR-0025](adr/0025-tenant-isolation.md)) |

## Invariants

| ID | Invariant | Enforced by | Tested by |
|----|-----------|-------------|-----------|
| I1 | Every ledger transaction's entries sum to zero per currency, so the whole ledger sums to zero in each currency. | Domain type that cannot be built unbalanced, plus a commit-time check of each sealed ledger transaction in PostgreSQL ([ADR-0004](adr/0004-enforcing-ledger-invariants.md)); a foreign key that makes every entry repeat its ledger transaction's currency ([ADR-0010](adr/0010-currency-on-the-ledger-header.md)); `NOT NULL` ledger columns, so no entry escapes those checks ([ADR-0003](adr/0003-money-representation.md)). | Unit and property tests of the domain type; database tests, as the runtime role, that write unbalanced, incomplete, mixed-currency or appended ledger transactions with raw SQL; posting tests in which a trigger corrupts a write that the types allowed, so that only the commit-time check can reject it; a catalog test that pins every ledger column's type and `NOT NULL`; a single-statement trial balance (every currency sums to zero across the ledger). |
| I2 | Ledger entries are append-only. Corrections are new, reversing transactions. | A trigger rejecting `UPDATE`, `DELETE` and `TRUNCATE`, and a runtime role without those privileges ([ADR-0004](adr/0004-enforcing-ledger-invariants.md)). | Database tests that attempt `UPDATE`, `DELETE` and `TRUNCATE` as the runtime role (expecting a permission error) and as the owner role (expecting the trigger's error), so each guard is tested on its own; catalog tests that pin the runtime role's privileges and grant options, check that it can neither create objects in `public` nor act through another role, and pin every guard trigger, its `ENABLE ALWAYS` and its function's `search_path`. |
| I3 | No balance is read from a mutable column that can drift from the entries. Balances are derived from entries. | No account balance is stored anywhere ([ADR-0002](adr/0002-balances-derived-from-entries.md)). A payment's refundable amount, kept on each row of its history, is written once and fixed by constraints from the row before it ([ADR-0017](adr/0017-payments-and-their-history.md)). | Holds by construction. Tests check derived balances against an independent in-memory model under concurrent writes. |
| I4 | One idempotency key produces at most one effect. | The key's record commits in the same transaction as its effect, under a primary key of its merchant and key, and holds the final answer that every retry gets ([ADR-0026](adr/0026-idempotency-keys.md)). | Copies of one request racing; the backend killed around the key's insert and during `COMMIT`; the server process killed with commands in flight; a late duplicate of a refusal after the payment has moved on. |
| I5 | A payment never makes an invalid state transition. | An exhaustive transition function whose result is the only input the step writer accepts ([ADR-0016](adr/0016-payment-lifecycle.md)); in PostgreSQL, an append-only history whose edge check, predecessor foreign key and primary key let only a legal step follow the current state, for any SQL the runtime role runs ([ADR-0017](adr/0017-payments-and-their-history.md)); one command at a time per payment, under a transaction-level advisory lock ([ADR-0018](adr/0018-serializing-a-payments-commands.md)). | Table tests of all 63 (status, event) pairs in Rust, and of every (from, kind, to) triple against the database as the runtime role; property tests of command sequences against a model; race tests of concurrent commands on one payment. |
| I6 | A payment state change, its ledger entries, and its outbound event commit atomically. | A payment's step, its posting and its record commit in one database transaction, through the one function that applies the rule of [ADR-0014](adr/0014-what-a-failed-commit-proves.md) ([ADR-0019](adr/0019-payment-commands-retries-and-errors.md)); deferred checks refuse a capture or refund without its record, or with a posting that is not exactly its lines written in the same transaction ([ADR-0021](adr/0021-what-causes-a-posting.md)). *The outbound event joins them with the outbox in phase 5.* | Crash-injection tests that kill the backend around each statement; tests that prove each deferred check rolls its transaction back. |
| I7 | A merchant can read or change only its own payments. | One check in Rust on every route that takes a payment's ID, and row-level security on every table a merchant owns, under a login role that sees nothing until a transaction names its merchant ([ADR-0025](adr/0025-tenant-isolation.md)). | Every route tried across merchants; each guard broken alone, to show that the other still holds; catalog tests that pin every policy and each role's `BYPASSRLS`. |
| I8 | Only the PSP's word moves a payment: apart from its creation and a cancellation, every step comes from a fact of the PSP's history, applied once and in the PSP's order, so what the service holds of that history is always a prefix of it. | Only the catch-up applies facts, through a type that only it can build; the merchant's role may insert only `create` and `cancel` steps, and no ledger line; facts chained by position under a primary and a predecessor foreign key, and checked against the PSP's history at every read; a deferred check (`LG007`) that refuses a step of the PSP's without its fact ([ADR-0031](adr/0031-the-psps-history.md), [ADR-0034](adr/0034-only-the-psps-word-moves-a-payment.md)). *In progress.* | A property test that delivers a PSP history's notifications duplicated, reordered and partly lost; each guard broken alone; a catalog test of the merchant role's privileges and policies; the backend killed around each statement of the catch-up. |
| I9 | A request sent to the PSP takes effect there at most once, even after the PSP has forgotten its idempotency key. | A stable ID per request, one sender, a lookup in the PSP's records before every resend, and one decision per request under a unique key ([ADR-0028](adr/0028-requests-to-the-psp.md), [ADR-0034](adr/0034-only-the-psps-word-moves-a-payment.md)). *In progress.* | The fake acts, loses its reply and forgets the key, and the sender restarts: one effect, and two with the lookup turned off. |

## Components

| Component        | Phase | Status  |
|------------------|-------|---------|
| Ledger           | 1     | done    |
| Payment lifecycle | 2    | done    |
| HTTP API, authentication and tenant isolation | 3 | done |
| Idempotency      | 3     | done |
| Mock PSP and webhooks | 4 | in progress |
| Outbox           | 5     | planned |
| Reconciliation   | 6     | planned |
