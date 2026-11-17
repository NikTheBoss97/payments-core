<!-- Author: Nicola Antonello -->

# ADR-0059: Moving times from chrono to time

- Status: accepted
- Date: 2026-11-17
- Supersedes: in ADR-0022's Decision, **One type** and **The feature**, which name chrono;
  in its 2026-11-02 amendment, **How a time is normalised**, **Its range**, and the bounds
  in **What the database holds**.

## Context

On 2026-11-17 RustSec published
[RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543), "chrono is
unmaintained", with no version that clears it. In January its maintainer had announced
that he would wind chrono down, and recommended jiff or time instead
([chronotope/chrono#1768](https://github.com/chronotope/chrono/issues/1768)); he filed the
advisory himself once jiff 1.0 was out. ADR-0027 fails `cargo deny` on an unmaintained
crate anywhere in the tree, so from that day every pull request's `deny` check failed. The
advisory reports no flaw in chrono: it says that none will be fixed.

sqlx offers `chrono` and `time` (ADR-0022). jiff has no support in sqlx
([transact-rs/sqlx#3487](https://github.com/transact-rs/sqlx/issues/3487), open since
2024): the `jiff-sqlx` crate reaches it through a type override in `sqlx.toml`, which
ADR-0009's rule against type overrides excludes.

Read in time 0.3.55's source and in sqlx `f1e94ec`, time differs where this project
depends on the details:

- sqlx re-exports five of time's types, `Date`, `OffsetDateTime`, `PrimitiveDateTime`,
  `Time` and `UtcOffset`, and not its RFC 3339 parser. It encodes and decodes
  `OffsetDateTime`, and not `UtcDateTime`.
- sqlx decodes a `timestamptz` by adding its microseconds to 2000-01-01, with an addition
  that panics outside time's range: the years -9999 to 9999, or -999,999 to 999,999 with
  the `large-dates` feature. ADR-0022's checks allow times up to the year 262142.
- time's RFC 3339 formatter refuses a year outside 0000 to 9999, and drops every trailing
  zero of the fraction: it writes `.12` where the API writes `.120`.
- time's RFC 3339 parser reads `23:59:60.5` as `23:59:59.999999999` of the same day, and
  only in the last second of a month in UTC; it refuses a leap second anywhere else.
  chrono read one anywhere, and ADR-0022's type folded it into the next second.

## Options

| Moving off chrono | Why not |
|---|---|
| Keep chrono, with an `ignore` that states why, and move later | Each phase until the move adds code on a crate that will not be fixed, so the move only grows. The owner chose to move now |
| **Move to time now** | Chosen |
| Move to jiff, which chrono's maintainer recommends | No support in sqlx, and a type override would break ADR-0009's rule |

| The range of a business time | Why not |
|---|---|
| **The years that RFC 3339 writes, 0000 to 9999** | Chosen |
| ADR-0022's bounds, kept through time's `large-dates` | No migration, but a feature that is off by default, for years that no business uses, an upper bound left by chrono, and years written by hand outside RFC 3339 |

| A leap second | Why not |
|---|---|
| **time's reading: the last microsecond of its month** | Chosen |
| The next second, as ADR-0022 folded it | Code of ours would read the text before time parses it, for a case that the PSP does not send |

## Decision

- **The crate.** sqlx's `time` feature in place of `chrono`, which leaves every crate of
  the workspace. `payments-core` depends on time directly, with `parsing`, since sqlx
  re-exports no parser, and `std` for the clock. Cargo resolves both requirements to one
  copy, so the type is still the one that sqlx encodes, and the code names it through
  sqlx's re-export, as it named chrono's. The mocks depend on time with `serde`, for RFC
  3339 on their wire.
- **One type.** `BusinessTime` holds an `OffsetDateTime` in UTC. A clock read is
  `OffsetDateTime::now_utc()`, where it was `Utc::now()`. Whole microseconds since the
  Unix epoch, which chrono's `timestamp_micros` gave, come from two functions beside the
  type, `unix_micros` and `from_unix_micros`.
- **Range.** From `0000-01-01T00:00:00Z` to `9999-12-31T23:59:59.999999Z`: the years that
  RFC 3339 writes, within time's range and PostgreSQL's. A new migration replaces the
  three checks of ADR-0022's amendment, under the same names. Every time that RFC 3339
  writes in UTC fits. One written with an offset can fall up to a day outside either
  end, and is then unreadable, so its fact cannot be placed (ADR-0031).
- **Microseconds.** As before: anything finer is dropped, downwards.
- **Leap seconds.** A time written with a leap second in the last second of a month, in
  UTC, is read as that month's last microsecond, `23:59:59.999999`. A leap second anywhere
  else is unreadable, so its fact cannot be placed (ADR-0031).
- **How the API writes a time.** Unchanged, and written by hand: RFC 3339 in UTC with `Z`,
  and 0, 3 or 6 fractional digits. Stored answers and the bodies of events keep their
  bytes.
- **The mocks** write times with time's own RFC 3339, which drops trailing zeros, and read
  any RFC 3339 time.
- **No ban on chrono.** Its advisory fails `cargo deny` wherever chrono returns, and
  enabling sqlx's `chrono` next to `time` fails the build, as ADR-0022 found of the
  reverse.

## Consequences

- chrono leaves `Cargo.lock` with the 12 helpers that found the local time zone: 13
  entries, as ADR-0022 counted. time brings 6.
- The type and the database refuse a time before the year 0 or after 9999, where they
  accepted 4714 BC to 262142.
- A leap second earlier in a month stops its payment for an operator. UTC has had none
  since the end of 2016, and in 2022 the CGPM decided to raise the largest difference
  allowed between UT1 and UTC by 2035, so that UTC runs on without one for at least a
  century.
- `payments-core` now depends on time directly. Cargo keeps it in step with sqlx's, and a
  sqlx on another major version of time would fail the build.
- Projects exposed to an abandoned package often do not respond, and remove it sooner when
  its end of life is stated explicitly (Miller et al., 2025). Here the advisory stated it,
  and `BusinessTime` was the layer that most of the change goes through.

## References

- RustSec, [RUSTSEC-2026-0543: chrono is unmaintained](https://rustsec.org/advisories/RUSTSEC-2026-0543) (2026).
- Dirkjan Ochtman, [Soft-deprecating chrono and chrono-tz](https://github.com/chronotope/chrono/issues/1768) (chrono issue 1768, 2026).
- time 0.3.55, its source: [`formattable.rs`](https://github.com/time-rs/time/blob/v0.3.55/time/src/formatting/formattable.rs) (RFC 3339 written), [`parsable.rs`](https://github.com/time-rs/time/blob/v0.3.55/time/src/parsing/parsable.rs) (RFC 3339 read, and the leap second), [`date.rs`](https://github.com/time-rs/time/blob/v0.3.55/time/src/date.rs) (the range of years).
- sqlx `f1e94ec`: [sqlx-core/src/types/mod.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/types/mod.rs) (the `time` re-export), [sqlx-postgres/src/types/time/datetime.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-postgres/src/types/time/datetime.rs) (encoding and decoding).
- G. Klyne and C. Newman, [RFC 3339: Date and Time on the Internet: Timestamps](https://www.rfc-editor.org/rfc/rfc3339) (IETF, 2002), section 5.6 and appendix D.
- BIPM, [Resolution 4 of the 27th CGPM: On the use and future development of UTC](https://www.bipm.org/en/cgpm-2022/resolution-4) (2022).
- IERS, [Leap_Second.dat](https://hpiers.obspm.fr/iers/bul/bulc/Leap_Second.dat): the last leap second, at the end of 2016.
- Courtney Miller, Christian Kästner and Bogdan Vasilescu, [“We Feel Like We’re Winging It:” A Study on Navigating Open-Source Dependency Abandonment](https://doi.org/10.1145/3611643.3616293) (ESEC/FSE, 2023).
- Courtney Miller, Mahmoud Jahanshahi, Audris Mockus, Bogdan Vasilescu and Christian Kästner, [Understanding the Response to Open-Source Dependency Abandonment in the npm Ecosystem](https://doi.org/10.1109/ICSE55347.2025.00004) (ICSE, 2025).
