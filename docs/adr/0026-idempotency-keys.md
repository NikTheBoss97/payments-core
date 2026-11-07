<!-- Author: Nicola Antonello -->

# ADR-0026: Idempotency keys

- Status: accepted; partly superseded by ADR-0028
- Date: 2026-11-03

## Context

A client that times out cannot tell whether its request took effect, and a retry
builds a new request, which gets new IDs. ADR-0005 and ADR-0019 left that gap to
this phase: until now, a partial refund could apply twice. Invariant I4 closes it:
one idempotency key produces at most one effect.

The sources agree on the shape of the answer. RIFL (SOSP 2015) says that "the
completion record must be created atomically with the mutations of the operation"
and that "It must not be possible for an operation to complete without a visible
completion record, or vice versa." Brandur Leach says of endpoints that only change
local state that mapping requests to transactions "is far easier and less
complicated" than his own recovery points, "and I'd suggest that anyone who can get
away with it take that path". Every request in this phase is such a request.

## Options

How a key is tied to its effect:

| Option | Why not |
|---|---|
| **The answer commits with the effect** | Chosen |
| Reserve the IDs first: commit the key with the IDs to use, then run the identical command, so that ADR-0019's `AlreadyApplied` settles a retry | One more commit per request, and refusals stay undecided, so a late duplicate can turn a refusal into an effect. Remembering them safely needs the same hook as the option chosen |
| Claim, then update, as Stripe and Brandur Leach do: commit a claim, run, update the row with the response | A mutable row, where the service has no `UPDATE` anywhere, and a lease to free a claim left by a crash. It pays off with calls to other systems, which this phase has none of |
| The key is the `TransitionId` | ADR-0005 rejected IDs chosen by the client. Clients would have to make UUIDv7s, one key would name a step across every merchant, and refusals would stay undecided |

What a refused request leaves:

| Option | Why not |
|---|---|
| **The refusal is stored under the key** | Chosen |
| Nothing, and a retry is decided again | A delayed duplicate can apply after the client was told no. TigerBeetle's docs state the same hazard: "Without this mechanism, a transfer that previously failed could succeed if retried when the underlying state changes." ADR-0019 left this to phase 3 |

How long a key lasts:

| Option | Why not |
|---|---|
| **Forever** | Chosen |
| A set time, such as Stripe's 24 hours or more | RIFL warns of "undetectable double-execution" when a client returns after its record is gone. It also needs a purge job and a role that may delete, where every other table is append-only |

What a key is unique within:

| Option | Why not |
|---|---|
| **The authenticated merchant, and the operator's keys apart** | Chosen |
| The whole service | One merchant's key would collide with another's, and reveal that it exists (ADR-0025) |
| Each payment | A key reused on another payment would go unnoticed |

How a reused key is noticed:

| Option | Why not |
|---|---|
| **SHA-256 of the method, path and raw body** | Chosen. The draft names a checksum of the payload as one way |
| Comparing the parsed fields | Needs a canonical form, and two requests that differ only in layout would count as one. Being stricter here can raise a false alarm, but never merges two requests |
| Storing the whole request | More to keep, for the same answer |

## Decision

