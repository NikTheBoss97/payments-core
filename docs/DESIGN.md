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

## Invariants

| ID | Invariant | Enforced by | Tested by |
|----|-----------|-------------|-----------|
| I1 | Every ledger transaction's entries sum to zero per currency, so the whole ledger sums to zero. | Domain type that cannot be built unbalanced, plus a database-level check at commit. | Unit tests, property tests, database tests that bypass the domain layer. |
| I2 | Ledger entries are append-only. Corrections are new, reversing transactions. | Database: no `UPDATE`/`DELETE` on ledger tables. | Database tests that attempt mutation. |
| I3 | No balance is read from a mutable column that can drift from the entries. Balances are derived from entries. | Schema design (see ledger ADR). | Property test: derived balance equals the replayed sum of entries. |
| I4 | One idempotency key produces at most one effect. | *Phase 3* | Concurrent-retry tests. |
| I5 | A payment never makes an invalid state transition. | *Phase 2* | Exhaustive transition-table tests and property tests. |
| I6 | A payment state change, its ledger entries, and its outbound event commit atomically. | *Phases 2 and 5*: single database transaction plus transactional outbox. | Crash-injection tests. |

## Components

| Component        | Phase | Status  |
|------------------|-------|---------|
| Ledger           | 1     | planned |
| Payment lifecycle | 2    | planned |
| Idempotency      | 3     | planned |
| Mock PSP and webhooks | 4 | planned |
| Outbox           | 5     | planned |
| Reconciliation   | 6     | planned |
