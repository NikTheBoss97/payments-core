<!-- Author: Nicola Antonello -->

# ADR-0003: Money representation

- Status: accepted
- Date: 2026-10-14

## Context

The way an amount is represented must never create or destroy money: no
inexact values, no silent rounding, no silent overflow. Five separate choices
are involved: the number type, how debits and credits are encoded, how the
currency is stored, the format at the API boundary, and how fractional
results are rounded.

## Options

Number type:

| Option | Why not |
|---|---|
| **`BIGINT` count of minor units** | Chosen |
| `INTEGER` count of minor units | Tops out at 21,474,836.47 in a two-decimal currency |
| `NUMERIC(p,s)` | Silently rounds extra fractional digits on write, and accepts NaN |
| Unconstrained `NUMERIC` | Accepts NaN, Infinity and any scale |
| PostgreSQL `money` | Takes its scale and currency from the `lc_monetary` setting, not from the value |
| Floating point | Cannot represent 0.1 exactly |
| 128-bit integers | No native PostgreSQL type and no sqlx mapping, for headroom we do not need |
| Units plus nanos (`google.type.Money`) | Two fields with carry and sign rules |
| Decimal strings | No arithmetic, and one amount has many spellings ("10.5", "10.50") |
| `BIGINT` at a finer in-house scale (micros) | Stored amounts are no longer payable without rounding |

Debit and credit:

| Option | Why not |
|---|---|
| **Signed amount, debit positive** | Chosen |
| Positive amount plus a direction column | Every sum needs a `CASE` |
| Separate debit and credit columns | One of the two is always zero |
| Transfer rows with a debit account and a credit account | Each row balances by construction, but a transaction with more than two entries (a capture of 100 split 97 to the merchant and 3 to fees) becomes several rows, the pairing is arbitrary when several accounts sit on each side, and every balance needs two sums |

Currency:

| Option | Why not |
|---|---|
| **ISO 4217 code on every entry, with a composite foreign key** | Chosen |
| Code on the account only | The zero-sum check needs a join, and nothing stops an account's currency from changing |
| Numeric ISO 4217 code | Unreadable, and Stripe, Adyen and Square all use the alphabetic code |
| PostgreSQL enum | Adding a currency needs a schema change, and an enum cannot store the number of decimals |

API format:

| Option | Why not |
|---|---|
| **Integer minor units with an uppercase ISO 4217 code** | Chosen |
| Decimal strings in major units (PayPal) | Differs from Stripe, Adyen and Square, and one amount has many spellings |
| JSON numbers in major units | Many JSON parsers read them as binary floating point |

Rounding:

| Option | Why not |
|---|---|
| **Integer basis points and one allocation function, in Rust** | Chosen |
| Rounding in SQL | Writing a decimal into `BIGINT` rounds silently, and `numeric` breaks ties away from zero |
| A decimal type with banker's rounding | A second number type in the domain, needed only for currency conversion, which is out of scope |

## Decision

- **Number type.** `amount bigint NOT NULL` counts minor units, the
  currency's smallest unit in ISO 4217: 1050 means 10.50 EUR (1,050 cents)
  or 1,050 JPY (the yen has no decimals). Payments move only whole minor
  units, and Stripe, Adyen and Square send them as integers, so an exact
  64-bit integer is enough. `NUMERIC`, which the PostgreSQL manual
  recommends for money, would add NaN, scale rules and a Rust decimal
  library for precision the ledger never uses.
- **Cap.** A CHECK forbids zero and caps a single entry at ±10^15 minor
  units, bounds included: 10 trillion in a two-decimal currency, about 1,000
  times Stripe's 12-digit limit for most card amounts. The cap follows
  Bitcoin's `MoneyRange` check, added after its 2010 value-overflow incident.
  It keeps every amount far from the `i64` limits, so negating an entry can
  never overflow and every amount is exact as a JavaScript number (below
  2^53). It also rejects grossly mis-scaled amounts, such as one carrying
  18 decimals, though not a slip by a factor of 100. It does not make sums
  safe: an `i64` sum of more than 9,223 capped entries can overflow, which is
  why sums use `i128` in Rust and `numeric` in SQL.
- **Debit and credit.** Debits are positive and credits negative, so a
  ledger transaction balances when its entries sum to zero per currency.
  Each account records its normal balance, the side its balance usually sits
  on: debit for assets such as cash at the bank, credit for liabilities such
  as money owed to merchants. A credit-normal account's raw sum is normally
  negative, so it is shown negated.
- **Currency.** A PostgreSQL domain, `currency_code`, over `text`, checked
  against `^[A-Z]{3}$` (not `char(3)`, which pads with spaces). Every entry
  carries the code, and a composite foreign key `(account_id, currency)` to
  `accounts (id, currency)` makes it match the account's currency. A
  `currencies` table stores each currency's ISO 4217 number of decimals,
  and `accounts.currency` references it, so an account cannot use an
  unknown code. Ledger arithmetic never reads the number of decimals; the API
  uses it to validate and format amounts, and each PSP adapter converts
  between it and the PSP's own number of decimals. A currency's number of
  decimals never changes once it is added, because that would rescale every
  stored amount: `currencies` is append-only, and only the migration role
  writes to it (ADR-0004).