- **The header** is `Idempotency-Key: "..."`, a Structured Field string as
  draft-ietf-httpapi-idempotency-key-header-08 requires, holding 1 to 255 visible ASCII
  characters without `"` or `\`, so no escapes arise. It is required on every `POST`,
  and a missing or malformed one, a bare token included, gets a 400.
- **The record.** `idempotency_records` holds `merchant_id`, `key`, `fingerprint` (32
  bytes), `status`, `body` and `recorded_at`, under the primary key `(merchant_id,
  key)`. It is insert-only. A `CHECK` keeps `status` between 200 and 499, so no
  unfinished or failed answer can be stored, and `body` must be JSON. It is text, so a
  replay returns the same bytes. `operator_idempotency_records` has the same shape
  without a merchant, for the operator's requests (ADR-0024).
- **The flow of a request:**
  1. authenticate (ADR-0024), and fingerprint the raw request;
  2. look the key up: a record with the same fingerprint is replayed, and one with
     another gets a 422;
  3. parse the body and build the command with fresh IDs, including the reads that
     ADR-0019 does outside any transaction;
  4. in one transaction: the lock (ADR-0018), the merchant (ADR-0025), and the decision
     (ADR-0016). If applied, write the step, posting and record. Applied or refused,
     insert the key's record holding the rendered answer, then `COMMIT` through the one
     function of ADR-0014;
  5. on `23505` for the key's primary key, another attempt won: drop the transaction
     and return to step 2, where the record now exists.
- **Final answers** are the ones the table of moves gives under the lock: applied, an
  illegal move, an amount above its limit. They depend on a state that can change. The
  rest of ADR-0023's table is stored nowhere: it cannot change, or nothing was decided.
- **Each attempt has its own IDs.** The record decides whether anything runs, so a
  retry that finds none starts afresh: its first attempt did not commit. So
  `AlreadyApplied` is never reached through HTTP. It stays as the library's own guarantee
  and for later phases, and it is a second line behind the key.
- **A duplicate in flight waits.** On a payment's command it waits on the lock, and where
  no lock exists, as on `POST /payments`, on the key's unique index (ADR-0005). Then it
  finds the record and gets the real answer. `lock_timeout` bounds each wait, and after
  that it is a 503. This knowingly departs
  from the draft's "SHOULD" of a 409: the wait needs no in-progress state, and it spares
  the client a second retry.
- **A key reused with a different request** gets a 422, as in the draft. The first
  request's record stands.
- **A key never expires.** `docs/api.md` publishes this, as the draft asks.
- **No secret is ever stored in an answer.** Issuing an API key adds `secret` to the
  first response only (ADR-0024).
- **In the library,** a command takes an optional seal holding the key, the fingerprint
  and how to render its answer, and writes the record inside its unit of work, which
  ADR-0019 built for this. Without a seal, a command behaves as it did in phase 2.

## Consequences

- One row and one `COMMIT` decide everything. No record means that the attempt left no
  effect, and a record means that its answer is final. A crash leaves nothing to clean
  up, so there is no lease, reaper or completer.
- No extra commit: one more insert per request.
- `idempotency_records` grows by one row per final answer, and is never purged.
- Trying again after a refusal takes a new key.
- ADR-0019's "rejections are not remembered" still holds for the library. The key layer
  does the remembering, as that record expected.
- Accounts opened by an attempt that then fails stay, committed on their own (ADR-0005),
  and the next attempt finds them.
- A call to another system cannot sit inside this transaction. Phase 4's calls to the PSP
  must come after a commit and carry a stable ID of their own, and phase 4 decides how.
  If a request ever has to wait for the PSP's answer, recovery points may return.
- The draft is not yet an RFC, so its header's syntax could still change.
- The tests: copies of one request racing, which must give one effect and one answer; the
  backend killed around the key's insert and during `COMMIT`; a late duplicate of a refusal
  after the payment has moved on; each reuse caught; two merchants holding one key; a test
  that every `POST` route refuses a missing key; and a scan showing that no stored answer
  holds a secret.

## Amendments

- 2026-11-07: ADR-0028 supersedes the list in "Final answers are the ones the table of
  moves gives under the lock: applied, an illegal move, an amount above its limit". A
  request recorded for the PSP is final too: its 202 is stored under the key, and the
  PSP's later decision does not change it. The consequence "Phase 4's calls to the PSP
  must come after a commit and carry a stable ID of their own, and phase 4 decides how"
  is decided there.

## References

- Jayadeba Jena, Sanjay Dalal and Dmitrii Tishkin, [The Idempotency-Key HTTP Header Field](https://datatracker.ietf.org/doc/draft-ietf-httpapi-idempotency-key-header/) (IETF Internet-Draft 08, 2026).
- Collin Lee, Seo Jin Park, Ankita Kejriwal, Satoshi Matsushita and John Ousterhout, [Implementing Linearizability at Large Scale and Low Latency](https://doi.org/10.1145/2815400.2815416) (SOSP, 2015), section 3.
- Sebastian Burckhardt et al., [Durable Functions: Semantics for Stateful Serverless](https://doi.org/10.1145/3485510) (OOPSLA, 2021).
- Brandur Leach, [Implementing Stripe-like Idempotency Keys in Postgres](https://brandur.org/idempotency-keys) (2017), and [Using Atomic Transactions to Power an Idempotent API](https://brandur.org/http-transactions) (2017).
- Malcolm Featonby, [Making retries safe with idempotent APIs](https://aws.amazon.com/builders-library/making-retries-safe-with-idempotent-apis/) (Amazon Builders' Library, 2021).
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests) (API reference).
- TigerBeetle, [Create transfers](https://docs.tigerbeetle.com/reference/requests/create_transfers/) (`id_already_failed`).
- M. Nottingham and P.-H. Kamp, [RFC 9651: Structured Field Values for HTTP](https://www.rfc-editor.org/rfc/rfc9651.html) (IETF, 2024).
