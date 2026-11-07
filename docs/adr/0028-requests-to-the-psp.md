<!-- Author: Nicola Antonello -->

# ADR-0028: Requests to the PSP

- Status: accepted
- Date: 2026-11-07
- Supersedes: in ADR-0023's Decision, the rows of `authorize`, `capture`, `refund` and
  `void` in the table of routes, and the bullet "What stays off HTTP"; in ADR-0026's
  Decision, the list of final answers, which gains a recorded request.

## Context

Until now a merchant reported what the PSP decided, and the ledger took its word
(ADR-0023). Phase 4 sends the merchant's requests to the PSP and records what the PSP
decides. ADR-0026 left the shape of that to this phase: "Phase 4's calls to the PSP
must come after a commit and carry a stable ID of their own, and phase 4 decides how."

Our database and the PSP share no transaction, so there is always a moment when one
side has acted and the other does not know it yet. A crash or a lost reply in that
moment is the failure model's "PSP timeouts with unknown outcome". Exactly-once
delivery over such a link is built from at-least-once delivery and a receiver that
drops repeats: the end-to-end argument of Saltzer, Reed and Clark, and Lampson's rule
of a unique ID per action, remembered once the action is done. So every request needs
a stable ID, the PSP must drop copies of an ID it has seen, and we must be able to send
the same ID again after a failure. Durable-execution systems meet the same limit:
Temporal warns that "Because Activities may be retried, these functions may be
executed more than once".

PSPs differ in what they answer at once. Adyen answers a capture with "received" and
sends the result later, and a capture can still fail "a few days after you submitted
the capture request". Stripe answers at once and protects retries with idempotency
keys, which it may prune after 24 hours: "We generate a new request if a key is reused
after the original is pruned."

## Options

How a merchant's command reaches the PSP:

| Option | Why not |
|---|---|
| **Record the request with the merchant's key, answer 202, and send it after the commit; the PSP's decision comes later** | Chosen |
| Call the PSP during the request, under an ID derived from the idempotency key, and answer with its decision | A cancel can race an authorization that the PSP then holds; a PSP that is down turns every request into a 503; and once the PSP has forgotten a key, any call may act twice unless every call looks up first, since nothing tells a retry from a first attempt |
| Three phases, as Airbnb's Orpheus: record the request, call, record the answer, and sweep up calls left in the middle | Correct, but a copy of the request that arrives during the call finds a request with no answer, which needs a lease or an "in progress" answer that ADR-0026 chose not to have |

How each meets the failures it must survive:

| What happens | Record, then send | Call during the request | Three phases |
|---|---|---|---|
| The PSP acts and its reply is lost | the sender sends the same ID again, after a lookup | 503, and the merchant's retry calls again | the sweep or a retry sends again |
| We crash right after the PSP acts | the sender sends again after the restart, after a lookup | nothing recorded; the PSP's notification brings the decision | the sweep sends again |
| Two copies of the merchant's request at once | the second waits on the lock and gets the stored 202 | both call with one ID, and the PSP acts once | the second needs a lease or an "in progress" answer |
| A resend after the PSP forgot the ID | prevented by the lookup before every resend | prevented only by a lookup before every call | prevented by the lookup before every resend |
| A cancel races an authorization | refused: the request is recorded | the PSP may hold an authorization that we refused | refused |
| The PSP is down for an hour | 202, and the request waits | 503 | 503 |

What happens before a request is sent again:

| Option | Why not |
|---|---|
| **Read the PSP's records first, and send again only if the request is not there** | Chosen |
| Send again blindly, with the same ID | The PSP's idempotency cache forgets, so a request it acknowledged yesterday and gets again today can act twice |
| Send again blindly while the request is young | A clock would decide, and `recorded_at` may not (ADR-0012) |

## Decision

- **A request is recorded, not performed.** `authorize`, `capture`, `refund` and `void`
  each record a request to the PSP, under the payment's lock (ADR-0018) and its
  merchant (ADR-0025), in the same transaction as the key's record (ADR-0026), and
  answer 202 (ADR-0033). The 202 is the request's final answer, stored under the key
  like an applied command.
- **Its ID is the stable ID.** A `RequestId` is a UUIDv7 made by the caller (ADR-0005).
  The sender passes it to the PSP as the request's idempotency key and reference, on
  every attempt.
