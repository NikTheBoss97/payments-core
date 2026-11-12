<!-- Author: Nicola Antonello -->

# ADR-0046: The four routes record requests

- Status: accepted
- Date: 2026-11-12

## Context

ADR-0043 and ADR-0045 give the last of phase 4's server pull requests the switch:
`authorize`, `capture`, `refund` and `void` record a request to the PSP and answer 202,
the route hands the request over to the sender's task after the commit, and phase 3's HTTP
tests, the API's document and the quickstart move onto the fake. ADR-0028 and ADR-0033 fix
the 202: its body holds `request_id` and `payment_id`, its `Location` is built from them,
and its `Retry-After` is fixed, so a replay is identical. ADR-0043 fixes the handover: a
channel of 1,024 IDs, tried once after the commit, and a step of the sender at each
handover.

Writing it settles what those records leave open. The PSP's business times now come from
its facts, so the bodies lose theirs. The store keeps an answer as a status and a body, so
a replayed 202 has to get its headers back. Several handovers can arrive together. And the
tests that killed a command, or ran it on the binary, now have a request to reach.

## Options

How a replayed 202 gets its `Location` and `Retry-After`:

| Option | Why not |
|---|---|
| **Built from the stored body each time an answer is given** | Chosen |
| Stored with the answer | A migration of both tables of answers, to keep what the body already says |

What a burst of handovers costs:

| Option | Why not |
|---|---|
| **The task takes every ID already queued, then takes one step** | Chosen |
| One step for each handover | A step searches the requests that wait, so N handovers make N searches of the same rows |

## Decision

- **The bodies.** `authorize`, `capture` and `refund` take `{"amount": n}`, and `void`
  takes none, as before. The PSP's business times come from its facts (ADR-0028,
  ADR-0031), so a body that still names `expires_at`, `captured_at` or `refunded_at` is a
  400 `malformed-body`, as any unknown field is.
- **The 202.** A recorded request is answered 202 with `{"request_id", "payment_id"}`, the
  answer that its key stores. `Location: /payments/{payment_id}/requests/{request_id}` and
  `Retry-After: 1` are built from that body each time the answer is given, so a replay is
  the same status, headers and bytes. Refusals keep their stored answers (ADR-0028,
  ADR-0039, ADR-0042).
- **The handover.** After `record_request` commits, the route tries to send the request's
  ID, only when this attempt recorded it (ADR-0043). On a handover, the sender's task takes
  every ID already queued, hands each to the `Sender`, and takes one step. A full channel
  drops the ID, and the next step's search finds the request, which is looked up before it
  is sent (ADR-0042).
- **The tests.** Phase 3's HTTP tests reach each state through the fake (ADR-0043): the
  route records the request, a step of a `Sender` of the fake's sends it, the fake decides,
  and a catch-up applies the decision. The crash tests kill a command around its request's
  row and in its commit, as they killed it around its step's. The binary's tests run it
  next to a fake that delivers its notifications to the binary, and wait, bounded, for each
  request to apply.
- **The quickstart** starts the fake's binary beside the server, with the same keys, and
  waits for each request to read `applied` before it sends the next.

## Consequences

- A merchant's command answers with where its request stands, and the merchant follows it
  through the request route until phase 5 publishes events (ADR-0033).
- The parsing of a merchant's business time goes with the bodies that carried one. The
  library's step commands keep their test callers until `LG007` refuses a step of the
  PSP's without its fact (ADR-0034).
- Tests: each command's 202, replayed with the same headers and bytes; a body with a time
  refused; the handover tried once, and never on a replay; a step at a handover without a
  tick; copies of a command racing, one request; a command killed around its request's
  row or in its commit, and its retry; a client that hangs up; the whole flow on the binary
  next to a fake; a request whose handover was lost, sent after a restart with a lookup
  first.

## References

- The records that it builds on: ADR-0028, ADR-0031, ADR-0033, ADR-0034, ADR-0039,
  ADR-0042, ADR-0043 and ADR-0045.
