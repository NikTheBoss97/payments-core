<!-- Author: Nicola Antonello -->

# ADR-0042: The sender's budget, its work and stopped payments

- Status: accepted
- Date: 2026-11-09
- Supersedes: in ADR-0037's Decision, "Who sees what", the stops among the tables on
  which a merchant has no privilege.

## Context

ADR-0028, ADR-0032 and ADR-0039 decide what the sender does. It sends a request after
its commit, under its `RequestId`, and looks the request up before every resend. It
stops at a `2xx`, or at a lookup that finds the request. It spaces its retries with full
jitter and no limit per request, and shares one token bucket for the process with the
sweep. They leave four things open: what refills the bucket and what a token buys,
where a pass of the sweep resumes when the bucket runs dry (ADR-0041), how the sender
learns of a request, and what happens to the requests of a payment that the catch-up
stopped.

Retry budgets in practice refill from successes. In gRPC's retry throttling, "Every
failed RPC will decrement the `token_count` by 1" and "Every successful RPC will
increment the `token_count` by `tokenRatio`", and retries stop once the count is at
half the bucket or below. The AWS SDKs' retry quota starts at 500 tokens, spends 14 or 5
on a retry, and restores some on a success, so that once failures spread "your
application fails fast instead of waiting through retries that are unlikely to
succeed". Both serve a caller that waits for its answer. The sender's caller does not
wait: the merchant already holds a 202, and the request is a promise with no limit per
request (ADR-0032).

A stop leaves a payment for an operator (ADR-0031). Its requests still wait for the
sender, and a merchant can still record new ones.

## Options

What refills the bucket, and what a token buys:

| Option | Why not |
|---|---|
| **Time: a token a second, up to 60; a token per retry** | Chosen |
| Successes, as gRPC and the AWS SDKs | In a full outage nothing succeeds, so retries stop, and the backlog waits for new requests to succeed first. ADR-0032 asks for a steady trickle |

Where a pass of the sweep resumes when the bucket runs dry:

| Option | Why not |
|---|---|
| **After the last payment that it caught up, in ID order, wrapping round** | Chosen |
| From the first payment, every pass | The lowest IDs take every token, and the payments after them wait as long as the outage lasts |

How the sender learns of a request:

| Option | Why not |
|---|---|
| **A handover after the commit, and a search at each step** | Chosen |
| The search only | Every request waits for a step, and its first send needs a lookup, which doubles the calls in normal operation |
| The handover, and one search at startup | A handover lost without a crash leaves its request unsent until the next restart |

A stopped payment's requests:

| Option | Why not |
|---|---|
| **Held, and a new one refused with a 409** | Chosen |
| Sent anyway | Money that the PSP moves while the payment is stopped cannot reach the ledger until an operator acts, so the ledger and the PSP drift further apart |

## Decision

- **The budget.** The bucket of ADR-0032 holds up to 60 tokens and gains one a second,
  counted from the time that each step is given: no step reads a clock. A retry costs
  one token: a lookup, with the send that it may lead to, or one catch-up of the sweep's
  pass. A first send, and the catch-up after a notification, cost none. A retry that
  finds no token waits for a later step. During an outage the process retries at most
  once a second, however long it lasts, and at most 60 times at once when the PSP comes
  back. Phase 8 measures both numbers.
- **The backoff.** Before the *n*th retry the delay is drawn uniformly, in milliseconds,
  between zero and `min(1 hour, 1 second × 2^n)`, and is never shorter than the PSP's
  `Retry-After` (ADR-0032). The draws come from the operating system through
  `getrandom`; a test passes its own.
- **The sweep's place.** A pass catches up the due payments in ID order, starting after
  the last payment that the pass before it caught up, and wrapping round. The place
  lives in memory with the bucket, so a restart starts again from the first payment,
  which costs one pass's fairness and never correctness. The IDs only order the pass.
