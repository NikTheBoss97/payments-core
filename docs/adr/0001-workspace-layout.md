# ADR-0001: Workspace layout

- Status: accepted
- Date: 2026-10-13

## Context

The system has one database whose transactions must cover several concerns at
once: ledger entries, payment state, idempotency records, and outbox events. It
also talks to an external PSP, which this repo mocks.

## Options

1. **Single crate** containing everything, with the mock PSP as a second binary.
   Simplest, but nothing stops the mock PSP from depending on core internals.
2. **One crate per area** (ledger, payments, outbox, ...). This looks modular, but
   the areas share one transaction boundary. `&mut Transaction` would have to be
   threaded across crates, adding boilerplate without real isolation.
3. **Cargo workspace with two crates**: `payments-core` (library plus API binary,
   with areas as modules) and `mock-psp` (a separate binary).

## Decision

Option 3. Crate boundaries follow the one real process and trust boundary: the
PSP is a different system reached only over HTTP and signed webhooks. Everything
that must commit atomically stays in one crate. `mock-psp` is added in phase 4,
not before.

## Consequences

- The core cannot accidentally depend on PSP internals, and vice versa.
- Areas inside `payments-core` are separated only by module privacy, which
  takes discipline in review.
