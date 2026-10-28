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
| [Ledgers and accounting](ledgers-and-accounting.md) | Double-entry bookkeeping, ledgers as immutable logs, and recording time. | 16 |
| [Money and identifiers](money-and-identifiers.md) | Representing amounts and currencies exactly, and generating identifiers. | 6 |
| [Schema design and database errors](schema-and-errors.md) | Constraints, privileges and operator resolution in PostgreSQL, and error codes that programs can act on. | 9 |
| [Transactions and concurrency](transactions-and-concurrency.md) | Isolation levels, locking, coordination and what a database transaction guarantees. | 7 |
| [Idempotency and messaging](idempotency-and-messaging.md) | Idempotency keys, retries, transactional messaging and webhooks. | 8 |
| [Payments lifecycle](payments-lifecycle.md) | Payment statuses, authorization and capture, compensation and reconciliation. | 5 |
| [Testing for correctness](testing.md) | Property-based testing, failure injection and testing database guarantees. | 6 |
| [Modelling the domain in Rust](rust.md) | Types that carry their invariants, and making illegal states unrepresentable. | 3 |
| [Benchmarking](benchmarking.md) | Measuring latency and throughput without fooling yourself. | 5 |
| [Engineering practice](engineering-practice.md) | Continuous integration and recording decisions. | 3 |