- **Where requests come from.** The process that records a request hands its ID to the
  sender after the commit, and the sender sends it at once, without a lookup: only a
  request handed over this way can have had no earlier send. Each step also searches
  for requests with neither an answer nor a decision. One that the sender does not know
  may have been sent before a crash, so it is due after a jittered delay, and a lookup
  comes first. A request that has gained an answer, a decision or a stop since leaves
  the sender.
- **The lookup is a catch-up** (ADR-0031, ADR-0041). It reads the history, applies the
  new facts, and looks for the request among the requests that the history lists. Found,
  the request is recorded as acknowledged (ADR-0039); missing, or under a 404, it is sent
  again. A history without that list is unreadable: read as empty, it would send again a
  request that the PSP may hold under a key that it has forgotten.
- **The answers** follow ADR-0032's table. An answer that the table does not name, such
  as a 401, 403 or 404, is an unknown outcome: the sender backs off and looks up before
  it sends again. Only the PSP's 400 or 422 records a request as malformed, so a wrong
  key strands no request. An answer is one insert as `payments_app`, committed through
  the one function of ADR-0014, and a `23505` means that it is recorded already. An
  insert whose outcome is unknown leaves the request with the sender, whose next lookup
  finds it.
- **The flag.** A request still unacknowledged an hour after its first attempt in this
  process is listed in the step's report, for the server to log. Timing lives in memory
  (ADR-0032), so after a restart the flag can come later.
- **One call at a time.** A step looks up and sends one request at a time, each call
  bounded by the client's timeout of 10 seconds, as a pass catches up one payment at a
  time (ADR-0041).
- **Stopped payments.** The sender leaves a stopped payment's requests, as the sweep
  leaves the payment, and the step reports them. `record_request` refuses a new request
  on a stopped payment with a 409, before any other check, and the API names the problem
  `payment-stopped`. To read the stop under the payment's lock, the merchant's role
  gains `SELECT` on `payment_stops`, under the policy `payment_stops_tenant_isolation`,
  which tests `EXISTS` on `payments` (ADR-0025). A held request stays `recorded`
  (ADR-0033), and what an operator does about it stays outside phase 4 (ADR-0037).
- **I9's test** follows ADR-0034 and ADR-0039. The fake takes a request and loses its
  reply, so the send times out, and then it forgets its keys. A new sender, as after a
  restart, finds the request, looks it up and records it as acknowledged, and the fake
  has taken it once. The control sends the same request again through the client, with
  no lookup, and the fake takes it twice.

## Consequences

- A long outage costs the PSP at most one retry a second from this process, and the
  sweep's catch-ups share that budget with the sender's lookups.
- In normal operation a request reaches the PSP at once; after a restart, within a few
  seconds of the sender's first step.
- The sender keeps no table of its own: what it knows is in the requests, answers,
  decisions and stops, and its timing is in memory.
- A merchant learns that its payment stopped from the 409, and reads its own payments'
  stops. The other tables of ADR-0037's "Who sees what" keep their rule.
- Tests: each answer of the client; the bucket's refill and spending; the delay's
  bounds, as a property; a pass that resumes after its last payment; a handed-over
  request sent without a lookup; a request that the search finds, looked up first; a
  lookup that finds the request; each answer of ADR-0032's table, and `Retry-After`;
  the bucket during an outage; the flag; held requests and the 409; I9 and its control;
  the merchant's role reading only its own stops, and the catalog test pinning the
  grant and the policy.

## References

- Noah Eisen and Eric Gribkoff, [gRPC Retry Design](https://github.com/grpc/proposal/blob/master/A6-client-retries.md) (gRPC proposal A6, last updated 2024): retry throttling.
- AWS, [Retry behavior](https://docs.aws.amazon.com/sdkref/latest/guide/feature-retry-behavior.html) (AWS SDKs and Tools Reference Guide): the retry quota of the updated retry behaviour.
- The records that it builds on: ADR-0014, ADR-0025, ADR-0028, ADR-0031, ADR-0032,
  ADR-0033, ADR-0034, ADR-0037, ADR-0039 and ADR-0041.
