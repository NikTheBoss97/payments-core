<!-- Author: Nicola Antonello -->

# Ledgers and accounting

Double-entry bookkeeping, ledgers as immutable logs, and recording time. Part of the [reading list](README.md).

## Double-entry accounting and ledgers

- ★ Martin Kleppmann, [Accounting for Computer Scientists](https://martin.kleppmann.com/2011/03/07/accounting-for-computer-scientists.html) (2011). Double-entry bookkeeping as a graph: accounts are nodes, transactions are edges, and all balances sum to zero.
- Lucas Rocha, [Accounting for Developers, Part I: The Fundamentals](https://www.moderntreasury.com/journal/accounting-for-developers-part-i) (Modern Treasury, 2022). Accounts, entries, debit-normal and credit-normal accounts, and how to check that a ledger balances.
- Martin Blais, [The Double-Entry Counting Method](https://beancount.github.io/docs/the_double_entry_counting_method/) (Beancount, 2016). Signed amounts instead of debit and credit columns: the postings of a transaction sum to zero.
- Martin Fowler, [Patterns for Accounting](https://martinfowler.com/eaaDev/AccountingNarrative.html) (draft, 2006). A balance as the sum of entries, and correcting mistakes with reversing entries instead of edits.
- Ilya Ganelin, [Ledger: Stripe's system for tracking and validating money movement](https://stripe.dev/blog/ledger-stripe-system-for-tracking-and-validating-money-movement) (Stripe, 2024). An immutable event log, fund flows as state machines, and clearing accounts that must drain to zero.
- Matt McNierney, [How to Scale a Ledger, Part VI: Concurrency Controls, Performance, and More](https://www.moderntreasury.com/journal/how-to-scale-a-ledger-part-vi) (Modern Treasury, 2023). The alternative to deriving balances on read: cached balances, balance locks and drift checks.
- Mitchell Franklin, Patty Graybeal and Dixon Cooper, [Principles of Accounting, Volume 1: Financial Accounting](https://openstax.org/details/books/principles-financial-accounting) (OpenStax, 2019). Normal balances and the chart of accounts (chapter 3), and subsidiary ledgers that must agree with their control account (section 7.2).
- Łukasz Strzałkowski, [Books, an immutable double-entry accounting database service](https://developer.squareup.com/blog/books-an-immutable-double-entry-accounting-database-service/) (Square, 2019). Square's ledger: journal entries that must balance to zero, in append-only tables. A useful contrast: it also caches balances, updated on every operation.
- TigerBeetle, [Data Modeling](https://docs.tigerbeetle.com/coding/data-modeling/). How a ledger database groups accounts by currency (`ledger`) and type (`code`), and points at business records kept elsewhere through `user_data`.

## Immutability, logs and derived data

- ★ Pat Helland, [Immutability Changes Everything](https://www.cidrdb.org/cidr2015/Papers/CIDR15_Paper16.pdf) (CIDR 2015). "The truth is the log": a bank balance is a fact derived from immutable records.
- ★ Martin Kleppmann and Chris Riccomini, [Designing Data-Intensive Applications, 2nd edition](https://martin.kleppmann.com/2026/03/24/designing-data-intensive-applications-2e.html) (O'Reilly, 2026). Transactions and isolation levels (chapter 8), derived data, event sourcing and ID generation.
- Martin Fowler, [Event Sourcing](https://martinfowler.com/eaaDev/EventSourcing.html) (2005). Store every change as an event, and rebuild state by replaying the events.

## Time

- ★ Martin Fowler, [Bitemporal History](https://martinfowler.com/articles/bitemporal-history.html) (2021). Actual history versus record history: when something happened versus when we learned of it, and why the record is append-only.
- Krishna Kulkarni and Jan-Eike Michels, [Temporal features in SQL:2011](https://doi.org/10.1145/2380776.2380786) (SIGMOD Record, 2012). Application-time periods that users set, and system-versioned tables whose times the database assigns, from a timestamp fixed for the whole transaction.
- PostgreSQL, [9.9.5. Current Date/Time](https://www.postgresql.org/docs/18/functions-datetime.html#FUNCTIONS-DATETIME-CURRENT) (PostgreSQL 18 docs). `now()` is the start of the current transaction, not the moment of the call or of the commit; `statement_timestamp()` and `clock_timestamp()` move.
- TigerBeetle, [Time](https://docs.tigerbeetle.com/coding/time/). Why the ledger cluster assigns every timestamp itself: they all come from one clock, unique and totally ordered.
