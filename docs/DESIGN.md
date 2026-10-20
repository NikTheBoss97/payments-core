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
- **Authentication and multi-tenancy.** At most a static API key.
- **Horizontal database scaling.** One PostgreSQL primary. Sharding and multi-region are out of scope.
- **A UI.**

## Failure model

| Assumed to happen                                    | Assumed not to happen                        |
|------------------------------------------------------|----------------------------------------------|
| Process crash at any instruction                     | Loss of committed PostgreSQL data            |
| Client timeouts and retries with the same key        | Clients reusing a key for a different request (detected and rejected, not merged) |
| Concurrent requests on the same payment or account   | Clock agreement between services             |
| Webhooks duplicated, delayed, reordered, or forged   | A PSP that sends a validly signed but false event |
| PSP timeouts with unknown outcome                    |                                              |
| Application bugs and manual SQL run as the runtime role, including code that bypasses the domain layer | A superuser or the migration role (the tables' owner) deliberately disabling the ledger's triggers or foreign-key checks |
| Lost `COMMIT` acknowledgements (the service cannot tell whether PostgreSQL committed) |              |

## Invariants

| ID | Invariant | Enforced by | Tested by |
|----|-----------|-------------|-----------|
| I1 | Every ledger transaction's entries sum to zero per currency, so the whole ledger sums to zero in each currency. | Domain type that cannot be built unbalanced, plus a commit-time check of each sealed ledger transaction in PostgreSQL ([ADR-0004](adr/0004-enforcing-ledger-invariants.md)); a foreign key that makes every entry repeat its ledger transaction's currency ([ADR-0010](adr/0010-currency-on-the-ledger-header.md)); `NOT NULL` ledger columns, so no entry escapes those checks ([ADR-0003](adr/0003-money-representation.md)). | Unit and property tests of the domain type; database tests, as the runtime role, that write unbalanced, incomplete, mixed-currency or appended ledger transactions with raw SQL; a catalog test that pins every ledger column's type and `NOT NULL`; a single-statement trial balance (every currency sums to zero across the ledger). |
| I2 | Ledger entries are append-only. Corrections are new, reversing transactions. | A trigger rejecting `UPDATE`, `DELETE` and `TRUNCATE`, and a runtime role without those privileges ([ADR-0004](adr/0004-enforcing-ledger-invariants.md)). | Database tests that attempt `UPDATE`, `DELETE` and `TRUNCATE` as the runtime role (expecting a permission error) and as the owner role (expecting the trigger's error), so each guard is tested on its own; catalog tests that pin the runtime role's privileges and grant options, check that it can neither create objects in `public` nor act through another role, and pin every guard trigger, its `ENABLE ALWAYS` and its function's `search_path`. |
| I3 | No balance is read from a mutable column that can drift from the entries. Balances are derived from entries. | No balance is stored anywhere ([ADR-0002](adr/0002-balances-derived-from-entries.md)). | Holds by construction. Tests check derived balances against an independent in-memory model under concurrent writes. |
| I4 | One idempotency key produces at most one effect. | *Phase 3* | Concurrent-retry tests. |
| I5 | A payment never makes an invalid state transition. | *Phase 2* | Exhaustive transition-table tests and property tests. |
| I6 | A payment state change, its ledger entries, and its outbound event commit atomically. | *Phases 2 and 5*: single database transaction plus transactional outbox. | Crash-injection tests. |

## Components

| Component        | Phase | Status  |
|------------------|-------|---------|
| Ledger           | 1     | in progress |
| Payment lifecycle | 2    | planned |
| Idempotency      | 3     | planned |
| Mock PSP and webhooks | 4 | planned |
| Outbox           | 5     | planned |
| Reconciliation   | 6     | planned |
