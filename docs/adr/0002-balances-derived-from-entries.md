<!-- Author: Nicola Antonello -->

# ADR-0002: Balances are derived from ledger entries

- Status: accepted; partly superseded by ADR-0018
- Date: 2026-10-14

## Context

Ledger entries are the system of record, and an account's balance is a
function of them: the sum of the account's entry amounts. The question is
whether to compute that sum when a balance is read, or to store it and keep
the stored copy in step with the entries.

Three forces pull against each other:

- **Drift.** A stored balance repeats information the entries already hold.
  Any write path that updates one without the other (a bug, a manual
  `UPDATE`, a backfill, a restore) makes them disagree, and the entries win
  by definition. Invariant I3 rules out balances that can drift.
- **Contention.** A stored balance is one row that every writer to the
  account must update. Every correct way to do that (an atomic
  `balance = balance + x`, `SELECT ... FOR UPDATE`, a version check,
  `SERIALIZABLE`) makes writers queue on that row until they commit. A fee or
  clearing account touched by every payment would put all payments in one
  queue. Appending entries writes only new rows, and addition is commutative,
  so concurrent appends never need to be ordered. In the terms of Bailis et
  al., the zero-sum rule (I1) is invariant-confluent under inserts: two
  transactions that each keep it still keep it when both commit, so they
  need no coordination. A lower bound on a balance is not: if an account
  holds 100, two concurrent withdrawals of 80 each see 100, each pass the
  check, and together leave -60.
- **Read cost.** A sum costs time proportional to the account's history; a
  stored value is read in constant time.

## Options

| Option | Why not |
|---|---|
| **A. Sum the entries when a balance is read** | Chosen |
| B. Running balance and per-account sequence on each entry | Serialises every writer to an account, so a shared fee account becomes a queue |
| C. Cached balance column on `accounts` | A mutable copy that can drift (violates I3), with the same queue |
| D. Cached balance kept up to date by a trigger | Still a mutable copy, and the queue is hidden inside every insert |
| E. Materialized view refreshed on a schedule | Stale between refreshes, so unusable for live balances |
| F. Immutable snapshots plus the sum of later entries | Needs a cut-off that no later commit can fall behind: timestamps and IDs cannot give one, a watermark on PostgreSQL's own transaction IDs can (see ADR-0005). Premature without measurements |
| G. Balances projected asynchronously by a worker | Lags, so it cannot decide whether money may move |
| H. Single-writer engine: TigerBeetle (a ledger database) or an LMAX-style in-memory processor | Moves the ledger out of PostgreSQL, which breaks the atomic commit of payment, ledger and outbox (I6) |
| I. A mix of A and B, chosen per account | Only needed once the ledger itself must enforce balance limits |

## Decision

Option A: no balance is stored anywhere.

- A balance is read with a single statement:
  `SELECT COALESCE(SUM(amount), 0)::bigint AS "balance!" FROM ledger_entries WHERE account_id = $1`.
  One statement reads one snapshot, even at READ COMMITTED, so the sum sees
  each ledger transaction's entries all or not at all, even one that commits
  while the query runs. `SUM` over no rows is NULL, hence `COALESCE`.
  `SUM(bigint)` returns `numeric`, and the cast back to `bigint` raises an
  error instead of wrapping on overflow. The `"balance!"` alias tells sqlx
  that the column is never NULL (ADR-0006).
- An index on `(account_id) INCLUDE (amount)` holds every column the query
  needs, so PostgreSQL can answer from the index alone (an index-only scan).
  Index entries carry no visibility information, so the scan skips the table
  only for pages that vacuum has marked all-visible (every row on the page
  visible to every transaction); for other pages it still reads the table.
- The ledger does not enforce balance floors, such as a balance that may not
  go below zero on the account's normal side (ADR-0003). Rules such as "a
  refund cannot exceed the captured amount" belong to the payment layer
  (phase 2), which coordinates by locking the payment row. Only that
  payment's own operations wait on it, unlike a fee account that every
  payment touches.
- Point-in-time ("as of") balances are not offered in phase 1. A timestamp
  filter is not final: a transaction can commit after the cut-off while
  carrying an earlier timestamp.

## Consequences

- Nothing can drift: the balances always add up to the sum of the entries,
  so the trial balance (every currency sums to zero across the whole ledger)
  follows from I1 alone (ADR-0004).
