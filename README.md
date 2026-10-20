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
| 1 | Double-entry ledger | in progress |
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
```

## Further reading

The theory behind each part of the design, grouped by topic. ★ marks the best
first reads.

### Double-entry accounting and ledgers

- ★ Martin Kleppmann, [Accounting for Computer Scientists](https://martin.kleppmann.com/2011/03/07/accounting-for-computer-scientists.html) (2011). Double-entry bookkeeping as a graph: accounts are nodes, transactions are edges, and all balances sum to zero.
- Lucas Rocha, [Accounting for Developers, Part I: The Fundamentals](https://www.moderntreasury.com/journal/accounting-for-developers-part-i) (Modern Treasury, 2022). Accounts, entries, debit-normal and credit-normal accounts, and how to check that a ledger balances.
- Martin Blais, [The Double-Entry Counting Method](https://beancount.github.io/docs/the_double_entry_counting_method/) (Beancount, 2016). Signed amounts instead of debit and credit columns: the postings of a transaction sum to zero.
- Martin Fowler, [Patterns for Accounting](https://martinfowler.com/eaaDev/AccountingNarrative.html) (draft, 2006). A balance as the sum of entries, and correcting mistakes with reversing entries instead of edits.
- Ilya Ganelin, [Ledger: Stripe's system for tracking and validating money movement](https://stripe.dev/blog/ledger-stripe-system-for-tracking-and-validating-money-movement) (Stripe, 2024). An immutable event log, fund flows as state machines, and clearing accounts that must drain to zero.
- Matt McNierney, [How to Scale a Ledger, Part VI: Concurrency Controls, Performance, and More](https://www.moderntreasury.com/journal/how-to-scale-a-ledger-part-vi) (Modern Treasury, 2023). The alternative to deriving balances on read: cached balances, balance locks and drift checks.
- Mitchell Franklin, Patty Graybeal and Dixon Cooper, [Principles of Accounting, Volume 1: Financial Accounting](https://openstax.org/details/books/principles-financial-accounting) (OpenStax, 2019). Normal balances and the chart of accounts (chapter 3), and subsidiary ledgers that must agree with their control account (section 7.2).
- Łukasz Strzałkowski, [Books, an immutable double-entry accounting database service](https://developer.squareup.com/blog/books-an-immutable-double-entry-accounting-database-service/) (Square, 2019). Square's ledger: journal entries that must balance to zero, in append-only tables. A useful contrast: it also caches balances, updated on every operation.
- TigerBeetle, [Data Modeling](https://docs.tigerbeetle.com/coding/data-modeling/). How a ledger database groups accounts by currency (`ledger`) and type (`code`), and points at business records kept elsewhere through `user_data`.

### Immutability, logs and derived data

- ★ Pat Helland, [Immutability Changes Everything](https://www.cidrdb.org/cidr2015/Papers/CIDR15_Paper16.pdf) (CIDR 2015). "The truth is the log": a bank balance is a fact derived from immutable records.
- ★ Martin Kleppmann and Chris Riccomini, [Designing Data-Intensive Applications, 2nd edition](https://martin.kleppmann.com/2026/03/24/designing-data-intensive-applications-2e.html) (O'Reilly, 2026). Transactions and isolation levels (chapter 8), derived data, event sourcing and ID generation.
- Martin Fowler, [Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html) (2005). Store every change as an event, and rebuild state by replaying the events.

### Time

- ★ Martin Fowler, [Bitemporal History](https://martinfowler.com/articles/bitemporal-history.html) (2021). Actual history versus record history: when something happened versus when we learned of it, and why the record is append-only.
- Krishna Kulkarni and Jan-Eike Michels, [Temporal features in SQL:2011](https://doi.org/10.1145/2380776.2380786) (SIGMOD Record, 2012). Application-time periods that users set, and system-versioned tables whose times the database assigns, from a timestamp fixed for the whole transaction.
- PostgreSQL, [9.9.5. Current Date/Time](https://www.postgresql.org/docs/18/functions-datetime.html#FUNCTIONS-DATETIME-CURRENT) (PostgreSQL 18 docs). `now()` is the start of the current transaction, not the moment of the call or of the commit; `statement_timestamp()` and `clock_timestamp()` move.
- TigerBeetle, [Time](https://docs.tigerbeetle.com/coding/time/). Why the ledger cluster assigns every timestamp itself: they all come from one clock, unique and totally ordered.

### Money and identifiers

- ★ Martin Fowler, [Money](https://martinfowler.com/eaaCatalog/money.html) (Patterns of Enterprise Application Architecture, 2002). Keep an amount with its currency, and watch for lost pennies when rounding to minor units.
- [ISO 4217 currency codes](https://www.iso.org/iso-4217-currency-codes.html) (ISO). The three-letter codes, and how many minor units each currency has (not always two).
- Stripe, [Supported currencies](https://docs.stripe.com/currencies). How a real PSP sends amounts as integer minor units, and where it departs from ISO 4217.
- PostgreSQL wiki, [Don't use money](https://wiki.postgresql.org/wiki/Don%27t_Do_This#Don.27t_use_money). Why the `money` type is a poor fit: it stores no currency, takes it from the locale, and rounds in surprising ways.
- David Goldberg, [What Every Computer Scientist Should Know About Floating-Point Arithmetic](https://doi.org/10.1145/103162.103163) (ACM Computing Surveys, 1991). Why decimal fractions such as 0.1 have no exact binary representation.
- K. Davis, B. Peabody and P. Leach, [RFC 9562: Universally Unique IDentifiers (UUIDs)](https://www.rfc-editor.org/rfc/rfc9562.html) (IETF, 2024). Defines UUIDv7, and explains why random UUIDs hurt B-tree index locality.

### Transactions, isolation and coordination

- ★ PostgreSQL, [Chapter 13: Concurrency Control](https://www.postgresql.org/docs/18/mvcc.html) (PostgreSQL 18 docs). What each isolation level guarantees, row and advisory locks, and serialization failures.
- Hal Berenson et al., [A Critique of ANSI SQL Isolation Levels](https://doi.org/10.1145/223784.223785) (SIGMOD, 1995). Precise definitions of lost updates, write skew and snapshot isolation.
- Dan R. K. Ports and Kevin Grittner, [Serializable Snapshot Isolation in PostgreSQL](https://vldb.org/pvldb/vol5/p1850_danrkports_vldb2012.pdf) (PVLDB, 2012). How PostgreSQL implements SERIALIZABLE, and what it costs compared with snapshot isolation.
- Peter Bailis et al., [Coordination Avoidance in Database Systems](https://www.vldb.org/pvldb/vol8/p185-bailis.pdf) (PVLDB, 2014). Which invariants can be kept without coordination: a foreign key under inserts can, a non-negative balance under withdrawals cannot.
- Joseph M. Hellerstein and Peter Alvaro, [Keeping CALM: When Distributed Consistency Is Easy](https://doi.org/10.1145/3369736) (Communications of the ACM, 2020). Explains the CALM theorem: monotonic programs need no coordination.
- Todd Warszawski and Peter Bailis, [ACIDRain: Concurrency-Related Attacks on Database-Backed Web Applications](https://doi.org/10.1145/3035918.3064037) (SIGMOD, 2017). How concurrent requests corrupt inventory and overspend gift cards in real e-commerce applications.
- PostgreSQL, [54.2. Message Flow](https://www.postgresql.org/docs/18/protocol-flow.html) (PostgreSQL 18 docs). The replies to a query: `CommandComplete` after each command, `ReadyForQuery` at the end, errors in between, and why a cancel request may or may not take effect. The background for telling a failed `COMMIT` from a lost one.

### Schema design and database errors

- ★ PostgreSQL, [5.5. Constraints](https://www.postgresql.org/docs/18/ddl-constraints.html) (PostgreSQL 18 docs). What each kind of constraint can check, why a foreign key must target a unique key (which lets a child row repeat its parent's values), and how NULLs slip past unique and check constraints.
- PostgreSQL, [5.8. Privileges](https://www.postgresql.org/docs/18/ddl-priv.html) (PostgreSQL 18 docs). Privileges granted per column, which leave a column such as a recording time to its default.
- PostgreSQL, [10.2. Operators](https://www.postgresql.org/docs/18/typeconv-oper.html) (PostgreSQL 18 docs). How PostgreSQL picks an operator: one that takes exactly the argument types wins wherever it sits on the search path, so a guard function compares only types for which `pg_catalog` has an exact operator.
- PostgreSQL wiki, [A Guide to CVE-2018-1058: Protect Your Search Path](https://wiki.postgresql.org/wiki/A_Guide_to_CVE-2018-1058:_Protect_Your_Search_Path). How a function created in `public` takes over calls that a built-in would answer through a cast, and the defences: no `CREATE` on `public`, and a narrower `search_path`.
- Bill Karwin, [SQL Antipatterns, Volume 1: Avoiding the Pitfalls of Database Programming](https://pragprog.com/titles/bksap1/sql-antipatterns-volume-1/) (Pragmatic Bookshelf, 2022). Includes Polymorphic Associations: why no foreign key can check a `(type, id)` pair, and what to do instead.
- Craig Kerstiens, [Enums vs Check Constraints in Postgres](https://www.crunchydata.com/blog/enums-vs-check-constraints-in-postgres) (Crunchy Data, 2022). A short case for a `CHECK` constraint over an enum type for a closed set of values.
- PostgreSQL, [Appendix A. PostgreSQL Error Codes](https://www.postgresql.org/docs/18/errcodes-appendix.html) (PostgreSQL 18 docs). Five-character SQLSTATEs, whose first two characters name a class that a program can act on without knowing the exact code.
- PostgreSQL, [41.9. Errors and Messages](https://www.postgresql.org/docs/18/plpgsql-errors-and-messages.html) (PostgreSQL 18 docs). Raising errors with a custom SQLSTATE from PL/pgSQL, and why codes ending in 000 are categories.
- Google, [AIP-193: Errors](https://google.aip.dev/193) (API Improvement Proposals). Messages are for people and machine-readable reasons are for programs, so clients never need to parse a message.

### Idempotency, messaging and webhooks

- ★ Brandur Leach, [Implementing Stripe-like Idempotency Keys in Postgres](https://brandur.org/idempotency-keys) (2017). A complete Postgres design for idempotency keys, including recovery points around calls to an external API.
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests) (API reference). A production contract: saved responses, an error when a key is reused with different parameters, and key expiry.
- Pat Helland, [Idempotence Is Not a Medical Condition](https://doi.org/10.1145/2181796.2187821) (ACM Queue, 2012). Why messages between systems get retried, and why idempotence makes that safe.
- Pat Helland, [Life beyond Distributed Transactions: an Apostate's Opinion](https://www.cidrdb.org/cidr2007/papers/cidr07p15.pdf) (CIDR 2007). Commit each entity on its own, enqueue messages in the same transaction, and process them idempotently.
- Chris Richardson, [Microservices Patterns](https://www.manning.com/books/microservices-patterns) (Manning, 2018). Chapter 3 covers the transactional outbox and its relays; chapter 4 covers sagas.
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0, 2023). Signed webhooks: message IDs for deduplication, timestamps against replay, and retries with backoff.
- Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks). Real delivery semantics: retries for up to three days, duplicates, and no ordering guarantee.
- Marc Brooker, [Timeouts, retries, and backoff with jitter](https://builder.aws.com/content/3EumjoZascWd1oZiEgL8ORlv3qE/timeouts-retries-and-backoff-with-jitter) (Amazon Builders' Library, 2019). How retries amplify load, and why backoff needs jitter.

### Payment lifecycle, compensation and reconciliation

- ★ Adyen, [Payments lifecycle](https://docs.adyen.com/account/payments-lifecycle). Real payment statuses, including why a payment can no longer be cancelled once it is sent for settlement.
- Stripe, [Place a hold on a payment method](https://docs.stripe.com/payments/place-a-hold-on-a-payment-method). Separate authorization and capture, authorization expiry, and partial capture.
- TigerBeetle, [Two-Phase Transfers](https://docs.tigerbeetle.com/coding/two-phase-transfers/). Holds in a ledger: pending transfers that are later posted, voided or expired.
- Gregor Hohpe, [Your Coffee Shop Doesn't Use Two-Phase Commit](https://www.enterpriseintegrationpatterns.com/docs/IEEE_Software_Design_2PC.pdf) (IEEE Software, 2005). Error handling without distributed transactions: write-off, retry, compensation and reconciliation.
- Hector Garcia-Molina and Kenneth Salem, [Sagas](https://doi.org/10.1145/38713.38742) (SIGMOD, 1987). Long-lived work as a sequence of transactions, with compensating transactions to undo a partial run.

### Testing for correctness

- ★ Ding Yuan et al., [Simple Testing Can Prevent Most Critical Failures](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/yuan) (OSDI, 2014). Most catastrophic failures studied came from mishandled non-fatal errors, so failure injection should target error paths.
- Koen Claessen and John Hughes, [QuickCheck: A Lightweight Tool for Random Testing of Haskell Programs](https://doi.org/10.1145/351240.351266) (ICFP, 2000). The paper that introduced QuickCheck, from which modern property-based testing descends.
- John Hughes, [Experiences with QuickCheck: Testing the Hard Stuff and Staying Sane](https://publications.lib.chalmers.se/records/fulltext/232550/local_232550.pdf) (2016). Testing stateful systems against a model of their state transitions, and finding race conditions.
- Jason Lingle, Mazdak Farrokhzad and contributors, [Proptest](https://proptest-rs.github.io/proptest/) (the proptest book). Property-based testing in Rust: strategies, shrinking, failure persistence and state-machine tests.
- Jingyu Zhou et al., [FoundationDB: A Distributed Unbundled Transactional Key Value Store](https://doi.org/10.1145/3448016.3457559) (SIGMOD, 2021). Section 4 describes deterministic simulation, which makes every injected failure reproducible.
- Kyle Kingsbury, [Jepsen: PostgreSQL 12.3](https://jepsen.io/analyses/postgresql-12.3) (2020). Testing isolation guarantees instead of assuming them; the SERIALIZABLE bug it found was fixed in 12.4.

### Modelling the domain in Rust

- ★ Alexis King, [Parse, don't validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/) (2019). Parse input once into types that carry their invariants, such as a transaction that cannot be unbalanced.
- Yaron Minsky, [Effective ML Revisited](https://blog.janestreet.com/effective-ml-revisited/) (Jane Street, 2011). Illustrates "make illegal states unrepresentable" with a before-and-after example.
- Cliff L. Biffle, [The Typestate Pattern in Rust](https://cliffle.com/blog/rust-typestate/) (2019). Encoding an object's state in its type, so that calls in the wrong state do not compile.

### Benchmarking

- ★ Gil Tene, [How NOT to Measure Latency](https://www.infoq.com/presentations/latency-response-time/) (QCon San Francisco, 2015). Coordinated omission, and why percentiles cannot be averaged.
- Bianca Schroeder, Adam Wierman and Mor Harchol-Balter, [Open Versus Closed: A Cautionary Tale](https://www.usenix.org/conference/nsdi-06/open-versus-closed-cautionary-tale) (NSDI, 2006). Closed-loop and open-loop load generators report different response times at the same load.
- Jeffrey Dean and Luiz André Barroso, [The Tail at Scale](https://doi.org/10.1145/2408776.2408794) (Communications of the ACM, 2013). Why tail latency matters at scale, and techniques that tolerate latency variability.
- Brendan Gregg, [Thinking Methodically about Performance](https://doi.org/10.1145/2405116.2413037) (ACM Queue, 2012). The USE method: check utilization, saturation and errors for every resource.
- Gernot Heiser, [Systems Benchmarking Crimes](https://gernot-heiser.org/benchmarking-crimes.html). A checklist of misleading benchmarking practices to avoid.

### Recording decisions

- Michael Nygard, [Documenting Architecture Decisions](https://www.cognitect.com/blog/2011/11/15/documenting-architecture-decisions) (Cognitect, 2011). The original ADR format: context, decision, status and consequences, with a reversed decision kept and marked superseded.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
