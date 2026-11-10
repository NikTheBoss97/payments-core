<!-- Author: Nicola Antonello -->

# The HTTP API

What a client of this service can rely on. The decisions behind it are in
[ADR-0023](adr/0023-the-http-api.md) (routes and answers),
[ADR-0024](adr/0024-authentication.md) (credentials),
[ADR-0025](adr/0025-tenant-isolation.md) (isolation),
[ADR-0026](adr/0026-idempotency-keys.md) (idempotency keys), and
[ADR-0030](adr/0030-signed-notifications.md),
[ADR-0032](adr/0032-answers-and-retries.md) and
[ADR-0043](adr/0043-the-servers-psp-routes-and-tasks.md) (the provider's notifications).

## Running the server

The binary takes its configuration from the environment, and stops at once if it
cannot use it. A variable that is set but empty counts as not set.

| Variable | Meaning | Default |
|---|---|---|
| `TENANT_DATABASE_URL` | The login that merchant routes hold. It sees nothing until a transaction names its merchant. | required |
| `SYSTEM_DATABASE_URL` | The login that looks an API key up, runs the operator's routes and records the provider's notifications. | required |
| `OPERATOR_KEY_SHA256` | The SHA-256 of the operator's secret, as 64 lowercase hex digits. The server never holds the secret itself. | required |
| `MOCK_PSP_URL` | Where the provider's API is: `http://` and its host and port, with no path. | required |
| `MOCK_PSP_API_KEY` | The key that the server's calls to the provider carry, as `Authorization: Bearer`. The server never logs it. | required |
| `MOCK_PSP_SIGNING_KEYS` | The keys that sign the provider's notifications, each `whsec_` and the base64 of 24 to 64 bytes, separated by spaces: the current one, and during a rotation the next. | required |
| `BIND_ADDRESS` | Where to listen. | `127.0.0.1:8080` |

It never runs migrations. It speaks plain HTTP/1.1, so TLS ends in front of it, and
in any real deployment it must: a secret sent in the clear is a stolen key. Stopping it
counts as a crash, which the design already has to survive, so there is no graceful
shutdown.

## Credentials

A merchant presents an API key as `Authorization: Bearer <secret>`. The merchant comes
from the credential, never from the request. A secret is `pck_` and 64 hex digits, and it is
shown once, because the server stores only its digest. A merchant can hold several keys, all
equivalent, so rotating one means issuing another and revoking the old.

The operator presents its own secret in the same way. It opens the
[operator's routes](#the-operators-routes) and nothing else, and a merchant's key opens none of
them. The [quickstart](../README.md#quickstart) shows how to make one. Changing it means
restarting the server with a new digest.

## Idempotency keys

Every `POST` but the [provider's notifications](#the-providers-notifications) requires
`Idempotency-Key: "..."`: a quoted string of 1 to 255 visible ASCII
characters without `"` or `\`. Sending a request again under its key is always safe: it
has one effect at most, and it gets the answer that the first attempt got.

- **What a key holds.** A request's final answer, committed in the same transaction as its
  effect. A retry gets the same status and the same bytes, and runs nothing. The one exception
  is a new API key's secret, which is in the first answer alone, because no stored answer holds
  a secret.
- **What is final.** A command that applied, and a refusal that depends on the state of the
  payment. Everything else is stored nowhere and leaves the key free: a request that was
  never decided, and an outcome that is not known. So after
  [`unavailable`](#unavailable), send the request again under the same key.
- **A key never expires.**
- **A key belongs to its merchant,** so two merchants can use the same one. The operator's keys
  are apart from every merchant's.
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

### `POST /payments/{id}/void`

Records that an authorization was voided. It takes no body, not even `{}`.

### `POST /payments/{id}/cancel`

Cancels a payment before its authorization. It takes no body, not even `{}`. Once an
authorization has been requested, the provider may hold one, so a cancel gets
[`authorization-requested`](#authorization-requested).

### What is not a route

A declined authorization, an expired one and a settlement report what the provider decided, so
a merchant cannot claim them. They arrive with the provider's webhooks.

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

## The operator's routes

These take the operator's secret, and an idempotency key like any other `POST`. An ID in their
paths follows the same rule as a payment's: one spelling, and any other is a
[`not-found`](#not-found).

### `POST /merchants`

Registers a merchant at the fee rate that the operator sets, in basis points from 0 to 10000. A
payment copies the rate when it is created, and keeps it.

```json
{ "fee_bps": 250 }
```

`201`:

```json
{ "merchant_id": "019b76da-a800-7000-8000-000000000001" }
```

### `POST /merchants/{id}/api-keys`

Issues an API key to a merchant. It takes no body, not even `{}`.

`201`, with the secret, which is shown here and never again:

```json
{
  "api_key_id": "019b76da-a800-7000-8000-000000000002",
  "secret": "pck_0000000000000000000000000000000000000000000000000000000000000000"
}
```

A retry under the same key gets the key's ID alone. If the first answer was lost, issue
another key, and revoke the one that nobody holds.

### `POST /api-keys/{id}/revoke`

Revokes an API key, which stops working on its next request. It takes no body, not even `{}`.

`200`, also if the key was already revoked, because what the request asks for already holds:

```json
{ "api_key_id": "019b76da-a800-7000-8000-000000000002" }
```

## The provider's notifications

`POST /webhooks/mock` takes the payment provider's notifications, signed as Standard
Webhooks `v1` signs them. It takes no API key and no idempotency key: the signature says
who sent a notification, and its ID tells its copies apart. A notification only names a
payment. What the provider decided, the service reads from the provider itself.

Before it reads the body, the service checks, in this order:

1. `webhook-id`, `webhook-timestamp` and `webhook-signature` each appear once;
2. the ID is 1 to 255 of `[0-9A-Za-z_-]`, and the timestamp is whole Unix seconds in
   digits alone;
3. the timestamp is at most 5 minutes from the service's clock, either way;
4. one `v1,` entry of the space-separated signature list is the base64 of the
   HMAC-SHA256, under one of the keys, of the ID, the timestamp and the body as
   received, joined by `.`.

The body must then name `data.payment_id`. Unlike the routes above, it may hold fields
that the service does not know, which it ignores.

| The service | Answers |
|---|---|
| recorded the notification, now or before | `202`, with no body |
| refused it | [`notification-refused`](#notification-refused) |
| could not record it | [`unavailable`](#unavailable), with `Retry-After: 5` |

A notification about a payment that the service does not hold is recorded, logged as a
warning, and answered `202`, since sending it again cannot change that. The service never
answers `410`, on which a sender stops delivering to an endpoint.

Once it has answered, the service reads the payment's history from the provider and
applies what the provider decided, in a task of its own: the answer never waits for that
read. At most 4 such reads run at once. A notification that finds them all busy, or whose
read fails, is recorded and answered all the same, and its payment waits for a later read.

## Problems

Every error is `application/problem+json` ([RFC 9457](https://www.rfc-editor.org/rfc/rfc9457.html)),
with a `type` that points to a section below, a fixed `title`, and the `status`. Its
text never reveals SQL, a constraint or another merchant's data.

### unauthorized

`401`, with `WWW-Authenticate: Bearer`. The credential is missing, malformed, unknown or
revoked, or it is a merchant's key on an operator's route or the other way round. All of these
get the same answer.

### not-found

`404`. No such route, or no such payment, merchant or API key, or another merchant's payment.
Sending the request again cannot change this, because IDs are made by the server.

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

### authorization-requested

`409`. The payment has a request to authorize it already, whether or not the provider has
decided it. A payment has one authorization, and once one is requested, the provider may hold
it, so the payment can no longer be cancelled. It is final and stored under the key, as for an
[`illegal-move`](#illegal-move).

### capture-or-void-pending

`409`. A capture or a void of the payment still waits for the provider's decision, and another
would race it. It is final and stored under the key, as for an [`illegal-move`](#illegal-move):
once the provider has decided, trying again takes a new key.

### payment-stopped

`409`. The payment is stopped: the provider's history held something that the service could not
apply, and an operator has to look at the payment before it takes another request. Requests
recorded before the stop wait too, and the provider is not sent them. It is final and stored
under the key, as for an [`illegal-move`](#illegal-move).

### notification-refused

`400`. The provider's notification is unreadable or does not verify: a header is missing,
repeated or not visible ASCII, the ID or the timestamp has another shape, the timestamp is
more than 5 minutes from the service's clock, no signature matches a key, or the body is
above 2 MB or names no payment. All of these get the same answer, and nothing is recorded.

### unavailable

`503`. The service could not finish the request, for instance because the database could
not answer, or it cannot tell whether the request took effect. A good key can get this and
never gets `unauthorized` for it. Sending the request again is safe: under its key, a request
that did take effect gets the answer and runs nothing. On the provider's route it carries
`Retry-After: 5`, and a copy of a notification that was recorded after all gets `202`.

### internal-error

`500`. A bug or a panic. It says nothing of its cause, which goes to the server's log.