- Writers never wait on a balance row, but each one still locks the account
  row: its foreign-key check takes a `FOR KEY SHARE` lock, the weakest row
  lock, which conflicts only with `FOR UPDATE` and does not block other
  inserts. When several transactions hold that lock at once, PostgreSQL
  stores a MultiXact ID in the row to name the group of lockers. Each new
  locker creates a new MultiXact that copies the group, and the space for
  these member lists is finite until vacuum frees it. On a very hot account
  this traffic must be monitored, for member-space usage as well as
  MultiXact age: running out of MultiXact member space, compounded by
  foreign keys, caused four database-wide write outages at Metronome in May
  2025. The foreign key stays, because it guarantees that each entry's
  account exists and has the entry's currency (ADR-0003). Phase 8 benchmarks
  a hot account under options A, B and I.
- Reads slow down as an account's history grows, and index-only scans depend
  on autovacuum keeping up with inserts (tuned in phase 8). The upgrade path
  is option F, which keeps the entries as the only source of truth. A
  measured read latency, not a guess, would trigger it.
- If the ledger ever has to enforce a balance limit, every transaction that
  moves a bounded balance toward its limit must be serialised. Limits are
  stated on the account's normal side (ADR-0003): for a credit-normal
  account, "never below zero" means `SUM(amount) <= 0`, and debits move the
  balance toward that limit. Three mechanisms work, each with conditions:
  - a transaction-scoped advisory lock (`pg_advisory_xact_lock` on a number
    derived from the account ID), which locks no row and needs no table
    privilege;
  - `FOR NO KEY UPDATE` on the account row, which needs `UPDATE` privilege on
    `accounts`, a privilege the runtime role of ADR-0004 does not have;
  - `SERIALIZABLE` for every such transaction, with its `SUM` and its insert
    in the same transaction and a retry on SQLSTATE `40001`.

  The two locks work only at READ COMMITTED, with the `SUM` in a later
  statement than the lock, so that its snapshot includes the previous
  holder's entries; under REPEATABLE READ the snapshot predates the lock
  wait. The guard must never be `FOR UPDATE` on the account row: that is the
  only row lock that conflicts with the `FOR KEY SHARE` locks of foreign-key
  checks, so every insert for the account would queue behind it.
- With nothing to drift, a test comparing a balance with a re-computed sum
  proves little. Balances are instead checked against an independent
  in-memory model under concurrent writes. The single-statement trial
  balance in the test suite checks I1 across the whole ledger and must
  return no rows:
  `SELECT currency, SUM(amount) FROM ledger_entries GROUP BY currency HAVING SUM(amount) <> 0`.

## Amendments

- 2026-10-21: Rust reads a balance with `balance(pool, AccountId)`. It
  returns the account's currency and normal side with the signed sum, so the
  amount never travels without its currency (ADR-0003), and `None` when no
  account has that ID, which a bare sum would report as zero. The statement
  joins the account row to the sum, so it still reads one snapshot. The
  cast's error, `22003`, is reported as an overflow, a bug: reaching it
  takes more than 9,223 entries at the cap on one side of one account
  (ADR-0003).
- 2026-10-28: a payment's refundable amount, stored on each row of its
  history (ADR-0017), is not a balance in this ADR's sense. It is written
  once and never updated, the constraints fix it from the row before it,
  and only that payment's commands, which run one at a time, write it. No
  account balance is stored, so the decision is unchanged.
- 2026-10-28: ADR-0018 supersedes "which coordinates by locking the payment
  row". The payment layer takes a transaction-level advisory lock on the
  payment's key instead, the first mechanism listed under Consequences,
  because the runtime role has no `UPDATE` privilege and so can lock no
  row. The rest of that bullet stands: only the payment's own commands wait
  on the lock.

## References

- Pat Helland, [Immutability Changes Everything](https://www.cidrdb.org/cidr2015/Papers/CIDR15_Paper16.pdf), CIDR 2015.
- Peter Bailis et al., [Coordination Avoidance in Database Systems](https://www.vldb.org/pvldb/vol8/p185-bailis.pdf), PVLDB 2014.
- PostgreSQL 18 manual, [Index-Only Scans and Covering Indexes](https://www.postgresql.org/docs/18/indexes-index-only-scans.html).
- PostgreSQL 18 manual, [Explicit Locking](https://www.postgresql.org/docs/18/explicit-locking.html) (row-level lock conflicts).
- Metronome, [Root Cause Analysis: PostgreSQL MultiXact member exhaustion incidents (May 2025)](https://metronome.com/blog/root-cause-analysis-postgresql-multixact-member-exhaustion-incidents-may-2025).
