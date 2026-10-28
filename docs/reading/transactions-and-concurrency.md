<!-- Author: Nicola Antonello -->

# Transactions and concurrency

Isolation levels, locking, coordination and what a database transaction guarantees. Part of the [reading list](README.md).

- ★ PostgreSQL, [Chapter 13: Concurrency Control](https://www.postgresql.org/docs/18/mvcc.html) (PostgreSQL 18 docs). What each isolation level guarantees, row and advisory locks, and serialization failures.
- PostgreSQL, [54.2. Message Flow](https://www.postgresql.org/docs/18/protocol-flow.html) (PostgreSQL 18 docs). The replies to a query: `CommandComplete` after each command, `ReadyForQuery` at the end, errors in between, and why a cancel request may or may not take effect. The background for telling a failed `COMMIT` from a lost one.
- Joseph M. Hellerstein and Peter Alvaro, [Keeping CALM: When Distributed Consistency Is Easy](https://doi.org/10.1145/3369736) (Communications of the ACM, 2020). Explains the CALM theorem: monotonic programs need no coordination.
- Todd Warszawski and Peter Bailis, [ACIDRain: Concurrency-Related Attacks on Database-Backed Web Applications](https://doi.org/10.1145/3035918.3064037) (SIGMOD, 2017). How concurrent requests corrupt inventory and overspend gift cards in real e-commerce applications.
- Peter Bailis et al., [Coordination Avoidance in Database Systems](https://www.vldb.org/pvldb/vol8/p185-bailis.pdf) (PVLDB, 2014). Which invariants can be kept without coordination: a foreign key under inserts can, a non-negative balance under withdrawals cannot.
- Dan R. K. Ports and Kevin Grittner, [Serializable Snapshot Isolation in PostgreSQL](https://vldb.org/pvldb/vol5/p1850_danrkports_vldb2012.pdf) (PVLDB, 2012). How PostgreSQL implements SERIALIZABLE, and what it costs compared with snapshot isolation.
- Hal Berenson et al., [A Critique of ANSI SQL Isolation Levels](https://doi.org/10.1145/223784.223785) (SIGMOD, 1995). Precise definitions of lost updates, write skew and snapshot isolation.
