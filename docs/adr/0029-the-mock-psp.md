<!-- Author: Nicola Antonello -->

# ADR-0029: The mock PSP, and how tests drive it

- Status: accepted
- Date: 2026-11-07
- Supersedes: in ADR-0001's Option 3, "`mock-psp` (a separate binary)": it becomes a
  library with a thin binary, which `payments-core` takes as a dev-dependency.

## Context

Every claim this phase makes is about a failure at the PSP's edge: a duplicate, a delay,
a reordering, a lost reply, a forgotten key, a PSP that is down, our own server dying.
A test proves that we survive a failure only if it can cause that failure on demand, the
same way on every run. So the stand-in for the PSP decides what phase 4 can prove.

Meszaros names the two kinds of double this needs. A Test Stub is "a control point to
inject indirect inputs". A Fake Object is "a much lighter-weight implementation" that
provides "a way for the interactions to occur in a self-consistent manner", so what was
sent earlier comes back later. ADR-0028's central scenario needs both: the PSP must still know what
it did after forgetting a key, and the test must decide when it fails. A double that
each test configures is a Configurable Test Double, so `mock-psp` is, strictly, a
configurable fake.

Providers' own tools do not cover this. Stripe's stripe-mock "does not attempt to
reproduce the behavior of the real Stripe API at all", "is stateless", and tells
regression suites to "define your own mocks". Deterministic simulation, as in
FoundationDB, runs the real code in a simulator where "all sources of nondeterminism and
communication are abstracted, including network, disk, time, and pseudo random number
generator", and FoundationDB "was built from the ground up" for it. In Rust, turmoil runs "multiple
concurrent hosts within a single thread". A PostgreSQL server is a process of its own,
with its own sockets, disk and scheduling, so neither can contain it.

## Options

Where the PSP's stand-in runs, and how a test drives it:

| Option | Why not |
|---|---|
| **A configurable fake in the test process, over real HTTP: `mock-psp` as a library with a thin binary** | Chosen |
| The binary alone, scripted over an HTTP control API | Cargo gives an integration test the paths of its own package's binaries (`CARGO_BIN_EXE_<name>`); another package's binary needs artifact dependencies, still unstable (`-Z bindeps`), or a build step outside Cargo. The control protocol would need a design and tests of its own |
| No PSP: tests post hand-signed notifications, and a stub answers the server's calls | A stub keeps nothing, so it cannot forget a key while keeping its records, answer ADR-0028's lookup, outlive our server's death with its records, or deliver again when our endpoint fails |

How tests control time:

| Option | Why not |
|---|---|
| **No fake clock: what time triggers at a PSP is a step that the test calls** | Chosen |
| tokio's paused clock | It advances by itself "once the runtime has no work to do", and the documented way to keep it still "even while waiting for I/O" is `spawn_blocking`, which sqlx's async I/O does not use. Timers could jump while a test waits on PostgreSQL. Read from the docs, not measured |

## Decision

- **Shape.** `crates/mock-psp` is a library, the fake, with a binary that serves it for
  local runs. `payments-core` lists it only under `[dev-dependencies]`, so the server's
  code cannot use it and reaches it only over HTTP. `mock-psp` depends on nothing of
  `payments-core`: they share the wire format and no code.
- **Its API:**

  | Route on the fake | What it does |
  |---|---|
  | `POST /v1/requests` | Takes a request under our `RequestId` and answers at once whether it holds it (ADR-0032). A copy within its idempotency cache gets the same answer; after the cache forgets, a copy counts as new, as at Stripe |
  | `GET /v1/payments/{payment_id}/history` | A payment's facts in the PSP's order, and the requests the PSP holds (ADR-0031) |

  It delivers thin, signed notifications to the server's `POST /webhooks/mock`
  (ADR-0030, ADR-0031), on the schedule of ADR-0032.
- **One fake per test,** on a random local port, as the `wiremock` crate does: "Each
  instance of MockServer is fully isolated". The fake lives in the test process, so it
  keeps its records when a test kills the server binary, as `tests/server_binary.rs`
  already does, and as a real PSP would.
- **The handle.** A test scripts each decision and fault through a Rust handle: approve,
  for a smaller amount too, or decline; refuse a capture; hold, duplicate, reorder or
  drop a notification; act and then lose the reply; answer 429 or 503 for a while;
  forget its idempotency keys; sign with a wrong key or a stale timestamp. It can also
  read what the fake received and how often it acted.
