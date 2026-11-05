<!-- Author: Nicola Antonello -->

# ADR-0023: The HTTP API

- Status: accepted
- Date: 2026-11-03

## Context

Phase 2 left the commands as a library API, and ADR-0016 and ADR-0019 put the
endpoints in phase 3, together with idempotency keys. Who may call them is
ADR-0024, whose payments they reach is ADR-0025, and what a retry gets is
ADR-0026. This record decides the routes, what each answer means, and what runs
a request.

An HTTP server brings one hazard of its own. Measured on axum 0.8.9 and hyper
1.12.0: a client that closed its socket 300 ms into a handler had the handler's
future dropped within 10 ms, after one of its five steps. A client that only
half-closed its socket and waited for the answer got the same, and no answer. A
command awaited in a handler could so lose its `COMMIT` whenever a client gives
up, which is what ADR-0014 forbids. Oxide met the same behaviour in Dropshot,
where a disconnect left mutex-protected data inconsistent.

## Options

What runs a request:

| Option | Why not |
|---|---|
| **One middleware that spawns the rest of the stack, for every request** | Chosen |
| `tokio::spawn` in each handler | Forgetting it in one handler is silent, and the extractors, which read the database, still run in the cancellable future |
| Accept the cancellation, since a retry settles the outcome | Safe for the money under ADR-0026, but it breaks ADR-0014's rule that nothing aborts a `COMMIT`, and it turns every client timeout into an unknown outcome that the client caused itself |

Which commands become routes:

| Option | Why not |
|---|---|
| **What a merchant asks for: create, authorize, capture, void, cancel and refund** | Chosen |
| Every command | A merchant could report its own payment settled, declined or expired |

Error bodies:

| Option | Why not |
|---|---|
| **`application/problem+json` (RFC 9457)** | Chosen |
| A JSON shape of this project's own | A standard exists, and the examples of the idempotency draft use it |

## Decision

- **Routes.** Every `POST` requires an idempotency key (ADR-0026).

  | Route | Credential | Body | Success |
  |---|---|---|---|
  | `POST /merchants` | operator | `fee_bps` | 201: `merchant_id` |
  | `POST /merchants/{id}/api-keys` | operator | none | 201: `api_key_id`, `secret` |
  | `POST /api-keys/{id}/revoke` | operator | none | 200 |
  | `POST /payments` | merchant | `currency`, `amount` | 201: `payment_id` and the reply |
  | `GET /payments/{id}` | merchant | | 200: the payment's terms, status and refundable amount |
  | `POST /payments/{id}/authorize` | merchant | `amount`, `expires_at` | 200: the reply |
  | `POST /payments/{id}/capture` | merchant | `amount`, `captured_at` | 200: the reply |
  | `POST /payments/{id}/refund` | merchant | `amount`, `refunded_at` | 200: the reply |
  | `POST /payments/{id}/void` | merchant | none | 200: the reply |
  | `POST /payments/{id}/cancel` | merchant | none | 200: the reply |

- **What stays off HTTP.** `decline`, `expire` and `settle` report what the PSP
  decided, so they stay library functions until phase 4's webhooks call them.
  `authorize` also reports the PSP's answer (ADR-0016), but nothing can be
  captured without it, so it stays on HTTP as the merchant's stand-in until
  then.
- **The server creates every ID** (ADR-0005), so a client cannot name a payment
  before it exists.
- **JSON.** Amounts are integers in minor units, and a fraction or exponent is
  refused when parsed. Currencies are ISO 4217 codes. Times are RFC 3339 strings
  that become a `BusinessTime` (ADR-0022), so one that cannot be stored is
  refused. Unknown fields are refused, so a misspelt or forbidden field, such as
  a merchant or a fee on `POST /payments`, is never silently ignored.
- **The reply** is the `Recorded` of ADR-0019: `seq`, `from`, `to` and
  `refundable`, with statuses as their labels.
- **Answers.**

  | Answer | Status | Stored under the key (ADR-0026) |
  |---|---|---|
  | Applied | 201 or 200 | yes |
  | An illegal move | 409 | yes |
  | An amount above its limit | 422 | yes |
  | No such payment, merchant or API key, or another merchant's payment | 404 | no: IDs are made by the server, so this cannot become true later |
  | An unknown currency | 422 | no: currencies change only by a migration |
  | A malformed body or idempotency key, or none | 400 | no |
  | A key reused with a different request | 422 | no |
  | A missing, unknown or revoked credential | 401 | no |
  | `NotConfirmed` | 503: retry with the same key | no |
  | `Invariant`, or a panic | 500 | no |

