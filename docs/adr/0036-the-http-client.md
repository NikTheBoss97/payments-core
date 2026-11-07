<!-- Author: Nicola Antonello -->

# ADR-0036: The HTTP client

- Status: accepted
- Date: 2026-11-07

## Context

The server now calls the PSP: the sender sends requests (ADR-0028), and the catch-up
reads histories (ADR-0031). The fake delivers notifications to the server (ADR-0029).
Each needs an HTTP client. ADR-0032 fixes what the client may do by itself: it never
retries, since only the sender and the sweep do, and it is bounded in time, since a
call happens outside any transaction and a timeout reads as an unknown outcome.

Measured with `cargo tree -e normal --target x86_64-unknown-linux-gnu`, on scratch
projects at the versions in this repository's lockfile, without TLS:

| Client | Crates in its tree | New to this repository |
|---|---|---|
| `hyper-util` 0.1.21, its legacy client, over HTTP/1 | 25 | 2: `try-lock`, `want` |
| `reqwest` 0.13.5, default features off | 67 | 6: those two, `reqwest`, `tower-http`, `ipnet`, `utf_types` |

The gap is small because axum and sqlx already bring most of the stack. The defaults
differ more:

| | `hyper-util` legacy client | `reqwest` 0.13.5 |
|---|---|---|
| Retries by itself | requests "disrupted before ever starting to write"; "Default is `true`" | requests where "an error or low-level protocol NACK is encountered that is known to be safe to retry", within "a retry budget that permits 20% extra requests" |
| Redirects | its builder has none to set | "Default will follow redirects up to a maximum of 10" |
| Timeouts | none built in | "Default is no timeout" |
| TLS, when deployed | `hyper-rustls` | a feature |

## Options

| Option | Why not |
|---|---|
| **`hyper-util`'s legacy client** | Chosen |
| `reqwest` 0.13 | Four more crates, and three defaults to override: redirects, retries and timeouts. It is shorter to write, and TLS is a feature rather than a crate |

## Decision

- **The client** is `hyper_util::client::legacy::Client`, with the features
  `client-legacy`, `http1` and `tokio`, and `retry_canceled_requests(false)`, so that
  nothing below the sender retries.
- **Every call is bounded** by a timeout around it, set in the implementation. It never
  wraps a database transaction, since a call to the PSP runs outside one (ADR-0026), so
  ADR-0014's rule against timeouts around a `COMMIT` does not apply. A timeout means an
  unknown outcome (ADR-0032).
- **Plain HTTP.** TLS belongs to deployment (ADR-0023, ADR-0030), and would add
  `hyper-rustls`, not measured here.
- **The fake uses the same client** for its deliveries.

## Consequences

- Two new crates, under `cargo deny` (ADR-0027).
- More code per call than with `reqwest`: building the request and collecting the body
  with `http-body-util`, which is already built.
- `hyper-util` calls this client "legacy", and its docs say only "Legacy implementations
  of connect module and Client". If a successor appears, moving to it is a change of one
  module.

## References

- hyper-util, [`client::legacy::Builder`](https://docs.rs/hyper-util/0.1.21/hyper_util/client/legacy/struct.Builder.html) (API docs, 0.1.21).
- reqwest, [`ClientBuilder`](https://docs.rs/reqwest/0.13.5/reqwest/struct.ClientBuilder.html) and [`retry`](https://docs.rs/reqwest/0.13.5/reqwest/retry/index.html) (API docs, 0.13.5).
