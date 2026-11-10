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
| 2 | Payment lifecycle state machine | done |
| 3 | Idempotency keys, HTTP API, authentication and tenant isolation | done |
| 4 | Mock PSP, signed webhooks | in progress |
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

`docker compose up -d --wait` also applies a changed `docker-compose.yml`: it
recreates the container, and the data stays in its volume.

### Running the server

The server never applies migrations ([ADR-0008](docs/adr/0008-database-role-provisioning.md)),
so that takes `sqlx-cli`, built from the sqlx commit that this project pins
([ADR-0006](docs/adr/0006-data-access-with-sqlx.md)):

```sh
cargo install sqlx-cli --git https://github.com/transact-rs/sqlx \
  --rev f1e94ec34dd5f11ad6488a3a8ac7988439a3f757 \
  --no-default-features --features postgres,rustls
sqlx migrate run --source crates/payments-core/migrations
```

The operator is one secret, of which the server holds only the digest, and the payment provider
signs its notifications with a key that the server holds too. Make both for this shell, and start
the server there:

```sh
set -a; . ./.env; set +a
operator="pck_$(openssl rand -hex 32)"
export OPERATOR_KEY_SHA256="$(printf %s "$operator" | openssl dgst -sha256 -r | cut -d' ' -f1)"
export MOCK_PSP_SIGNING_KEYS="whsec_$(openssl rand -base64 32)"
cargo run -p payments-core &
```

Once it logs `listening on 127.0.0.1:8080`, take a merchant on, and take a payment through its
life. Every `POST` carries an idempotency key, so any of these can be sent again and has one
effect. The full API is in [`docs/api.md`](docs/api.md).

```sh
post() { curl -s "localhost:8080$1" -H "Authorization: Bearer $2" -H "Idempotency-Key: \"$3\"" -d "$4"; }
member() { sed -E "s/.*\"$1\":\"([^\"]+)\".*/\1/"; }

merchant=$(post /merchants "$operator" merchant-1 '{"fee_bps":250}' | member merchant_id)
secret=$(post "/merchants/$merchant/api-keys" "$operator" key-1 '' | member secret)

payment=$(post /payments "$secret" create-1 '{"currency":"EUR","amount":10000}' | member payment_id)
post "/payments/$payment/authorize" "$secret" authorize-1 '{"amount":10000,"expires_at":"2030-01-01T00:00:00Z"}'; echo
post "/payments/$payment/capture" "$secret" capture-1 '{"amount":10000,"captured_at":"2027-01-01T00:00:00Z"}'; echo
post "/payments/$payment/refund" "$secret" refund-1 '{"amount":2500,"refunded_at":"2027-01-02T00:00:00Z"}'; echo

curl -s "localhost:8080/payments/$payment" -H "Authorization: Bearer $secret"; echo
```

This speaks plain HTTP on the local interface. In any real deployment TLS ends in front of the
server, because a secret sent in the clear is a stolen key.

## Layout

```
crates/payments-core/   core library and API server
crates/mock-psp/        the fake PSP that tests drive, and its binary for local runs
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
| I4 | One idempotency key produces at most one effect. | Guaranteed since phase 3 | [API](docs/correctness/api.md) |
| I5 | A payment never makes an invalid state transition. | Guaranteed since phase 2 | [Payments](docs/correctness/payments.md) |
| I6 | A payment's state change, its ledger entries and its outbound event commit atomically. | The state change and its entries since phase 2; the outbound event in phase 5 | [Payments](docs/correctness/payments.md) |
| I7 | A merchant can read or change only its own payments. | Guaranteed since phase 3 | [API](docs/correctness/api.md) |
| I8 | Only the PSP's word moves a payment: after its creation, every step but a cancellation comes from the PSP's history, once and in its order. | Phase 4, in progress: designed in [ADR-0034](docs/adr/0034-only-the-psps-word-moves-a-payment.md) | |
| I9 | A request sent to the PSP takes effect there at most once. | Phase 4, in progress: designed in [ADR-0028](docs/adr/0028-requests-to-the-psp.md) and [ADR-0034](docs/adr/0034-only-the-psps-word-moves-a-payment.md) | |

### Running the tests

```sh
cargo test --lib          # unit and property tests; no database needed
cargo test --workspace    # every test; needs the database from the quickstart
```

Each database test runs in its own freshly migrated database, created by
`#[sqlx::test]` and named after the test. Two runs of the same tests against one
server collide, so a second run at the same time takes a server of its own,
under another project name and on a free port, and removes it afterwards
([ADR-0040](docs/adr/0040-the-local-servers-limit-and-parallel-runs.md)):

```sh
POSTGRES_HOST_PORT=5434 docker compose -p payments-core-2 up -d --wait
DATABASE_URL=postgres://payments:payments@localhost:5434/payments cargo test --workspace
docker compose -p payments-core-2 down -v
```

A checkout can keep a server of its own instead: in its `.env`, set
`COMPOSE_PROJECT_NAME` and `POSTGRES_HOST_PORT`, and that port in the database
URLs.

CI runs on every pull request: formatting, `clippy` with warnings denied, the
whole suite, a release-mode test that integer overflow still panics
([ADR-0003](docs/adr/0003-money-representation.md)), and a check that the
committed sqlx query cache matches the queries.

Two more workflows look past the project's own behaviour
([ADR-0027](docs/adr/0027-supply-chain-checks.md)). `cargo-deny` fails on a
dependency with a known advisory, an unlisted licence or an unknown source, and
also runs daily, because an advisory can appear with no commit. CodeQL scans the
Rust code and the workflows. It does not report SQL injection in the form sqlx 0.9
requires, so `clippy` refuses SQL built at run time unless the call states why.

## Further reading

Eight first reads. The [full reading list](docs/reading/README.md) groups
163 references by topic, and ★ marks the best first reads in each.

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