- **Errors** carry an absolute `type` URI into `docs/api.md`, which also publishes
  the idempotency policy, as the draft requires. Their text never reveals SQL, a
  constraint or another merchant's data.
- **One middleware runs every request in a task of its own,** from the
  extractors to the response, so a client that goes away cancels nothing.
  Measured with the middleware in place: a handler ran all of its steps after
  its client closed the socket, a panic became a 500, and a request whose body
  never finished arriving never started its handler.
- **No timeout wraps a request** (ADR-0014). Waits are bounded on the server
  (ADR-0018).
- **The binary** reads `TENANT_DATABASE_URL`, `SYSTEM_DATABASE_URL`,
  `OPERATOR_KEY_SHA256` and `BIND_ADDRESS`, which defaults to `127.0.0.1:8080`. It
  never runs migrations (ADR-0008) and speaks plain HTTP: TLS ends in front of it.
- **Dependencies:** `axum`, `tokio`, `serde` and `serde_json`, and in tests `tower`
  and `http-body-util`, to send a request to the router without a socket.

## Consequences

- A client that hangs up gets no answer, and its command still finishes. Its retry
  gets the answer (ADR-0026).
- A client that half-closes its socket gets no answer at all, because hyper reads
  that as the client having gone. `docs/api.md` says so.
- Nothing limits how many requests run at once. Each spawned task waits for a
  pooled connection, and one that waits too long answers 503. A limit belongs to
  what phase 8 measures.
- 409 always means a conflict with the payment's state, as in RFC 9110. The
  idempotency draft uses it for a request still in flight, which this service
  never answers (ADR-0026), so a client written for the draft's meaning may retry
  a stored refusal. The `type` tells them apart, and such a retry changes nothing.
- A merchant vouches for the PSP's authorization until phase 4, so the ledger can
  record a capture on a merchant's word alone. Phase 4 closes this.
- A payment cannot reach `settled`, `failed` or an expired `voided` through the API
  until phase 4. Tests reach those states through the library.

## Amendments

- 2026-11-04: A 5xx's answer says nothing of its cause, so the server logs it, which
  the Decision left out, although ADR-0024 and ADR-0027 assume logs. The project's owner
  chose `tracing` and `tracing-subscriber` over one `eprintln!`, because they are what
  Rust services use. They cost 4 crates, the subscriber and what it needs, since sqlx
  builds `tracing` already. A line holds the status and the error's chain, and nothing
  of the request, so no header can reach it. A panic's message comes from Rust's own
  hook. A request refused for its own sake logs nothing, because anyone can send one.
- 2026-11-04: Answers that the table left out, each a `problem+json` that is not stored:
  a path that is no route is a 404, and a wrong method is a 405 with `Allow`. An ID in a
  path has one spelling, and any other is a 404, as no payment is. A retry that spelled
  the ID differently would otherwise read as another request.
- 2026-11-05: A body above axum's default limit of 2 MB is a 413, also a problem that is
  not stored. A body is held in memory for its fingerprint, so a limit has to exist, and
  this one is far above any body that a route takes.
- 2026-11-05: What the operator's routes leave open. `POST /api-keys/{id}/revoke` answers
  `api_key_id`, because a stored answer has to be JSON (ADR-0026), and revoking a revoked key
  is a 200 like the first, because what the request asks for already holds. A fee above 10,000
  basis points is a malformed body, since it cannot be parsed into a rate. `OPERATOR_KEY_SHA256`
  is 64 lowercase hex digits, and the binary stops at once on anything else, without repeating
  what it read, because the likeliest mistake is pasting the secret itself.

## References

- R. Fielding, M. Nottingham and J. Reschke, [RFC 9110: HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html) (IETF, 2022), sections 15.5.10 (409) and 15.5.21 (422).
- M. Nottingham, E. Wilde and S. Dalal, [RFC 9457: Problem Details for HTTP APIs](https://www.rfc-editor.org/rfc/rfc9457.html) (IETF, 2023).
- jgallagher, [Add server-side configuration to `tokio::spawn` endpoint futures](https://github.com/oxidecomputer/dropshot/issues/695) (oxidecomputer/dropshot #695, 2023).
