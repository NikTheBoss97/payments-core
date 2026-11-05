<!-- Author: Nicola Antonello -->

# Correctness: the API

The [README](../../README.md#correctness) lists every invariant of the project; this page shows how the API's are tested.

Phase 3 guarantees that one idempotency key produces at most one effect, and that a merchant
can read or change only its own payments. Each is enforced twice. A key's record commits in the
same transaction as its effect, and a primary key refuses a second one. A route compares a
payment's merchant with the credential's, and row-level security refuses the rest, under a login
role that sees nothing until a transaction names its merchant. Under failure, a client's one rule
is to send the request again under its key.
[`docs/DESIGN.md`](../DESIGN.md#invariants) states how each invariant is enforced,
[ADR-0023](../adr/0023-the-http-api.md) to [ADR-0026](../adr/0026-idempotency-keys.md) say why,
and [`docs/api.md`](../api.md) says what a client can rely on.

| | Invariant | Tested by |
|---|---|---|
| I4 | One idempotency key produces at most one effect. | Copies of every `POST` racing, which must give one effect and one answer, through the router ([`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs)) and on the real binary's own threads, where two tests count every copy waiting at the same moment before they let any go ([`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs)). The backend killed at every statement of every route, and a `COMMIT` that commits and loses its session ([`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs)). The server process killed with commands in flight, and with an answer that nobody read ([`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs)). A late duplicate of a refusal, each reuse of a key, two merchants holding one key, and every `POST` refusing a missing key ([`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`keyed_commands.rs`](../../crates/payments-core/tests/keyed_commands.rs)). The tables' own rules, in raw SQL ([`idempotency_schema.rs`](../../crates/payments-core/tests/idempotency_schema.rs)). |
| I7 | A merchant can read or change only its own payments. | Every route that takes a payment's ID, tried with another merchant's key, answers exactly what no payment answers ([`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_reading.rs`](../../crates/payments-core/tests/api_reading.rs)). Each guard broken alone: row-level security turned off, so that only the comparison is left, and a route that forgets the comparison, so that only the policies are left ([`api_isolation.rs`](../../crates/payments-core/tests/api_isolation.rs)). Every command and read as the tenant role, across merchants and on one shared connection ([`tenant_commands.rs`](../../crates/payments-core/tests/tenant_commands.rs), [`tenant_isolation.rs`](../../crates/payments-core/tests/tenant_isolation.rs)). Catalog tests that pin what row-level security covers, every policy's text and each role's privileges ([`catalog.rs`](../../crates/payments-core/tests/catalog.rs)). |

The API's flows also keep the invariants below them. Each test here that moves money ends with
every currency summing to zero (I1) and no posting without a cause, and most also check the
money against balances derived from the entries (I3), whatever was sent twice.

The tests also attack the guards themselves:

- **A secret stored.** After a key is issued, retried, used and revoked, every row of every table is read as text and searched for the merchant's and the operator's secrets. The controls: the first answer did hold one, and the same scan finds the merchant's and the key's IDs. The type of a stored answer has no field for a secret ([`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs)).
- **A secret logged.** CodeQL has no model of `tracing`, the logger that the server uses ([ADR-0027](../adr/0027-supply-chain-checks.md)), so tests read the real binary's log instead, after a whole flow, after races and after failures, and search it for every secret and for the header. A secret also has no `Display`, which a `compile_fail` doctest pins, and a `Debug` that hides it ([`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs), [`credentials.rs`](../../crates/payments-core/src/credentials.rs)).
- **A credential that is nearly right.** 14 bad ones on each of the operator's routes, and a dozen on a merchant's, including a key's digest, which is what the table and the environment hold. All get one answer, and nothing runs ([`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs), [`api_credentials.rs`](../../crates/payments-core/tests/api_credentials.rs)).
- **Two spellings of one thing.** Another spelling of a real ID reads as no such thing, on a payment's routes and the operator's, so that one thing has one path ([`api_reading.rs`](../../crates/payments-core/tests/api_reading.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs)).
- **The middleware.** A control shows that without it a client that hangs up cancels its handler ([`api_requests.rs`](../../crates/payments-core/tests/api_requests.rs)), and hang-ups on real commands fail if a route is left out of it ([`api_hang_ups.rs`](../../crates/payments-core/tests/api_hang_ups.rs)).

## Failures the API is tested against

| Failure | What the client gets | Test |
|---|---|---|
| Copies of any `POST` race each other | One effect and one answer, the same to every copy. An issue shows its secret to exactly one | [`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs), [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| Every copy waits behind a lock or a key's index at the same moment, then all go | The same, on the server's own threads, with no error in its log | [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| A retry after a success or a stored refusal | The same status, bytes and headers, and nothing runs | [`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs) |
| A refused request's copy arrives after the payment has moved on | The refusal, which stays final. Under a new key the request applies | [`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs) |
| A key is used for another body, route or ID | `422`, and the first answer stands | [`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs) |
| A client hangs up while its command waits | No answer. The command lands, once, and the retry finds the answer | [`api_hang_ups.rs`](../../crates/payments-core/tests/api_hang_ups.rs) |
| The backend dies before or after any write, or during `COMMIT`, on any route | `503`. Nothing written in its transaction stays, the retry applies once, and a late copy gets that answer | [`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs) |
| The backend dies while a request waits for a lock or a key | `503`, and the retry applies once | [`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs) |
| A `COMMIT` commits, then the session dies | `503`. The retry finds the answer and moves nothing. A kill that lands too early leaves nothing, and the retry applies. The test fails unless some kills hit | [`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs) |
| A refusal dies as it is stored | `503`. Nothing was told or stored, so it is decided again, and the same request can now apply | [`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs) |
| The server process is killed while commands wait | Nothing stays, with no clean-up. After a restart the retry applies once | [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| The server process is killed after a command committed, with its answer unread | After a restart the retry gets the answer, and moves nothing | [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| A key cannot be looked up | `503`, and nothing runs | [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs) |
| A credential cannot be looked up | `503`, never `401`, so that a client never concludes that a good key is bad | [`api_credentials.rs`](../../crates/payments-core/tests/api_credentials.rs) |
| The database refuses a statement | `500`, which says nothing of its cause, and the key stays free | [`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs) |
| A handler panics | `500`, which says nothing of its cause | [`api_errors.rs`](../../crates/payments-core/tests/api_errors.rs), [`api_requests.rs`](../../crates/payments-core/tests/api_requests.rs) |
| A body is malformed, too large, or fails on its way in | `400` or `413`, and the key stays free | [`api_commands.rs`](../../crates/payments-core/tests/api_commands.rs), [`api_idempotency.rs`](../../crates/payments-core/tests/api_idempotency.rs), [`api_operator.rs`](../../crates/payments-core/tests/api_operator.rs) |

A killed process leaves its sessions behind for as long as they wait: [PostgreSQL meets a lost
client](https://www.postgresql.org/docs/18/runtime-config-connection.html#GUC-CLIENT-CONNECTION-CHECK-INTERVAL)
only when it next touches the socket, which a test observes. Each then rolls back as soon as it
gets what it waited for, and until then `lock_timeout` bounds the wait.

## What these tests do not show

- **A process killed at any chosen instruction.** The failure model assumes that it can happen. The tests kill the server while commands wait, and after one committed. Between those, what stands behind the answer is that one `COMMIT` decides both the effect and its record, which the tests that kill a session during `COMMIT` exercise.
- **That the operator's comparison takes constant time.** A test cannot tell `subtle` from `==`, so that rests on `subtle`'s design. ADR-0024 also notes that an equality test on a digest reveals nothing useful.
- **What is outside the failure model.** A stolen key used before it is revoked ([ADR-0024](../adr/0024-authentication.md)), and SQL written on purpose, as the tenant role, that names another merchant, which [`tenant_isolation.rs`](../../crates/payments-core/tests/tenant_isolation.rs) pins as the known limit.

## What these tests have found

- **A second spelling of an ID.** axum decodes a path before an extractor sees it, so `/payments/%30...` reached the payment, against the rule that an ID has one spelling. A retry spelled that way got a `422` under its key, so nothing could apply twice. A test of the operator's routes found it after two pull requests had shipped the rule.
- **A refused statement that read as something to retry.** `42501` fell through the classifier to `NotConfirmed`, which told a caller to retry what can never succeed. Designing the tenant role found this rather than a test ([ADR-0025](../adr/0025-tenant-isolation.md)), and tests now pin it.
- **A clean scan that said little.** Plants through axum handlers showed that CodeQL loses a request's body at `serde_json` and has no model of `tracing`, so its silence says little about this server, and the tests on the real binary carry that weight instead ([ADR-0027](../adr/0027-supply-chain-checks.md)).
- **Tests that tested too little.** With `register_merchant` always storing 250, nothing noticed, because every test registered at 250. With the lookup after a duplicate key naming no merchant, nothing noticed either, because no test reached it as a merchant. Races and the `503` and `500` mappings were first tested on only one or two commands. Deliberate breakages, or planning them, found each of these, and each has a test now.
- **A test that had never failed.** Counting what each new test caught under those breakages showed one at zero, which passed before its code existed and under every breakage after. It got a breakage of its own, and the count is now part of how a pull request is checked.
