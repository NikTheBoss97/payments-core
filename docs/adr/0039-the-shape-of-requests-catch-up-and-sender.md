<!-- Author: Nicola Antonello -->

# ADR-0039: The shape of requests, the catch-up and the sender

- Status: accepted
- Date: 2026-11-08

## Context

ADR-0028, ADR-0031, ADR-0032 and ADR-0034 decide what the library does in phase 4: it
records a merchant's request and seals its 202 under the key, catches a payment up with
the PSP's history, and sends each request until the PSP holds it. They leave the shape
of that code open, and the code that exists shapes it.

`apply_in` already applies one command's step, posting and record inside a transaction
that its caller has begun and locked (ADR-0019), so several facts can share one unit of
work. A key's seal renders only a step's answer or a refusal of the table of moves
(ADR-0026), which a recorded request is neither. And some refusals at request time,
such as a capture while another is in flight, are about requests, not about moves.

## Options

How much one catch-up commits:

| Option | Why not |
|---|---|
| **Every new fact, in one locked transaction** | Chosen |
| One transaction per fact | Smaller commits, but more of them, and each repeats the check of the prefix |

Who makes the IDs of the steps and postings that a catch-up writes:

| Option | Why not |
|---|---|
| **The catch-up** | Chosen |
| Its caller, as every command's caller does (ADR-0005) | A command's ID is its identity, which a retry compares (ADR-0019). Here a fact's identity is its position, so nothing would ever compare these IDs |

How a request's answer is sealed:

| Option | Why not |
|---|---|
| **The seal renders one enum of outcomes: a step applied, a request recorded, or a refusal** | Chosen |
| A second seal, for requests | Two paths through one store of answers |

What the sender does when its lookup finds a request at the PSP:

| Option | Why not |
|---|---|
| **It records the request as acknowledged, and stops** | Chosen |
| It goes on looking until the PSP decides | Reads that learn nothing that a `2xx` would not have told it |

How the test of I9 shows that it can fail (ADR-0034):

| Option | Why not |
|---|---|
| **Its control resends through the PSP client, with no lookup** | Chosen |
| A switch in the sender that turns the lookup off | A switch in production code whose only use is to disable a safety check |

## Decision

- **A catch-up commits once.** It reads the history outside any transaction, takes the
  payment's lock, checks the prefix, applies every new fact through `apply_in`, writes
  the facts, decisions and links of ADR-0037, and commits through the one function of
  ADR-0014. A crash leaves every new fact applied or none.
- **The catch-up makes its own IDs.** A retry after an unknown `COMMIT` reads the history
  and the facts held again, under the lock, and applies only the facts that are missing.
  A fact cannot be applied twice, because its position is a key (ADR-0037), and the lock
  puts the attempts in order (ADR-0018). This is ADR-0019's identity first, at the level
  of facts, and ADR-0015's rule of the same IDs stays with `post`, which the catch-up
  never retries by itself.
- **The seal's enum.** A seal renders `Decided`: a step applied, a request recorded, or a
  `Refusal`. A refusal is one of the table of moves, an amount above its limit, or a
  conflict with a request already recorded: an authorization requested already, or a
  capture or a void in flight. Every refusal is final, and stored under the key
  (ADR-0026).
- **The answers at request time.** A conflict is a 409, like a move that the table
  refuses: a second authorization, a capture or a void while another capture or void is
  in flight, and a cancellation once an authorization has been requested. An amount is a
  422: a refund above what stays refundable once the refunds in flight are counted. A
  request is in flight until a fact decides it (ADR-0037).
- **A lookup that finds the request** records it as acknowledged, as a `2xx` would have,
  and the sender stops. The same read serves the catch-up (ADR-0031).
- **I9's control** resends through the PSP client with no lookup, after the fake forgot
  its key, and sees the fake act twice: the test can fail.
- **Where a catch-up stops** (ADR-0031): `history_changed` when the facts held are not
  the PSP's first facts; `no_place` for a fact that no step can take from the payment's
  status, or of a kind that the service does not know; `contradiction` for a decision on
  a request that the service never sent, or one whose kind or amount disagrees with it.
- **The sweep** catches up the payments that have a request not yet decided, an
  authorization past its `expires_at`, or a capture not yet settled. Its server sets the
  cadence.

## Consequences

- One catch-up holds the payment's lock while it applies every new fact, which phase 8
  measures.
- The tests of the catch-up compare steps and facts by kind and position, not by ID.
- Phase 3's routes render `Decided` from the start, so their answers do not change. The
  cancellation route gains a refusal, and the API document a problem type for each
  conflict.

## References

- No source beyond the records that it builds on: ADR-0005, ADR-0014, ADR-0015,
  ADR-0018, ADR-0019, ADR-0026, ADR-0028, ADR-0031, ADR-0032, ADR-0034 and ADR-0037.