- **What a request carries.** An authorization asks for an amount, at most the
  payment's, and the PSP may authorize less (ADR-0016). A capture asks for at most the
  amount authorized, a refund for at most what stays refundable once the refunds in
  flight are counted, and a void for nothing. The business times leave the bodies:
  `expires_at`, `captured_at` and `refunded_at` are the PSP's to report (ADR-0022).
- **What is refused at once,** as a 409 or 422 stored under the key (ADR-0026), with no
  request recorded: a move that the table of moves cannot take from the payment's
  status (ADR-0016); a second authorization; a capture or a void while another capture
  or void is in flight; a refund above what stays refundable. `cancel` stays a step of
  our own, and is refused once an authorization has been requested.
- **The sender** is a task in the server and the only code that sends a request to the
  PSP. It sends a request after its commit and never from inside a transaction
  (ADR-0026): at once, and then as ADR-0032 spaces the attempts.
- **Acknowledgements.** A `2xx` from the PSP is recorded as an append-only row, once per
  request. The sender then stops, and a merchant sees that the PSP holds the request
  (ADR-0033). Correctness does not depend on it.
- **A lookup before every resend.** The first send needs none. Before any later one,
  the sender reads the PSP's records for the payment (ADR-0031) and sends again only if
  the request is not there. No clock decides. Orpheus does the same, to "query service
  for the status of a transaction first if it's a retry-attempt", and so did Bernstein,
  Hsu and Mann's recoverable requests in 1990: after a failure the client asks which IDs
  the server last received and answered before resending anything.
- **Decisions come from the PSP's history** (ADR-0031), never from its answer to a send,
  which says only whether it holds the request (ADR-0032).
- **Not in phase 4:** a capture or refund that fails after the PSP confirmed it, as
  Adyen's `CAPTURE_FAILED` and `REFUND_FAILED` can. ADR-0016's extension rule leaves
  room for a capture reversal, and such a fact stops the payment for an operator
  (ADR-0031).

## Consequences

- A merchant learns an outcome by polling its request (ADR-0033), until phase 5
  publishes events.
- A PSP that is down delays decisions, not answers.
- No request waits for the PSP's answer, so the recovery points that ADR-0026 warned of
  are not needed.
- ADR-0023's "A merchant vouches for the PSP's authorization until phase 4" ends: only
  the PSP's word moves a payment (I8, ADR-0034).
- I9 holds: a request takes effect at the PSP at most once (ADR-0034). A test makes the
  fake act, lose its reply and forget the key, restarts the sender, and finds one
  effect; with the lookup turned off, the same test finds two (ADR-0029).
- This is durable execution written by hand: Temporal, Durable Functions and Beldi
  journal each step, and here the journal is the table of requests, so no framework is
  added.
- Other tests: copies of one request racing give one request and one answer; the sender
  killed around each statement; a cancel racing an authorization request; each refusal
  at request time.

## References

- J. H. Saltzer, D. P. Reed and D. D. Clark, [End-to-End Arguments in System Design](https://doi.org/10.1145/357401.357402) (ACM Transactions on Computer Systems, 1984).
- Butler Lampson, [Hints and Principles for Computer System Design](https://arxiv.org/abs/2011.02455) (arXiv, 2021), section 3.6.2.
- Philip A. Bernstein, Meichun Hsu and Bruce Mann, [Implementing Recoverable Requests Using Queues](https://doi.org/10.1145/93597.98721) (SIGMOD, 1990).
- Jon Chew and Ninad Khisti, [Avoiding Double Payments in a Distributed Payments System](https://medium.com/airbnb-engineering/avoiding-double-payments-in-a-distributed-payments-system-2981f6b070bb) (Airbnb Tech Blog, 2019).
- Sebastian Burckhardt et al., [Durable Functions: Semantics for Stateful Serverless](https://doi.org/10.1145/3485510) (OOPSLA, 2021).
- Haoran Zhang, Adney Cardoza, Peter Baile Chen, Sebastian Angel and Vincent Liu, [Fault-tolerant and transactional stateful serverless workflows](https://www.usenix.org/conference/osdi20/presentation/zhang-haoran) (OSDI, 2020).
- Temporal, [Activity Definition](https://docs.temporal.io/activity-definition) (docs).
- Adyen, [Capture](https://docs.adyen.com/online-payments/capture) and [Refund](https://docs.adyen.com/online-payments/refund) (docs).
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests) (API reference).