- **Two memories.** Permanent records of what it did, which make the history, and an
  idempotency cache that forgets after a day, as Stripe's may, or when the test says so.
- **No fake clock.** Pruning keys, expiring an authorization, settling and the next
  delivery attempt are steps that the test calls, and the binary runs the same steps on
  timers. ADR-0028's sender has the same shape: one step, "send what is due at time
  *t*". This is Meszaros's Humble Object: "We extract the logic into a separate
  easy-to-test component that is decoupled from its environment", and the loop that
  calls it stays thin. A notification's timestamp comes from the real clock on both
  sides; its check is a pure function of `now`, tested with fixed times (ADR-0030).
- **Determinism.** In a test the fake makes no random choice: the test scripts every
  decision and fault. Property tests generate schedules, and replay a failure from
  proptest's saved seed. What stays out of reach is the scheduling of our tasks against
  PostgreSQL, which deterministic simulation would control.
- **Fidelity.** The fake has its own tests, because "A fake must have its own tests"
  (Software Engineering at Google). Each behaviour it copies cites its source:

  | Behaviour | Source |
  |---|---|
  | Answers a request at once and decides later | Adyen: a capture "will be processed asynchronously" |
  | Forgets idempotency keys while keeping its records | Stripe prunes keys "at least 24 hours old" |
  | Duplicates a notification, and sends one fact in two events | Stripe: "two separate Event objects are generated and sent" |
  | No order between notifications | Stripe: "doesn't guarantee the delivery of events in the order that they're generated" |
  | Retries a failed delivery for days, with backoff | the Standard Webhooks example schedule |
  | Thin notifications, with the state read back | Stripe's thin events |
  | Authorizes less than asked, and reports an expiry | ADR-0016 |

  Fowler's answer to "is the double accurate?" is contract tests against the real
  service. No real PSP is in reach here, so the documentation is the contract.
- **Scope:** authorize or decline, capture, void, refund, settle and expire; deliveries
  and their retries; pruning keys; the history. Not 3-D Secure, disputes, or a capture
  that fails after it succeeded (ADR-0028).

## Consequences

- The workspace gains its second crate, as ADR-0001 planned. The server's code still
  cannot reach the PSP's internals; its tests can, as the operator of a real PSP's
  sandbox can drive its controls.
- `docker compose` can run the binary next to the server, for a local demonstration.
- Fidelity rests on documented behaviour. What a real PSP returns for a payment's
  history is not verified (ADR-0031).
- Shared nothing but the wire format: a misunderstanding of it fails a test instead of
  hiding in shared code. ADR-0030's published test vector checks the signatures of both.
- The fake brings no new crate of its own beyond ADR-0036's client: axum, `hmac` and
  `sha2` are already built.
- Exploring faults automatically, as Filibuster extends "existing functional test
  suites to cover failure scenarios", is left to phase 7. Phase 4 writes its scenarios
  by hand.

## References

- Gerard Meszaros, [xUnit Test Patterns: Refactoring Test Code](http://xunitpatterns.com/) (Addison-Wesley, 2007): Test Double, Fake Object, Configurable Test Double and Humble Object.
- Andrew Trenk and Dillon Bly, [Test Doubles](https://abseil.io/resources/swe-book/html/ch13.html), chapter 13 of Software Engineering at Google (O'Reilly, 2020).
- Martin Fowler, [ContractTest](https://martinfowler.com/bliki/ContractTest.html) (2011).
- Jingyu Zhou et al., [FoundationDB: A Distributed Unbundled Transactional Key Value Store](https://doi.org/10.1145/3448016.3457559) (SIGMOD, 2021), section 4.
- tokio-rs, [turmoil](https://github.com/tokio-rs/turmoil).
- Christopher S. Meiklejohn, Andrea Estrada, Yiwen Song, Heather Miller and Rohan Padhye, [Service-Level Fault Injection Testing](https://doi.org/10.1145/3472883.3487005) (SoCC, 2021).
- Stripe, [stripe-mock](https://github.com/stripe/stripe-mock) (README).
- tokio, [`time::pause`](https://docs.rs/tokio/latest/tokio/time/fn.pause.html) (API docs).
- wiremock, [crate documentation](https://docs.rs/wiremock/latest/wiremock/) (test isolation).
- The Cargo Book, [Environment variables](https://doc.rust-lang.org/cargo/reference/environment-variables.html) and [Artifact dependencies](https://doc.rust-lang.org/cargo/reference/unstable.html#artifact-dependencies).
