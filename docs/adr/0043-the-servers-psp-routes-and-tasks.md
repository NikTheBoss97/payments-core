<!-- Author: Nicola Antonello -->

# ADR-0043: The server's PSP routes and tasks

- Status: accepted; partly superseded by ADR-0044
- Date: 2026-11-10

## Context

The library holds what phase 4 decided about the PSP: recording a request (ADR-0039),
catching a payment up with the PSP's history and sweeping the payments that wait for it
(ADR-0041), and the sender with its budget (ADR-0042). The server runs none of it. Its
four routes still apply steps with times that the merchant gives, no route takes the
PSP's notifications, and nothing sends a request or sweeps.

The records fix most of what the server does. The four routes record a request under the
payment's lock and answer 202 (ADR-0028, ADR-0033). `POST /webhooks/{psp}` verifies a
notification before it parses the body, records it once in a short transaction, answers
202, 400 or 503, and catches up after the answer (ADR-0030, ADR-0032). The sender is a
task in the server, which hands it each request after the commit (ADR-0028, ADR-0042).
Three things are left open: how the work is split into pull requests, how a request
reaches the sender's task, and how the catch-ups that notifications start are bounded.

A notification costs one short transaction and one read of the PSP (ADR-0031,
ADR-0032). A pool of sqlx holds at most 10 connections unless told otherwise, and the
system role's pool also serves the operator's routes and the lookup of every API key
(ADR-0025). The PSP may announce many facts at once, as after an outage.

## Options

How the work is split:

| Option | Why not |
|---|---|
| **Three pull requests, each leaving the server working: the webhook route, then the server's tasks, then the switch of the four routes** | Chosen |
| Two: the routes, then the background work | The routes would record requests that nothing sends until the second, or would carry the tasks with them |
| One | Too large to review: routes, tasks, the API's document, the quickstart, and phase 3's HTTP tests moved onto the fake |

How a request reaches the sender's task:

| Option | Why not |
|---|---|
| **A bounded channel in the process, which the route tries once after the commit; a full or closed channel drops the request** | Chosen |
| An unbounded channel | A sender that falls behind, as in an outage, holds every new request in memory |
| PostgreSQL's `LISTEN` and `NOTIFY` | `NOTIFY` reaches "each client application that has previously executed `LISTEN`", so every process would send the request without a lookup, which only the process whose commit recorded it may do (ADR-0042) |

How the catch-ups that notifications start are bounded:

| Option | Why not |
|---|---|
| **A task per notification, at most 4 at once, through a semaphore that the route tries and never waits on; without a permit, the sweep catches the payment up** | Chosen |
| One worker with a queue | One slow read of the PSP holds up every catch-up behind it |
| A task per notification, unbounded | A burst could take every connection of the system role's pool, and with them the lookup of every API key |

## Decision

- **Three pull requests.** The first adds the webhook route: the PSP's configuration,
  verification checked against the specification's test vector, recording, the answers,
  and the catch-up after the answer. The second runs the sender's steps and the sweep's
  passes in the server. The third switches the four routes to recorded requests with the
  handover, adds the request route and the authorization in `GET /payments/{id}`, and
  moves the API's document, the quickstart and phase 3's HTTP tests onto the fake. Until
  the third, the four routes apply their steps as in phase 3.
- **The configuration.** `MOCK_PSP_URL`, `MOCK_PSP_API_KEY` and `MOCK_PSP_SIGNING_KEYS`
  are required, and the server stops at once without one, naming the variable and never
  its value. A variable that is set but empty counts as not set. The keys are written
  `whsec_` and base64 and separated by spaces, as the fake's binary reads them. The
  server reads a key of 24 to 64 bytes, the specification's range, so that its test
  vector, whose key has 24, goes through the same parser; every key that this project
  makes has 32 (ADR-0030). Each call to the PSP waits at most 10 seconds (ADR-0042).
- **Verification** follows ADR-0030's order, with these details. Each of the three
  headers appears exactly once. The ID is 1 to 255 of `[0-9A-Za-z_-]`, the shape that
  `psp_notifications` allows, and the timestamp is ASCII digits only, so it has no sign.
  The MAC covers the ID and the timestamp as they were received. The window holds its
  ends: 300 seconds off, either way, verifies, and 301 does not, as in the
  specification's reference library. The signature header is split at each space, an
  entry that is not `v1,` and the base64 of 32 bytes is skipped, and the notification
  verifies when one entry matches one key.
- **Reading the body.** Only a verified body is parsed. It must hold `data.payment_id`, a
  UUID. Its `type` and `timestamp` are not checked, and other fields are ignored, as in
  histories (ADR-0041).
