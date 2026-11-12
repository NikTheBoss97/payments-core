<!-- Author: Nicola Antonello -->

# ADR-0045: Following a request, and the switch in two pull requests

- Status: accepted
- Date: 2026-11-12
- Supersedes: in ADR-0043's Decision, "Three pull requests", which become four; and in
  ADR-0037's Decision, "Who sees what", the links of steps among the tables on which a
  merchant has no privilege.

## Context

ADR-0043 gives its third pull request the switch: the four routes record requests and
hand them over, the request route and the authorization in `GET /payments/{id}` appear,
and the API's document, the quickstart and phase 3's HTTP tests move onto the fake. In the
code, five files of HTTP tests, holding 65 tests, drive the four routes, and each test
that needs a state must reach it through the fake. With the rest, that pull request would
change, by estimate, well over 2,000 lines: what ADR-0043 turned down as too large to
review when it chose three pull requests over one.

ADR-0033 and ADR-0043 promise that the request route shows an applied request's amount
and business time. A fact keeps no business time (ADR-0037). The time is in the record
that the fact's step wrote, `expires_at` in `payment_authorizations`, `captured_at` in
`payment_captures` and `refunded_at` in `payment_refunds`, and only
`payment_transition_facts` says which step a fact applied. A payment has at most one
authorization and one capture, but it may have several refunds, of one amount too, and
ADR-0037 kept the links from the merchant's role.

The route's path, `/payments/{id}/requests/{request_id}`, holds two IDs, and the one
extractor that checks a payment's merchant reads a path as a single ID (ADR-0025).

## Options

How the switch lands:

| Option | Why not |
|---|---|
| **Two pull requests: the read side, then the switch** | Chosen |
| One pull request, as ADR-0043 says | One review of the routes, the new route, the API's document, the quickstart and the moved tests |

Where an applied refund's business time comes from:

| Option | Why not |
|---|---|
| **The merchant's role reads its own payments' links of steps** | Chosen |
| A payment's nth refund fact taken as its nth refund step | Relies on facts applying in order, and goes wrong for a payment that phase 3's route also refunded, until the switch |
| No business time until later | Breaks the answer that ADR-0033 and ADR-0043 describe |

## Decision

- **Four pull requests for phase 4's server.** After the webhook route and the server's
  tasks, the third adds the read side: the request route, the authorization in
  `GET /payments/{id}`, and the tests' helpers that take a request through the fake. The
  fourth switches the four routes to recorded requests with the handover, and moves the
  API's document, the quickstart and phase 3's HTTP tests onto the fake. Until the fourth,
  the four routes apply their steps as in phase 3. Each leaves a server that starts and
  passes its tests, and the rest of ADR-0043 stands.
- **A merchant reads its own payments' links of steps.** `payments_tenant` gets `SELECT`
  on `payment_transition_facts`, under the policy
  `payment_transition_facts_tenant_isolation`, which tests `EXISTS` on `payments`
  (ADR-0025), as for the stops (ADR-0042). It writes none, and the notifications stay out
  of its reach.
- **The path.** Each ID is read by its name, `id` and then `request_id`, in its one
  spelling (ADR-0026). Another spelling, another payment's request, another merchant's
  and none all read as the same 404 (ADR-0033).
- **The answer** is a 200 with `request_id`, `payment_id`, `kind`, `amount`, `state` and
  `applied`. `amount` is `null` for a void, which asks for none. `applied` is `null`
  until the request applies, and then holds the amount applied and the business time of
  the step that applied it, named as its record names it: `{amount, expires_at}` for an
  authorization, whose amount may be less than asked; `{amount, captured_at}` for a
  capture; `{amount, refunded_at}` for a refund; and `{}` for a void.
- **The state** is derived from rows in one statement, as the merchant: `applied` when a
  decision names a fact that applies (`authorize`, `capture`, `void`, `refund`),
  `refused` when it names one that refuses, otherwise `acknowledged` once the PSP's 2xx is
  recorded, and otherwise `recorded`, which a request that the PSP refused as malformed
  keeps showing (ADR-0037).
- **`GET /payments/{id}`** adds `authorization`: `null` until the payment has one, and then
  `{amount, expires_at}`, read in the statement that reads the status, so the two always
  agree.
- **Times** are written in RFC 3339, in UTC with `Z`, with the fractional digits that they
  need, six at most (ADR-0022): `2030-01-01T00:00:00Z`.
- **The tests' helpers** take a request through the fake as the server will: the merchant
  records it, a `Sender` of the fake's takes it by handover and sends it in a step, which
  records the acknowledgement, the fake decides, and a catch-up applies the decision. The
  fourth pull request records over HTTP instead.

## Consequences

- Phase 4 takes one pull request more. The read side lands while only the tests record
  requests, so until the switch the route answers 404 to every real request ID, and
  `authorization` shows what phase 3's authorize route wrote.
- A merchant now sees which of its PSP's facts applied which of its steps. It could read
  both already, and the link adds the business time that ADR-0033 promised.
- Tests: each state reached through the fake and shown; an authorization for less than
  asked; the time of each of two refunds of one amount; a void's empty `applied`; a
  refused capture told apart from one in flight; a malformed request still `recorded`;
  another merchant's request, another payment's, none and another spelling, each a 404;
  the authorization `null` and then shown; the merchant's role reading only its own
  links, and writing none.

## References

- G. Klyne and C. Newman, [RFC 3339: Date and Time on the Internet: Timestamps](https://www.rfc-editor.org/rfc/rfc3339.html) (IETF, 2002).
- The records that it builds on: ADR-0022, ADR-0025, ADR-0026, ADR-0033, ADR-0037,
  ADR-0042 and ADR-0043.
