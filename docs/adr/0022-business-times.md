<!-- Author: Nicola Antonello -->

# ADR-0022: Business times

- Status: accepted
- Date: 2026-10-28

## Context

ADR-0012 deferred effective time, the time at which something happened in
the world, on one condition: phase 2 links every money-moving ledger
transaction to the business record that holds its business time. Phase 6
will match settlement lines by such times, and phase 4's sweeps need the
PSP's authorization deadline.

Rust needs a date-time type that sqlx can bind and read. sqlx offers two,
behind its `time` and `chrono` features. With both enabled, every query
whose date type sqlx infers calls a function marked `#[deprecated]`, so
CI's `-D warnings` fails the build: whichever crate is chosen, the other
cannot slip in unnoticed. The crate was to be chosen by measurement, and
the two natural measures disagree:

| Packages added | `time` | `chrono` |
|---|---|---|
| to `Cargo.lock`, which lists every platform's dependencies | 6 | 13 |
| to the build on Linux, where CI runs | 6 | 2 |
| to the build on Windows | 6 | 1 |

sqlx enables chrono's `clock` feature, which brings in `iana-time-zone`, a
crate that finds the local time zone, and its helpers for macOS, Android,
Haiku and Windows. 11 of chrono's 13 lock entries are never built on Linux
or Windows. Dependabot does not watch this repository's Cargo dependencies,
so a lock entry that is never built costs nothing.

## Options

| Option | Why not |
|---|---|
| **Store business times now, as `chrono::DateTime<Utc>` through sqlx's `chrono` feature** | Chosen |
| The same, with `time::OffsetDateTime` | Wins only on lock entries, which nothing here acts on, and adds 6 packages to every build instead of 1 or 2 |
| Defer business times to phase 4 | Breaks ADR-0012's condition, and captures made before phase 4 would have no date |

## Decision

- **Columns.** `payment_authorizations.expires_at`,
  `payment_captures.captured_at` and `payment_refunds.refunded_at`, each
  `timestamptz NOT NULL` without a precision (ADR-0012). They hold the
  deadline the PSP reported, and the times at which the capture and the
  refund took effect.
- **Supplied by the caller.** Each time is part of its command, and the
  library never reads a clock for it: the ID constructors stay the only
  clock reads (ADR-0005). A retry compares the time like the rest of the
  command (ADR-0019).
- **One type.** A newtype over `chrono::DateTime<Utc>`, a type that states
  its time zone, used through sqlx's re-export `sqlx::types::chrono`. The
  type is then always the one that sqlx encodes, and no direct dependency
  has to be kept in step with sqlx's.
- **Microseconds.** PostgreSQL stores a `timestamptz` to the microsecond,
  and sqlx sends a value as whole microseconds, dropping the rest. The
  newtype therefore truncates to whole microseconds when it is built
  (`trunc_subsecs(6)`). Otherwise a time with nanoseconds would come back
  from the database different from the command's, and every retry would be
  `Mismatch`.
- **The feature.** sqlx's `chrono` feature is enabled in the same commit as
  the first query that binds or returns a time. When Rust first reads
  `recorded_at`, it uses the same type.
- **Records carry no `recorded_at`.** They commit with their step, whose
  `recorded_at` is the same instant (ADR-0017).

## Consequences

- ADR-0012's condition holds: every money-moving ledger transaction is
  linked to a record that holds its business time.
- A retry rebuilt with a fresh time is `Mismatch`: callers resend the
  identical command, time included.
- chrono's `clock` feature offers `Utc::now()`. The library does not call
  it, and review keeps it so, under ADR-0005's rule on clock reads.
- Enabling `time` as well, by mistake or through another dependency that
  turns on sqlx's feature, fails CI's build.
- Linux builds also compile `iana-time-zone`, which nothing in the library
  uses.

## References

- Krishna Kulkarni and Jan-Eike Michels, [Temporal features in SQL:2011](https://doi.org/10.1145/2380776.2380786) (SIGMOD Record, 2012).
- Michael H. Böhlen, Anton Dignös, Johann Gamper and Christian S. Jensen, [Temporal Data Management – An Overview](https://doi.org/10.1007/978-3-319-96655-7_3) (eBISS 2017, Springer LNBIP 324, 2018).
- Martin Fowler, [Bitemporal History](https://martinfowler.com/articles/bitemporal-history.html) (2021).
- PostgreSQL 18 manual, [Date/Time Types](https://www.postgresql.org/docs/18/datatype-datetime.html) (resolution of 1 microsecond).
- chrono 0.4.45, [`DateTime`](https://docs.rs/chrono/0.4.45/chrono/struct.DateTime.html) and [`SubsecRound::trunc_subsecs`](https://docs.rs/chrono/0.4.45/chrono/round/trait.SubsecRound.html).
- sqlx `f1e94ec`: [sqlx-core/src/types/mod.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/types/mod.rs) (the `chrono` re-export), [sqlx-postgres/src/types/chrono/datetime.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-postgres/src/types/chrono/datetime.rs) (whole microseconds), [src/lib.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/src/lib.rs) (`warn_on_ambiguous_inferred_date_time_crate`).
