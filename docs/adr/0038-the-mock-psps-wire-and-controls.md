<!-- Author: Nicola Antonello -->

# ADR-0038: The mock PSP's wire format and controls

- Status: accepted
- Date: 2026-11-08

## Context

ADR-0029 decides what the mock PSP is: a configurable fake in the test process, over real
HTTP, with two routes, scripted through a Rust handle and without a fake clock. ADR-0030
to ADR-0032 decide how it signs, what its history holds and how it answers. They leave
open how a test drives the fake's decisions, how strictly the fake keeps its own state,
what the wire calls a fact, and the event types of its notifications.

The server shares no code with the fake (ADR-0029), so whatever the wire says, the
server's catch-up reads it with types of its own.

## Options

How a test drives the fake's decisions:

| Option | Why not |
|---|---|
| **The fake holds each request until the test decides it** | Chosen |
| A standing policy, such as "approve everything", with per-request overrides | Shorter tests, but a fact appears the moment its request arrives, so a test cannot step in between |
| A queue of outcomes, taken in arrival order | Compact, but it ties outcomes to arrival order, which races when the sender sends several requests at once |

How strict the fake's own model is:

| Option | Why not |
|---|---|
| **It keeps each payment's state, and refuses a decision that no PSP could make; one method appends any fact** | Chosen |
| It records whatever the test scripts | A mistaken test would quietly build a history that no PSP could produce |

What the wire calls a fact:

| Option | Why not |
|---|---|
| **A PSP's own words, in the past tense** | Chosen |
| The kinds of the service's tables, copied through | Simpler, but nothing would test the translation that a real PSP needs |

The event types of notifications:

| Option | Why not |
|---|---|
| **One type, `payment.updated`** | Chosen |
| One type per kind of fact, such as `payment.captured` | More information, but it invites reading a decision out of a notification, which decides nothing (ADR-0031) |

## Decision

- **Holding.** A fake for a test holds every request that it takes until the test
  approves it, approves it for less, declines it or refuses it. Settling and expiring
  are steps that the test takes, and so are delivering, pruning keys and the binary's
  timers, each with any time that the test likes. Every fact then appears at a moment
  that the test chose (Meszaros's Configurable Test Double). The binary runs the same
  code with a standing rule: approve at once, or refuse what it cannot approve, and hold
  what it can do neither.
- **A self-consistent fake.** The fake keeps each payment's state on its own side and
  panics on a decision that a real PSP could not make, such as capturing more than was
  authorized: that is a mistake in the test. One method appends any fact unchecked, for
  the tests of a history that the service cannot place, such as a capture that fails
  after it succeeded (ADR-0031).
- **The wire's words.** A fact's kind is the past tense of an outcome: `authorized`,
  `declined`, `captured`, `capture_refused`, `voided`, `void_refused`, `expired`,
  `settled`, `refunded` and `refund_refused`. The catch-up maps each to the step that
  it makes, or to a refusal (ADR-0037).
- **Notifications.** Each fact is announced by one notification of type
  `payment.updated`, in the payload structure of Standard Webhooks: `type`,
  `timestamp`, and `data` with the payment's ID alone.
- **Shapes.** A request is `{id, payment_id, kind, amount, currency}`, with the
  service's API key as `Authorization: Bearer`, and unknown fields refused. The fake
  answers 202 `{id, status: "received"}` for a request that it holds or a copy that its
  cache knows, 422 for an ID with other content (as ADR-0026 answers a reused key), 400
  for a malformed request, and 401 without the key. A history is `{payment_id, facts,
  requests}`; a fact carries its `id`, `position` and `kind`, and, where its kind has
  them, `request_id`, `amount` and one of `expires_at`, `captured_at` or `refunded_at`,
  in RFC 3339 at whole microseconds (ADR-0022).
- **IDs.** The fake's own IDs are `fct_` or `msg_` and the 32 hex digits of a UUIDv7, of
  the shape that ADR-0037 allows, so that a restarted binary never repeats one.
- **Deliveries.** Each attempt is signed with the time that it is made, and waits 15
  seconds for an answer, the least that the specification recommends. A failed attempt
  is due again on the specification's example schedule, with up to a fifth of jitter,
  and never before a `Retry-After`; the fake gives up after the tenth.
- **The binary** reads `MOCK_PSP_ADDRESS`, `MOCK_PSP_WEBHOOK_URL`, `MOCK_PSP_API_KEY`,
  `MOCK_PSP_SIGNING_KEYS` and `MOCK_PSP_SETTLE_AFTER_SECONDS`, and every second delivers
  what is due, settles the captures older than its delay, lets authorizations lapse at
  their expiry, and prunes the keys a day old. It repeats nothing that it reads.

## Consequences

- A test that wants a fact must say so: more lines than a standing policy, and no fact
  that the test did not choose.
- The catch-up carries a mapping from the wire's words to the service's kinds, which
  its own tests break.
- The fake's tests pin each shape byte for byte, and check every signature with HMAC
  code of their own as well as the specification's test vector.
- The binary's records live in memory and end with it: it serves local runs, not tests.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): the payload structure, event types, the example retry schedule and request timeouts.
- Gerard Meszaros, [xUnit Test Patterns: Refactoring Test Code](http://xunitpatterns.com/) (Addison-Wesley, 2007): Configurable Test Double and Fake Object.
