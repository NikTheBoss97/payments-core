<!-- Author: Nicola Antonello -->

# ADR-0033: Following a request

- Status: accepted
- Date: 2026-11-07
- Supersedes: in ADR-0023's table of routes, the answer of `GET /payments/{id}`, "the
  payment's terms, status and refundable amount", which gains the authorization.

## Context

Four routes now answer 202, and the outcome comes later (ADR-0028). RFC 9110 says that
a 202's representation "ought to describe the request's current status and point to (or
embed) a status monitor". Ours is a stored answer that every retry with the same key
gets unchanged (ADR-0026), so it cannot describe a status that keeps changing: it can
only point to where the status lives.

The payment's status cannot serve as that monitor. A capture that the PSP refuses
leaves the payment `authorized`, which reads exactly like a capture still in progress.
And phase 3's `GET /payments/{id}` does not show the authorization, whose amount and
expiry the PSP now reports.

Microsoft's Asynchronous Request-Reply pattern gives the 202 a `Location` "that the
client can poll to check the result of the long-running operation", and `Retry-After` to
pace the polling. Its status endpoint answers 200 with "enough information for the
client to understand the current state of the operation", and it warns against polling
the target resource for a 404 that turns into a 200, because "invalid request IDs also
return HTTP 404". Google's AIP-151 has a long-running method return an `Operation` that
clients poll. Stripe makes each refund an object of its own, whose `status` "can be
`pending`, `requires_action`, `succeeded`, `failed`, or `canceled`".

## Options

| Option | Why not |
|---|---|
| **Each request is a resource: `GET /payments/{id}/requests/{request_id}`** | Chosen |
| The payment is the monitor and lists its requests | Each 202 would point at the payment rather than at its request, and the payment's answer would grow with every refund |
| The payment's status alone | A refused capture reads exactly like one in flight |

## Decision

- **The route** `GET /payments/{id}/requests/{request_id}` takes its payment's ID through
  the one extractor that checks the merchant (ADR-0025), finds the request under that
  payment, and always answers 200. Another merchant's request, or a request of another
  payment, is a 404 exactly like none.
- **Its answer** holds the request's ID, its kind and amount, and its state, with the
  PSP's decision and business times once there is one.
- **A request's state is derived, never stored in a column,** as a payment's status is
  (ADR-0017): `recorded` while only the request exists, `acknowledged` once the PSP holds
  it (ADR-0028), and `applied` or `refused` once its history decides it (ADR-0031).
- **The 202's body** holds `request_id` and `payment_id`. Its `Location` is built from
  them and its `Retry-After` is fixed, so a replayed answer stays identical to the first.
- **`GET /payments/{id}`** adds the authorization once there is one: the amount
  authorized, which may be less than the amount asked for, and `expires_at`.
- **Refusals at request time** create no request (ADR-0028), so there is nothing to
  follow: their stored answer is the outcome, as in phase 3.
- **Isolation.** Every new table that a merchant reads gets a policy named
  `<table>_tenant_isolation`, which tests `EXISTS` on `payments` (ADR-0025).

## Consequences

- A merchant polls until phase 5 publishes events.
- No route lists a payment's requests. A merchant never loses a request's ID, because a
  retry with the same key replays the 202 that holds it.
- `docs/api.md` documents the route, the states and the 202 when the code lands.
- Tests: each state reached and shown; a refused capture told apart from one in flight;
  another merchant's request and another payment's request answered 404; the 202
  replayed byte for byte.

## References

- R. Fielding, M. Nottingham and J. Reschke, [RFC 9110: HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html) (IETF, 2022), section 15.3.3.
- Microsoft, [Asynchronous Request-Reply pattern](https://learn.microsoft.com/en-us/azure/architecture/patterns/async-request-reply) (Azure Architecture Center).
- Google, [AIP-151: Long-running operations](https://google.aip.dev/151) (API Improvement Proposals).
- Stripe, [The Refund object](https://docs.stripe.com/api/refunds/object) (API reference).
