<!-- Author: Nicola Antonello -->

# The HTTP API

What a client of this service can rely on. The decisions behind it are in
[ADR-0023](adr/0023-the-http-api.md) (routes and answers),
[ADR-0024](adr/0024-authentication.md) (credentials),
[ADR-0025](adr/0025-tenant-isolation.md) (isolation) and
[ADR-0026](adr/0026-idempotency-keys.md) (idempotency keys).

This is being built route by route: so far, a payment can be read.

## Running the server

The binary takes its configuration from the environment, and stops at once if it
cannot use it.

| Variable | Meaning | Default |
|---|---|---|
| `TENANT_DATABASE_URL` | The login that merchant routes hold. It sees nothing until a transaction names its merchant. | required |
| `SYSTEM_DATABASE_URL` | The login that looks an API key up. | required |
| `BIND_ADDRESS` | Where to listen. | `127.0.0.1:8080` |

It never runs migrations. It speaks plain HTTP/1.1, so TLS ends in front of it, and
in any real deployment it must: a secret sent in the clear is a stolen key. Stopping it
counts as a crash, which the design already has to survive, so there is no graceful
shutdown.

## Credentials

A merchant presents an API key as `Authorization: Bearer <secret>`. The merchant comes
from the credential, never from the request.

## Connections

A client that hangs up gets no answer, and what it asked for still finishes. A client
that only half-closes its socket gets no answer either, because the server reads that
as the client having gone.

No timeout wraps a request. Waits are bounded on the server, and one that waits too
long answers [`unavailable`](#unavailable).

## Routes

### `GET /payments/{id}`

The payment's terms, its status and what can still be refunded.

```json
{
  "payment_id": "019b76da-a800-7000-8000-000000000001",
  "psp": "mock",
  "currency": "EUR",
  "amount": 10000,
  "fee_bps": 250,
  "status": "created",
  "refundable": 0
}
```

Amounts are integers in minor units. An ID has one spelling, lower case with hyphens,
and any other is a [`not-found`](#not-found). So are another merchant's payment and no
payment at all, which read exactly alike, so an ID's existence does not leak. The
credential is checked first, so someone with no key learns nothing about an ID.

## Problems

Every error is `application/problem+json` ([RFC 9457](https://www.rfc-editor.org/rfc/rfc9457.html)),
with a `type` that points to a section below, a fixed `title`, and the `status`. Its
text never reveals SQL, a constraint or another merchant's data.

### unauthorized

`401`, with `WWW-Authenticate: Bearer`. The credential is missing, malformed, unknown or
revoked. All of these get the same answer.

### not-found

`404`. No such route, or no such payment, or another merchant's payment. Sending the
request again cannot change this, because IDs are made by the server.

### method-not-allowed

`405`, with `Allow` naming the methods that the route takes.

### unavailable

`503`. The service could not finish the request, for instance because the database could
not answer. A good key can get this and never gets `unauthorized` for it. Sending the
request again is safe.

### internal-error

`500`. A bug or a panic. It says nothing of its cause, which goes to the server's log.