- **The answers.** A notification recorded now or before gets 202, with no body. Every
  refusal is one 400, the problem `notification-refused`, whichever check failed, a body
  above the 2 MB limit included, and it logs nothing, since anyone can post one. A failure
  to record is the problem `unavailable` with `Retry-After: 5`, the first delay of the
  specification's example schedule. A PSP other than `mock` is a 404.
- **Recording** takes one transaction as `payments_app`: the insert, which also reads
  whether the payment exists, then the `COMMIT` through the one function of ADR-0014. A
  `23505` on `psp_notifications_pkey` drops the transaction, finds the copy, and takes the
  payment that the copy names. A copy is answered and caught up like the first, whose
  catch-up may have found no permit or failed. A notification about a payment that the
  service does not hold is recorded, logged as a warning, answered 202, and starts no
  catch-up.
- **The catch-up after a notification** runs in a task of its own, once the route has
  tried the semaphore of 4 and got a permit, which the task holds until it ends. Without
  one, the route logs that it leaves the payment to the sweep, as a catch-up that fails
  leaves it. The answer never waits for it. It costs no token (ADR-0042), is never retried
  (ADR-0032), and its result is logged when it fails or stops the payment.
- **The handover.** A channel of 1,024 request IDs. After the commit, the route tries to
  send the ID, only for a request that this attempt recorded and never when it replays a
  stored 202. A full or closed channel drops it, and the sender's search finds the
  request as a resend, looked up first (ADR-0042).
- **The sender's task.** One task owns the `Sender` (ADR-0042). It takes a step every
  second and at each handover, with the time now and the operating system's randomness,
  and a pass of the sweep every 60 seconds. A `select!` only picks the next event, and the
  work runs after it, so no step or pass stops half done. A report that is not empty is
  logged. The task calls the PSP once at a time, so a pass can hold the steps back for at
  most its tokens times the 10-second timeout; phase 8 measures it.
- **The four routes** answer 202 with `Retry-After: 1`. The request route answers
  `request_id`, `payment_id`, `kind`, `amount` and `state`, and, once the request applied,
  the amount applied and its business time. `GET /payments/{id}` adds `authorization`,
  `null` until there is one (ADR-0033).
- **Tests over HTTP** reach a state through the fake: the route records a request, the
  fake decides it, and a catch-up applies it. They never use the library's step commands,
  since `LG007` (ADR-0034) will refuse the PSP's steps that those write without a fact.
  One test starts the binary next to a fake and waits, bounded, for an authorization to
  apply. Tests and the quickstart make their keys at runtime, so no key but the
  specification's vector enters the repository (ADR-0030).

## Consequences

- Each pull request leaves a server that starts and passes its tests, and phase 3's
  routes keep their behaviour until the third.
- A notification is answered after one short transaction, whatever the PSP's history
  holds. A burst starts at most 4 reads of the PSP, and the sweep catches up the rest.
- A lost handover costs one lookup, never a send without one.
- A refused notification leaves no line in the log. A wrong key shows as the PSP's failed
  deliveries, while the sweep keeps payments current; phase 8 adds the counts.
- The first pull request's tests: the specification's vector; each check failing alone;
  a body changed by one byte; the window's ends, as a pure function of `now`; two keys
  during a rotation; each answer of ADR-0032's table, both ways; a `23505` between two
  copies; an unknown payment; an answer that does not wait for its catch-up; the bound of
  4; and the binary verifying a notification under the keys in its environment and
  catching its payment up.

## Amendments

- 2026-11-10: ADR-0044 supersedes, in "The sender's task", "A report that is not empty
  is logged". A step's report lists held and overdue requests in every step while they
  last, and in an outage each request that finds no token, so the task logs what
  changes, once: each attempt and catch-up that did something, and a request when it
  becomes held or overdue.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0), and its Rust reference library: the window, the signature header and the example schedule.
- PostgreSQL, [NOTIFY](https://www.postgresql.org/docs/18/sql-notify.html) (PostgreSQL 18 documentation).
- tokio, [`Semaphore::try_acquire_owned`](https://docs.rs/tokio/1.54.1/tokio/sync/struct.Semaphore.html#method.try_acquire_owned) and [`mpsc::Sender::try_send`](https://docs.rs/tokio/1.54.1/tokio/sync/mpsc/struct.Sender.html#method.try_send) (tokio 1.54.1 API docs).
- The records that it builds on: ADR-0014, ADR-0025, ADR-0028, ADR-0030, ADR-0031,
  ADR-0032, ADR-0033, ADR-0034, ADR-0036, ADR-0039, ADR-0041 and ADR-0042.
