<!-- Author: Nicola Antonello -->

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
| 1 | Double-entry ledger | done |
| 2 | Payment lifecycle state machine | — |
| 3 | Idempotency keys | — |
| 4 | Mock PSP, signed webhooks | — |
| 5 | Transactional outbox | — |
| 6 | Reconciliation | — |
| 7 | Failure-injection and property tests | — |
| 8 | Benchmarks | — |

## Quickstart

Built with Rust, tokio, axum, sqlx and PostgreSQL 18. Requires Rust (stable)
and Docker.

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
docs/correctness/       how each component's invariants are tested
docs/reading/           further reading, by topic
```

## Correctness

The invariants of [`docs/DESIGN.md`](docs/DESIGN.md#invariants), and where
their tests are described. Each page names the tests that try to break every
guard, and the failures they reproduce.

| | Invariant | Status | Tests |
|---|---|---|---|
| I1 | Every ledger transaction's entries sum to zero, so each currency sums to zero across the ledger. | Guaranteed since phase 1 | [Ledger](docs/correctness/ledger.md) |
| I2 | The ledger's tables are append-only. | Guaranteed since phase 1 | [Ledger](docs/correctness/ledger.md) |
| I3 | Balances are derived from the entries: none is stored. | Guaranteed since phase 1 | [Ledger](docs/correctness/ledger.md) |
| I4 | One idempotency key produces at most one effect. | Phase 3 | |
| I5 | A payment never makes an invalid state transition. | Phase 2 | |
| I6 | A payment's state change, its ledger entries and its outbound event commit atomically. | Phases 2 and 5 | |

### Running the tests

```sh
cargo test --lib          # unit and property tests; no database needed
cargo test --workspace    # every test; needs the database from the quickstart
```

Each database test runs in its own freshly migrated database, created by
`#[sqlx::test]`. CI runs on every pull request: formatting, `clippy` with
warnings denied, the whole suite, a release-mode test that integer overflow
still panics ([ADR-0003](docs/adr/0003-money-representation.md)), and a check
that the committed sqlx query cache matches the queries.

## Further reading

Eight first reads. The [full reading list](docs/reading/README.md) groups
109 references by topic, and ★ marks the best first reads in each.

- ★ Martin Kleppmann, [Accounting for Computer Scientists](https://martin.kleppmann.com/2011/03/07/accounting-for-computer-scientists.html) (2011). Double-entry bookkeeping as a graph: accounts are nodes, transactions are edges, and all balances sum to zero.
- ★ Pat Helland, [Immutability Changes Everything](https://www.cidrdb.org/cidr2015/Papers/CIDR15_Paper16.pdf) (CIDR 2015). "The truth is the log": a bank balance is a fact derived from immutable records.
- ★ Martin Kleppmann and Chris Riccomini, [Designing Data-Intensive Applications, 2nd edition](https://martin.kleppmann.com/2026/03/24/designing-data-intensive-applications-2e.html) (O'Reilly, 2026). Transactions and isolation levels (chapter 8), derived data, event sourcing and ID generation.
- ★ PostgreSQL, [Chapter 13: Concurrency Control](https://www.postgresql.org/docs/18/mvcc.html) (PostgreSQL 18 docs). What each isolation level guarantees, row and advisory locks, and serialization failures.
- ★ Brandur Leach, [Implementing Stripe-like Idempotency Keys in Postgres](https://brandur.org/idempotency-keys) (2017). A complete Postgres design for idempotency keys, including recovery points around calls to an external API.
- ★ Adyen, [Payments lifecycle](https://docs.adyen.com/account/payments-lifecycle). Real payment statuses, including why a payment can no longer be cancelled once it is sent for settlement.
- ★ Ding Yuan et al., [Simple Testing Can Prevent Most Critical Failures](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/yuan) (OSDI, 2014). Most catastrophic failures studied came from mishandled non-fatal errors, so failure injection should target error paths.
- ★ Alexis King, [Parse, don't validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/) (2019). Parse input once into types that carry their invariants, such as a transaction that cannot be unbalanced.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