- **API.** Amounts travel as positive JSON integers in minor units
  (`amount_minor`), next to an uppercase ISO 4217 code. Stripe, Adyen and
  Square also use integer minor units (Stripe with lowercase codes).
  `amount_minor` is parsed into a positive amount type, separate from the
  signed ledger amount, so a non-integer, zero, negative or over-cap value
  is rejected with a 4xx response. The domain layer, not the client,
  decides the sign of each entry: a transaction with every entry reversed
  still sums to zero, so the zero-sum check could not catch a wrong sign
  sent by a client.
- **Rounding.** The ledger never rounds; the domain rounds once, by a stated
  rule, before it posts. Fee rates are integer basis points (hundredths of a
  percent: 2.9% is 290). A fee is computed in `i128` on the positive amount
  and rounded by one explicit rule, chosen with the payment flows in phase 2,
  and the merchant's entry is the gross minus the fee, so the entries always
  add up to the gross. Splits use one allocation function: each share gets
  its floor, the leftover units go to the largest remainders, and ties go to
  the share listed first. For 5 cents split 70/30 the exact shares are 3.5
  and 1.5: rounding each half up gives 4 and 2 (6 cents), truncating gives 3
  and 1 (4 cents), and the allocation function gives 4 and 1.
- **Rust.** An amount newtype that can only be built through validated
  constructors, including when it is deserialised. Arithmetic is checked,
  sums accumulate in `i128`, and conversions use `From` or `TryFrom` rather
  than `as`, which truncates silently; the workspace denies clippy's
  `as_conversions` lint. As a backstop, the root `Cargo.toml` sets
  `overflow-checks = true` under `[profile.release]` (Cargo's release
  default is `false`, under which overflow wraps silently). A missed check
  then panics, and the open database transaction rolls back instead of
  committing. The setting covers every crate in the build, dependencies
  included, as the dev and test profiles already do. A test overflows on
  purpose and CI runs it in release, so losing the setting fails CI. The
  `money` module also forbids clippy's `arithmetic_side_effects` lint (its
  module doc lists what the lint misses). The workspace does not deny it,
  because an operator that overflows elsewhere panics through this
  backstop, while a truncating `as` has none.
- **SQL.** SQL never computes an amount that is stored: every amount written
  to the ledger comes from a bound `i64` parameter, never from an expression
  such as `amount * 0.029`. Aggregates that only read amounts, such as `SUM`
  in the balance query (ADR-0002) and in the zero-sum check (ADR-0004), are
  exact, because `SUM(bigint)` returns `numeric`. Every column of the ledger
  tables (ADR-0004) is `NOT NULL`.

## Consequences

- Arithmetic is exact everywhere, so the database's zero-sum check never
  involves rounding.
- `SUM(bigint)` returns `numeric`, so balance queries cast the result back to
  `bigint`, which raises an error rather than wrapping on overflow.
- PostgreSQL silently rounds a decimal written into a `BIGINT` column (10.5
  is stored as 11), the same flaw held against `NUMERIC(p,s)`. The
  difference is that no fraction can reach the column: amounts arrive only
  as Rust `i64` parameters, and SQL never computes a stored amount.
- A NULL would slip past the composite foreign key (not checked when any of
  its columns is NULL), the CHECKs (a CHECK that evaluates to NULL passes)
  and `SUM` (which ignores NULLs), hence `NOT NULL` on every ledger column.
- An integer means nothing without its currency's number of decimals, and
  PSPs disagree with ISO 4217 for some currencies: Adyen uses two decimals for
  ISK, and Stripe expects ISK and UGX amounts with two decimals. Each PSP
  adapter owns its conversion, and the mock PSP uses at least one such
  currency so the conversion is tested.
- A bug that reverses every entry still sums to zero, so tests check each
  account's change, not only the total.
- Amounts smaller than one minor unit, and crypto assets, are out of scope
  (DESIGN.md non-goals). Supporting them would change the newtype and the
  column, not the code around them.

## Amendments

- 2026-10-16: the Rust bullet states that `overflow-checks` covers every
  crate in the build and that a CI test guards it. The decision is
  unchanged.
- 2026-10-16: the Rust bullet allows `From` as well as `TryFrom`, because
  clippy rejects a fallible conversion where an infallible `From` exists,
  and names the workspace lint that denies `as`. The decision, no `as`, is
  unchanged.
- 2026-10-16: the Rust bullet records that the `money` module forbids
  clippy's `arithmetic_side_effects` lint, and why the workspace does not
  deny it. The decision is unchanged.

## References

- Martin Fowler, [Money](https://martinfowler.com/eaaCatalog/money.html), Patterns of Enterprise Application Architecture.
- PostgreSQL wiki, [Don't use money](https://wiki.postgresql.org/wiki/Don%27t_Do_This#Don.27t_use_money).
- Stripe, [Supported currencies](https://docs.stripe.com/currencies).
- Adyen, [Currency codes and minor units](https://docs.adyen.com/development-resources/currency-codes).
- Square, [Money object](https://developer.squareup.com/reference/square/objects/Money).
- Bitcoin Core, [consensus/amount.h](https://github.com/bitcoin/bitcoin/blob/master/src/consensus/amount.h) (`MoneyRange`).
