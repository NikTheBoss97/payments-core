<!-- Author: Nicola Antonello -->

# Correctness: the PSP

The [README](../../README.md#correctness) lists every invariant of the project; this page shows how the PSP's are tested.

Phase 4 guarantees that only the PSP's word moves a payment, and that a request sent to the PSP
takes effect there at most once. Facts come only from the PSP's history, which a catch-up reads
and applies in its order, checking each time that what the service holds is the history's first
facts. Four guards hold that: a type that only the catch-up makes, the merchant role's privileges
and policy, the chain of facts, and a check at `COMMIT`, `LG007`, that ties every step of the
PSP's to its fact. A request carries one ID from its record to the PSP, and the one sender looks
it up in the PSP's records before every resend, so a PSP that forgot the key still acts once.
[`docs/DESIGN.md`](../DESIGN.md#invariants) states how each invariant is enforced,
[ADR-0028](../adr/0028-requests-to-the-psp.md) to
[ADR-0048](../adr/0048-the-fact-check-and-the-catch-ups-own-commands.md) say why, and
[`docs/api.md`](../api.md) says what a client can rely on.

| | Invariant | Tested by |
|---|---|---|
| I8 | Only the PSP's word moves a payment: after its creation, every step but a cancellation comes from the PSP's history, once and in its order. | Each kind of fact applied with its step, record and posting, only after the facts held, and each fact once when copies of a catch-up race ([`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs)). A property test that delivers a history's notifications duplicated, reordered and partly lost, after which every fact has reached its payment once and in the PSP's order ([`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs)). A history that changes, a fact that has no place, and a decision that contradicts the requests, each of which stops the payment at that position, and a stopped payment that takes no more facts or requests ([`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs), [`recording_requests.rs`](../../crates/payments-core/tests/recording_requests.rs), [`sending.rs`](../../crates/payments-core/tests/sending.rs)). Random histories of three merchants' requests and the PSP's decisions against a model ([`payment_model.rs`](../../crates/payments-core/tests/payment_model.rs)). The backend killed around each statement of the catch-up ([`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs)). Each guard broken alone, below. |
| I9 | A request sent to the PSP takes effect there at most once. | The fake acts, loses its reply and forgets the key, and the sender restarts: one effect. The control turns the lookup off and sees two ([`sending.rs`](../../crates/payments-core/tests/sending.rs)). A lookup that finds the request, which records it and sends nothing, two senders racing, which leave one request and one answer, and every other answer read as an unknown outcome that the next attempt looks up ([`sending.rs`](../../crates/payments-core/tests/sending.rs)). Copies of one request racing, which record it once ([`recording_requests.rs`](../../crates/payments-core/tests/recording_requests.rs)). One decision per request under a unique key, in raw SQL ([`facts_schema.rs`](../../crates/payments-core/tests/facts_schema.rs)). The routes that record requests killed at every statement, the server killed while requests wait, after which each is recorded once, and a server that sends, as it starts, what it finds recorded ([`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs), [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs)). |

The PSP's flows also keep the invariants below them. The catch-up's flow test and its property
test end with every currency summing to zero (I1) and no posting without a cause, and the tests of
captures and refunds, and of a catch-up killed, check the money against balances derived from the
entries (I3).

The tests also attack the guards themselves:

- **The merchant's credential.** As the merchant's role, on its own payment so that isolation lets the row through, each step of the PSP's is refused by `payment_transitions_tenant_steps`, which PostgreSQL names, and the system role then takes the same row. A posting's header, a ledger line and each record are refused for want of a privilege ([`tenant_steps.rs`](../../crates/payments-core/tests/tenant_steps.rs)).
- **A step without its fact.** As the system role, each step of the PSP's, with its record and its posting but no fact, is refused at `COMMIT` with `LG007`, and commits with its fact. A catch-up whose link a trigger loses fails as a bug and leaves nothing ([`facts_schema.rs`](../../crates/payments-core/tests/facts_schema.rs), [`authorizing_payments.rs`](../../crates/payments-core/tests/authorizing_payments.rs)).
- **A fact out of place.** A gap in the chain, a second fact at a position, a fact of another kind or amount than its step, and a second decision for a request, each refused by its key, in raw SQL ([`facts_schema.rs`](../../crates/payments-core/tests/facts_schema.rs)).
- **The type that applies a fact.** A `compile_fail` doctest shows that code outside the crate cannot make a `PspWord`, which the commands of the PSP's moves take ([`catch_up.rs`](../../crates/payments-core/src/catch_up.rs)).
- **A forged notification.** A signature under another key, a changed body, a timestamp outside the window, and an ID or timestamp of the wrong shape each get the same `400` and record nothing ([`notifications.rs`](../../crates/payments-core/src/notifications.rs), [`api_webhooks.rs`](../../crates/payments-core/tests/api_webhooks.rs)).
- **A history in the wrong shape.** A `200` that is not the payment's history and a body above the bound are unreadable, and a failed read applies nothing ([`psp_client.rs`](../../crates/payments-core/tests/psp_client.rs), [`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs)).
- **What behaviour cannot show.** Catalog tests pin the merchant role's privileges, its policies, and the fact check's trigger with its `ENABLE ALWAYS` and `search_path` ([`catalog.rs`](../../crates/payments-core/tests/catalog.rs)).

## Failures the PSP is tested against

| Failure | What happens | Test |
|---|---|---|
| A notification comes twice, late, out of order, or never | Every fact reaches its payment once and in the PSP's order. The sweep catches up a payment whose notification never came | [`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs), [`recording_notifications.rs`](../../crates/payments-core/tests/recording_notifications.rs), [`api_webhooks.rs`](../../crates/payments-core/tests/api_webhooks.rs) |
| A notification is forged or spoiled | `400`, and nothing is recorded | [`api_webhooks.rs`](../../crates/payments-core/tests/api_webhooks.rs) |
| A notification names a payment that the service does not hold | Recorded, flagged, answered `202`, and no catch-up starts | [`api_webhooks.rs`](../../crates/payments-core/tests/api_webhooks.rs), [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| The backend dies before or after a notification's record, or during its `COMMIT` | `503` with `Retry-After`, and nothing is recorded. The PSP's next delivery is recorded once | [`api_webhooks.rs`](../../crates/payments-core/tests/api_webhooks.rs), [`recording_notifications.rs`](../../crates/payments-core/tests/recording_notifications.rs) |
| A notification's catch-up fails, or finds no permit | The answer does not wait for it, and the sweep catches the payment up later | [`api_webhooks.rs`](../../crates/payments-core/tests/api_webhooks.rs), [`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs) |
| The PSP never answers, or is down | Timed out, or no answer: an unknown outcome, which the next attempt looks up | [`psp_client.rs`](../../crates/payments-core/tests/psp_client.rs), [`sending.rs`](../../crates/payments-core/tests/sending.rs) |
| The PSP's reply to a send is lost | The PSP holds the request. The next lookup finds it, records it as acknowledged and sends nothing | [`psp_client.rs`](../../crates/payments-core/tests/psp_client.rs), [`sending.rs`](../../crates/payments-core/tests/sending.rs) |
| The PSP forgets the idempotency key and keeps its records | One effect at the PSP, and two with the lookup turned off | [`sending.rs`](../../crates/payments-core/tests/sending.rs) |
| The PSP asks to be called later, or is down for hours | `Retry-After` holds the next attempt back. One budget, shared with the sweep, bounds the retries of an outage, and a request left unacknowledged for an hour is flagged | [`sending.rs`](../../crates/payments-core/tests/sending.rs), [`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs) |
| Two senders, or copies of a catch-up, race | One request and one answer; each fact applied once | [`sending.rs`](../../crates/payments-core/tests/sending.rs), [`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs) |
| The PSP's history changes, holds a fact that has no place, or decides a request that the service never sent | The payment stops at that position for an operator. Nothing after it is applied, its requests wait, and a new one is refused | [`catching_up.rs`](../../crates/payments-core/tests/catching_up.rs), [`recording_requests.rs`](../../crates/payments-core/tests/recording_requests.rs), [`sending.rs`](../../crates/payments-core/tests/sending.rs) |
| The backend dies before or after any write of a catch-up, or during its `COMMIT` | Nothing written in its transaction stays, and the next catch-up applies the fact once | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A catch-up's `COMMIT` commits, then the session dies | The next catch-up finds the fact held, and money moved once | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A route that records a request dies at any statement | `503`. Nothing stays, and the retry records the request once | [`api_crashes.rs`](../../crates/payments-core/tests/api_crashes.rs), [`keyed_crashes.rs`](../../crates/payments-core/tests/keyed_crashes.rs) |
| The server starts with requests recorded but not sent, and facts that no notification announced | Its first step sends what is recorded, and its first pass catches up what waits for the PSP | [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| The server is killed while requests wait | Nothing stays, and after a restart each is recorded once | [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| SQL run as the system role commits a step of the PSP's without its fact | `LG007` at `COMMIT`, which proves a rollback | [`facts_schema.rs`](../../crates/payments-core/tests/facts_schema.rs) |
| SQL run as the merchant's role writes a step of the PSP's, a record or a ledger line | Refused by the policy, or for want of a privilege | [`tenant_steps.rs`](../../crates/payments-core/tests/tenant_steps.rs) |

## What these tests do not show

- **That the signature check takes constant time.** It goes through `Mac::verify`, which compares through `CtOutput`, as read in `digest` 0.11.3's source. A test cannot show timing.
- **A false history.** A PSP whose history is false, or that changes what it already reported, is outside the [failure model](../DESIGN.md#failure-model). A catch-up stops a payment whose history changes under it, but a false history that never changes looks true.
- **An answer that cannot be recorded.** No test makes the insert of a send's answer, or its `COMMIT`, fail. The request then stays with the sender, whose next lookup finds what the PSP holds.
- **A full channel of handovers.** No test fills its 1,024 places. A dropped handover costs a lookup, which the tests of a restart show.
- **The flag across restarts.** A request is flagged an hour after its first attempt in this process, so a process restarted more often than hourly would never flag it.
- **The order of the checks at `COMMIT` across rows.** PostgreSQL 18.6 ran them in the order that their rows were written, which no documentation states. The tests rely only on the order of one row's checks, which [CREATE TRIGGER](https://www.postgresql.org/docs/18/sql-createtrigger.html) states.

## What these tests have found

- **A capture that the sweep forgot.** The sweep left out a capture refunded in full before it settled, which the PSP will still settle. Its test failed first, and the sweep now waits for a refunded payment's settlement too.
- **A rule that held by luck.** A settlement after a full refund is recorded without a step only if the PSP had not settled the payment before. No test held a payment settled twice, so nothing checked that, until a planted bug showed it.
- **What the merchant could not read.** A request on a stopped payment is a `409` that the merchant's role must read under the payment's lock, and an applied refund's time is reached only through the link from its fact to its step. [ADR-0037](../adr/0037-storing-requests-and-facts.md) had kept both from that role. Writing the code found each, and a migration grants each ([ADR-0042](../adr/0042-the-senders-budget-work-and-stopped-payments.md), [ADR-0045](../adr/0045-following-a-request-and-the-switch-in-two.md)).
- **A case that no status shows.** The model's histories never settled a payment refunded in full, so a catch-up that stopped one went unseen. 2 of 400 histories do, so the model now runs 80 and fails unless one of them does.
- **A guard reached before the rule under test.** A first draft of `LG007` failed 36 tests that commit a step of the PSP's without its fact, some of them attacks on another check that the draft reached first. The fixtures now write each step's fact, so that each test still breaks one rule ([ADR-0048](../adr/0048-the-fact-check-and-the-catch-ups-own-commands.md)).
- **A missing link that only crashes noticed.** A catch-up that wrote no link from a step to its fact was caught only by the crash tests, which count rows. `LG007` now refuses it at `COMMIT` in every test that applies a fact.
- **Tests that tested too little.** Counting what each new test caught under planted bugs left tests at zero more than once. Five of the PSP client's had caught nothing, and two lines had no test watching them: a base's final slash and the sensitive mark on the key's header. Ten tests moved off the PSP's step commands had caught nothing either, nine of them on the path that every command shares. Listing in advance the bugs that no test would catch added the tests of the retries' growing waits, of the oldest request going first, and of a lookup's `Retry-After`, each the only test that caught its bug.
