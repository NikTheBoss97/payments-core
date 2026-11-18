<!-- Author: Nicola Antonello -->

# ADR-0050: Webhook endpoints, registered by the operator

- Status: accepted; partly superseded by ADR-0060
- Date: 2026-11-15
- Supersedes: in ADR-0024, "It opens only the three operator routes of ADR-0023": the
  operator's key also opens the two routes below.

## Context

A delivery needs a URL for each merchant. Phase 3 answered a close question for API keys:
the owner chose operator endpoints, "for scripting over HTTP", a merchant holds any number
of keys, revoking one appends a row, and a key's secret is shown once (ADR-0024).

Standard Webhooks recommends the other way: "enable customers to add multiple webhook
endpoints", with "an API to add, remove, and list webhook endpoints", and event types
chosen per endpoint. Stripe works that way, and its endpoint's secret is "Only returned at
creation".

What tells the two apart is who supplies the URL that the server will call. A merchant's
URL is untrusted input, and calling it is where server-side request forgery lives: a URL
such as `http://169.254.169.254/`, or an internal host, makes the server send a request
from inside the network. For destinations that users supply, OWASP's cheat sheet says
"Allow lists cannot be used here", calls the block-list approach "not an impenetrable
wall", and lists the defences: every A and AAAA record checked, the connection made to the
checked address, redirects off, and outbound traffic filtered at the network layer.
Standard Webhooks' advice is to "prevent the webhooks from calling into internal networks
and services", for example through a dedicated proxy.

## Options

| Option | Why not |
|---|---|
| **The operator registers them, as API keys** | Chosen |
| Merchants register their own | The SSRF defences above, to build and keep; new privileges and policies for the merchant's role; and an exception for the fake merchant on `localhost` in tests. A security feature that is not the outbox: recorded as a later need |
| One URL on the merchant, set when it is created | Moving it would need a mechanism of its own anyway, with no overlap between the old URL and the new |

## Decision

- **Routes,** in the shape of ADR-0023's table. Both take an idempotency key, and their
  answers are recorded as the operator's are (ADR-0026):

  | Route | Credential | Body | Success |
  |---|---|---|---|
  | `POST /merchants/{id}/webhook-endpoints` | operator | `url` | 201: `endpoint_id`, `secret` |
  | `POST /webhook-endpoints/{id}/disable` | operator | none | 200: `endpoint_id` |

- **The secret is shown once.** The first answer carries it and the stored answer does
  not, so a retry gets the ID alone (ADR-0026). An operator who lost it registers another
  endpoint and disables the unused one. ADR-0051 decides what the secret is.
- **Any number per merchant,** all equivalent. Each receives every event of its merchant
  that is written while it is enabled (ADR-0053). Moving a URL, or changing a secret,
  means registering the new endpoint and then disabling the old.
- **Disabling appends a row** with its reason, the operator's or one of ADR-0053's, and a
  disabled endpoint stays disabled. Disabling it again answers 200 like the first time, as
  revoking a revoked key does (ADR-0023's amendment). An endpoint that the server does not
  know is a 404.
- **The URL** is absolute and `http`, without user information, which would store a
  credential, and without a fragment. Anything else is a malformed body. The relay's
  client speaks plain HTTP, as the PSP's does, which refuses an `https` address at start
  for the same reason: TLS belongs to deployment, where `hyper-rustls` joins as a feature
  (ADR-0036). Refusing `https` when an endpoint is registered keeps it from failing every
  attempt until ADR-0053 disables it. The build that adds TLS also accepts `https`.
- **Rows,** append-only under ADR-0004's guards. An endpoint holds its ID, a UUIDv7 made
  in Rust (`EndpointId`), its merchant, its URL and `recorded_at`; a disablement holds its
  endpoint, under a unique key, its reason and `recorded_at`. `payments_app`, which runs
  the operator's routes (ADR-0025), writes both. The merchant's role has no privilege on
  either.

## Consequences

- A merchant changes its URL through the operator, as it gets its keys.
- The operator is trusted with URLs: one that points inside the network joins the failure
  model's "assumed not to happen" column. Merchants registering their own endpoints, with
  the SSRF defences above, is a later need.
- `docs/api.md` documents both routes when the code lands.
- Tests: a replayed registration without its secret; disabling twice; an unknown
  endpoint; each kind of malformed URL, and an `https` one; the merchant's role refused on
  both tables; a merchant's key refused on both routes.

## Amendments

- 2026-11-18: ADR-0060 supersedes "Anything else is a malformed body." Text that is not an
  absolute URL is still `400 malformed-body`; an absolute URL that these rules refuse is
  `422 unusable-url`.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): multiple endpoints, the endpoint management API, and server-side request forgery.
- Stripe, [The Webhook Endpoint object](https://docs.stripe.com/api/webhook_endpoints/object) (API reference).
- OWASP, [Server-Side Request Forgery Prevention Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html) (OWASP Cheat Sheet Series).
