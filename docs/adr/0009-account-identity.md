<!-- Author: Nicola Antonello -->

# ADR-0009: Account identity and the normal side

- Status: accepted; partly superseded by ADR-0020
- Date: 2026-10-17

## Context

Phase 2 must find specific accounts, such as the EUR fee-revenue account or
a merchant's EUR payable account. The UUIDv7 `id` (ADR-0005) is a surrogate
key: stable, compact and what entries reference, but it says nothing about
what an account is. Code also needs a business key.

Accounts are append-only (ADR-0004), so a row may carry only facts that never
change. A label with a typo, or a merchant's old name, would stay forever.

Each account also records its normal side (ADR-0003): the side on which its
balance increases, debit for assets and expenses, credit for liabilities,
equity and revenue. The column needs a type that sqlx's `query!` macros can
check without a type override (ADR-0006).

## Options

Identity:

| Option | Why not |
|---|---|
| **Business key `(kind, party, currency)`** | Chosen |
| One address string per account, such as `merchant_payable:<id>` (Formance) | Structure inside a string cannot be checked: a typo becomes a new account, and totals per kind need string parsing |
| The same key, with the chart of accounts created and seeded now | Fixes phase-2 decisions (how fees, refunds and settlement are booked) in seeds that cannot change later: the table would be append-only, and sqlx refuses to run when an applied migration's file was edited |
| Anonymous accounts, found through mapping tables written in phase 2 | The ledger cannot say what its own accounts are, every lookup needs a join, and repointing a mapping changes where money goes |

Normal side:

| Option | Why not |
|---|---|
| **`text` with a `CHECK`, parsed into a Rust enum** | Chosen |
| PostgreSQL enum type | `query!` cannot map it without a type override, or without the `sqlx-toml` feature and a config file; a value cannot be removed without dropping and re-creating the type |
| `boolean`, such as `credit_normal` | The meaning hides in the column name, and SQL also accepts `'yes'`, `'on'` and `'1'` as true |
| `smallint` sign, +1 or -1 | Unreadable, and it invites multiplying amounts by a display flag |

## Decision

- **Columns.** `accounts` has exactly `id`, `kind`, `party`, `currency`,
  `normal_side` and `recorded_at` (ADR-0012): no name, description, metadata
  or balance.
- **Business key.** `UNIQUE (kind, party, currency)`, every column
  `NOT NULL`. By default a `UNIQUE` constraint treats NULLs as distinct, so a
  NULL in the key would allow duplicates.
  - `kind` is the account's place in the chart of accounts, such as
    `fee_revenue` or `merchant_payable`:
    `CHECK (kind ~ '^[a-z][a-z0-9_]{0,62}$')`.
  - `party` is whom the account is kept for: `'platform'` for the
    business's own accounts, a lowercase label such as a PSP's name, or a
    merchant's ID as a canonical lowercase hyphenated UUID. A `CHECK` allows
    only these forms. Labels never contain a hyphen and UUIDs always do, so
    every party has exactly one spelling. The column is called `party`, not
    `owner`, because a PSP does not own the receivable kept for it.
  - `UNIQUE (id, currency)` stays as the target of the entries' foreign key
    (ADR-0003).
- **Normal side.** `normal_side text NOT NULL` with
  `CHECK (normal_side IN ('debit', 'credit'))` and no default. Rust parses it
  once into `enum NormalSide`. It is for display only: the domain posts
  explicit debits and credits and never derives an entry's sign from it,
  because reversed entries still sum to zero (ADR-0003).
- **Closed sets of labels.** Application SQL never spells a label such as
  `'debit'`: it binds `NormalSide::as_str()`, or returns the column for Rust
  to parse. A misspelt literal would compile and silently match nothing. No
  sqlx type override is used, on parameters (ADR-0006) or on result columns,
  because both skip the type check. A database round-trip test of every
  variant stands in for the check an enum would give. Phase 2's payment
  statuses follow the same rule.
