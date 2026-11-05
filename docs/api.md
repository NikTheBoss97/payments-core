<!-- Author: Nicola Antonello -->

# The HTTP API

What a client of this service can rely on. The decisions behind it are in
[ADR-0023](adr/0023-the-http-api.md) (routes and answers),
[ADR-0024](adr/0024-authentication.md) (credentials),
[ADR-0025](adr/0025-tenant-isolation.md) (isolation) and
[ADR-0026](adr/0026-idempotency-keys.md) (idempotency keys).

This is being built route by route: so far, a merchant can take a payment through its life.

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

## Idempotency keys

Every `POST` requires `Idempotency-Key: "..."`: a quoted string of 1 to 255 visible ASCII
characters without `"` or `\`. Sending a request again under its key is always safe: it
has one effect at most, and it gets the answer that the first attempt got.

- **What a key holds.** A request's final answer, committed in the same transaction as its
  effect. A retry gets the same status and the same bytes, and runs nothing.
- **What is final.** A command that applied, and a refusal that depends on the state of the
  payment. Everything else is stored nowhere and leaves the key free: a request that was
  never decided, and an outcome that is not known. So after
  [`unavailable`](#unavailable), send the request again under the same key.
- **A key never expires.**
- **A key belongs to its merchant,** so two merchants can use the same one.
- **A key is for one request:** the method, the path and the exact bytes of the body. Reusing
  it for anything else is a [`key-reused`](#key-reused), even if only the layout of the JSON
  differs, and the first answer stands.
- **A duplicate in flight waits** for the first attempt, and then gets its answer. The service
  never answers that a request is in progress.
- **Trying again after a refusal takes a new key,** because the key remembers the refusal.

## Connections

A client that hangs up gets no answer, and what it asked for still finishes. Sending it
again under its key gets the answer. A client that only half-closes its socket gets no
answer either, because the server reads that as the client having gone.

No timeout wraps a request. Waits are bounded on the server, and one that waits too
long answers [`unavailable`](#unavailable).

## Routes

A body is JSON, whatever its `Content-Type` says, and at most 2 MB. Unknown fields are
refused, so a misspelt or forbidden one is never silently ignored. Amounts are integers in
minor units: a fraction or an exponent is refused, even `100.0`. A currency is an ISO 4217
code in upper case. A time is an RFC 3339 string that names its offset, and it is kept to
whole microseconds in UTC.

A command on a payment answers `200` with where the payment now stands: the step's `seq`, the
statuses it moved between, and what can still be refunded.

```json
{ "seq": 1, "from": "created", "to": "authorized", "refundable": 0 }
```

### `POST /payments`

Creates a payment. The merchant comes from the credential, the fee rate is the one that the
operator set, and the provider is the mock until there are others.

```json
{ "currency": "EUR", "amount": 10000 }
```

`201`, with the new payment's ID and where it stands:

```json
{
  "payment_id": "019b76da-a800-7000-8000-000000000001",
  "seq": 0,
  "from": "created",
  "to": "created",
  "refundable": 0
}
```

### `POST /payments/{id}/authorize`

Records that the provider authorized the payment, for at most the amount requested. Until
the provider's webhooks exist, this is the merchant's word for it.

```json
{ "amount": 8000, "expires_at": "2027-01-01T00:00:00Z" }
```

### `POST /payments/{id}/capture`

Records a capture, for at most the amount authorized, and posts it to the ledger.

```json
{ "amount": 8000, "captured_at": "2026-12-01T00:00:00Z" }
```

### `POST /payments/{id}/refund`

Records a refund and posts it to the ledger. Below what can still be refunded it is partial,
and at that amount it is final.

```json
{ "amount": 3000, "refunded_at": "2026-12-02T00:00:00Z" }
```

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

### malformed-key

`400`. The `Idempotency-Key` header is missing, there is more than one, or it is not a quoted
string of 1 to 255 visible ASCII characters without `"` or `\`. A bare token is not a string.

### malformed-body

`400`. The body is not what the route takes. Nothing was decided, so the key is still free.

### body-too-large

`413`. The body is above 2 MB.

### key-reused

`422`. This key holds the answer to a different request. Sending that request again still
gets its answer.

### unknown-currency

`422`. A well-formed code that the service does not know. Nothing is stored, because
currencies change only by a migration.

### illegal-move

`409`. The payment's status does not allow this. It is always about the payment, and never about
a request still in flight, which the service does not answer. It is final and stored under the
key, so a copy of the request delivered later gets this too, even if the status has since moved
on. Trying again takes a new key.

### amount-above-limit

`422`. The amount is above what the payment allows: the amount requested, the amount authorized,
or what can still be refunded. It is final and stored under the key, as for an
[`illegal-move`](#illegal-move).

### unavailable

`503`. The service could not finish the request, for instance because the database could
not answer, or it cannot tell whether the request took effect. A good key can get this and
never gets `unauthorized` for it. Sending the request again is safe: under its key, a request
that did take effect gets the answer and runs nothing.

### internal-error

`500`. A bug or a panic. It says nothing of its cause, which goes to the server's log.
