<!-- Author: Nicola Antonello -->

# Further reading

The theory behind each part of the design, grouped by topic. ★ marks the best
first reads. Within each topic, starred entries come first, then living
documentation, then papers, books and posts from the newest to the oldest. An
older classic is listed as the origin of an idea the project uses, next to a
recent treatment of it. Each reference was checked against the original
publication: its title, authors, year and link.

| Topic | What it covers | Entries |
|---|---|---|
| [Ledgers and accounting](ledgers-and-accounting.md) | Double-entry bookkeeping, ledgers as immutable logs, and recording time. | 20 |
| [Money and identifiers](money-and-identifiers.md) | Representing amounts and currencies exactly, splitting them without losing a unit, and generating identifiers. | 10 |
| [Schema design and database errors](schema-and-errors.md) | Constraints, privileges and operator resolution in PostgreSQL, NULLs and keys, and error codes that programs can act on. | 13 |
| [Transactions and concurrency](transactions-and-concurrency.md) | Isolation levels, locking, coordination and what a database transaction guarantees. | 11 |
| [Idempotency and messaging](idempotency-and-messaging.md) | Idempotency keys, retries, transactional messaging and webhooks. | 12 |
| [Payments lifecycle](payments-lifecycle.md) | Payment statuses and state machines, authorization and settlement, compensation and reconciliation. | 11 |
| [Testing for correctness](testing.md) | Property-based testing, mutation testing, failure injection and testing database guarantees. | 10 |
| [Modelling the domain in Rust](rust.md) | Types that carry their invariants, and making illegal states unrepresentable. | 7 |
| [Benchmarking](benchmarking.md) | Measuring latency and throughput without fooling yourself. | 8 |
| [Engineering practice](engineering-practice.md) | Continuous integration and recording decisions. | 7 |