- **Creating accounts.** Posting never creates accounts: creating them on
  first use turns every typo into an account. An account is created by
  `INSERT ... ON CONFLICT (kind, party, currency) DO NOTHING` in its own
  statement, at READ COMMITTED, followed by a `SELECT` by key that also
  checks the stored normal side. The two are separate statements because a
  single statement reads one snapshot: written as one CTE, it returns no row
  when it loses a race to another session. A database transaction that
  creates several accounts creates them in sorted key order, so that two
  such transactions cannot deadlock.
- **Caching and recovery.** An account's ID is read back by key, and cached
  only after the creating statement has committed, per connection pool and
  never in a process-wide static. After a lost `COMMIT` (ADR-0005), the
  service runs get-or-create again and reads the ID by key: for an account,
  the key is the identity, not the generated ID.
- **The chart of accounts waits for phase 2,** which designs the payment
  flows. It then adds an append-only `account_kinds` table seeded with the
  real chart, and `FOREIGN KEY (kind, normal_side) REFERENCES
  account_kinds (kind, normal_side)`, which also ties each kind to its normal
  side. In Rust, a typed reference such as
  `enum AccountRef { FeeRevenue(Currency), MerchantPayable(MerchantId, Currency) }`
  then becomes the only producer of keys.

## Consequences

- One indexed lookup finds any account, and creating an account is
  idempotent and safe under concurrent callers, before phase 3's idempotency
  keys exist.
- Every column is part of the account's identity, so "an account that needs
  different attributes is replaced by a new account" (ADR-0004) always means
  a new key. Display labels live outside the ledger.
- Violations use PostgreSQL's own codes and the constraint's name: `23505`
  for a duplicate key, `23514` for a malformed kind, party or side, and
  `23503` for an unknown currency and, from phase 2, an unknown kind or a
  kind with the wrong normal side.
- The database checks how a party is spelled, not whether it is the right
  party for the kind. Rust guards that, through the typed reference and a
  posting path that never creates accounts.
- Each new kind needs a migration, on purpose.
- The total of a kind is a `SUM` over its accounts, like a control account
  over its subsidiary ledger, and is never stored (ADR-0002).

## Amendments

- 2026-10-20: the `party` CHECK also rejects a label of exactly 32
  hexadecimal digits, the form a UUID takes without its hyphens. Otherwise a
  merchant whose ID begins with a letter would have a second accepted
  spelling, so "exactly one spelling" needs both rules. The decision is
  unchanged.
- 2026-10-28: `account_kinds` landed with three kinds (ADR-0020), and
  `23503` on its key, `accounts_kind_normal_side_fkey`, is classified as a
  bug. ADR-0020 supersedes "then becomes the only producer of keys":
  `AccountRef` builds every key that the library uses, and `AccountKey`
  stays public, so that ledger-level tests can open accounts of kinds they
  seed.

## References

- PostgreSQL 18 manual, [Constraints](https://www.postgresql.org/docs/18/ddl-constraints.html) (unique constraints and NULLs; foreign-key targets).
- PostgreSQL 18 manual, [INSERT](https://www.postgresql.org/docs/18/sql-insert.html) (`ON CONFLICT DO NOTHING`).
- PostgreSQL 18 manual, [Transaction Isolation](https://www.postgresql.org/docs/18/transaction-iso.html) (`ON CONFLICT` at Read Committed).
- PostgreSQL 18 manual, [Enumerated Types](https://www.postgresql.org/docs/18/datatype-enum.html).
- PostgreSQL 18 manual, [UUID Type](https://www.postgresql.org/docs/18/datatype-uuid.html) (the input forms it accepts, including 32 digits without hyphens).
- Mitchell Franklin, Patty Graybeal and Dixon Cooper, [Principles of Accounting, Volume 1: Financial Accounting](https://openstax.org/details/books/principles-financial-accounting) (OpenStax, 2019): normal balances and the chart of accounts (chapter 3), subsidiary ledgers and control accounts (section 7.2).
- Modern Treasury, [Ledger Account object](https://docs.moderntreasury.com/platform/reference/ledger-account-object) (`normal_balance`).
- TigerBeetle, [Data Modeling](https://docs.tigerbeetle.com/coding/data-modeling/) (`ledger`, `code`, `user_data`).
- Formance, [Accounts](https://docs.formance.com/v3.2/modules/ledger/core-concepts/accounts) (account addresses).
