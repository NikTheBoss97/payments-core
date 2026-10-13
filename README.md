# payments-core

A payment processing backend where money is never lost, duplicated, or created,
even with retries, crashes, concurrent requests, and out-of-order events.

It is built around a double-entry ledger. Correctness under failure is the focus;
see [`docs/DESIGN.md`](docs/DESIGN.md) for goals, the failure model, and the
invariants, and [`docs/adr/`](docs/adr/) for the decisions behind them.

> All payment providers are mocked. No real card data is ever used.

## Status

| Phase | Scope | Status |
|-------|-------|--------|
| 1 | Double-entry ledger | in progress |
| 2 | Payment lifecycle state machine | — |
| 3 | Idempotency keys | — |
| 4 | Mock PSP, signed webhooks | — |
| 5 | Transactional outbox | — |
| 6 | Reconciliation | — |
| 7 | Failure-injection and property tests | — |
| 8 | Benchmarks | — |

## Quickstart

Requires Rust (stable) and Docker.

```sh
cp .env.example .env
docker compose up -d --wait
cargo test --workspace
```

## Layout

```
crates/payments-core/   core library and API server
docs/DESIGN.md          goals, failure model, invariants
docs/adr/               architecture decision records
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
