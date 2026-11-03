<!-- Author: Nicola Antonello -->

# ADR-0024: Authentication

- Status: accepted
- Date: 2026-11-03

## Context

Until phase 3 the design listed authentication and multi-tenancy as non-goals,
with "at most a static API key". Choosing what an idempotency key is unique within
reopened that: the project's owner decided that it be built as if both were goals,
with real credentials and real isolation (ADR-0025), and the non-goal is rewritten.
The tenant is the merchant, which already exists and already owns its payments.

## Options

Who presents what:

| Option | Why not |
|---|---|
| **An API key per merchant, sent as a Bearer token, and one operator key** | Chosen |
| One static key for the service | Cannot tell merchants apart, so it isolates nothing |
| OAuth 2.0 or signed JWTs | An issuer, signing keys and expiry, for server-to-server clients that gain nothing from them here |
| Mutual TLS | Certificates to issue and rotate, and TLS ends in front of the service (ADR-0023) |

How a key is stored:

| Option | Why not |
|---|---|
| **The SHA-256 digest of a 256-bit random secret** | Chosen |
| Argon2 or bcrypt | Built to slow down guessing of human-chosen passwords: a cost on every request that 256 random bits do not need, and a per-key salt rules out looking a key up by its digest |
| The secret itself, or encrypted | A database leak hands over every key, or the encryption key has to live next to them |

How merchants and keys are created:

| Option | Why not |
|---|---|
| **Operator endpoints behind a separate key** | Chosen by the owner, for scripting over HTTP |
| A command-line tool on the host | No admin surface at all, but no way to script onboarding over HTTP, and it was not the owner's choice |

## Decision

- **A merchant holds any number of API keys,** all equivalent, so rotating one means
  issuing another and revoking the old.
- **The secret** is `pck_` followed by 64 lowercase hex digits: 32 bytes from the
  operating system. The prefix lets a secret scanner recognise a leaked key. It
  travels as `Authorization: Bearer <secret>` (RFC 6750) and is shown once.
- **Only its digest is stored.** `api_keys` holds `id`, `merchant_id`, a unique
  32-byte `digest` and `recorded_at`. Authenticating hashes what was presented and
  looks the digest up. NIST requires look-up secrets of 112 bits or more to be
  stored "in a hashed form using an approved hashing function", and asks for a
  password hashing scheme only below that. An API key is not literally a look-up
  secret, but the reasoning carries over: nothing here is guessable. An equality
  test on a digest reveals nothing useful about the secret, since a client cannot
  steer a SHA-256 digest bit by bit.
- **Revoking appends.** `api_key_revocations` holds `api_key_id` and `recorded_at`,
  and a key with a row there stops working on its next request. Both tables are
  append-only under the guards of ADR-0004.
- **Every failure looks the same.** A missing, malformed, unknown or revoked key
  gets one 401, with `WWW-Authenticate: Bearer` and one body. A database failure
  while authenticating is a 503, so a client never concludes that a good key is bad.
- **The operator** is one credential: the environment holds `OPERATOR_KEY_SHA256`,
  the digest of a secret made the same way, so the server never holds the secret
  itself. It is compared in constant time with `subtle`. It opens only the three
  operator routes of ADR-0023, and a merchant's key opens none of them.
- **The merchant comes from the credential, never from the request.** `POST
  /payments` has no merchant field, and an unknown field is refused (ADR-0023).
- **The operator sets the fee.** A merchant must not choose its own rate, so
  `merchants` gains `fee_bps`, set by `POST /merchants`, and `POST /payments` copies
  it onto the payment, where ADR-0020 fixes it for the payment's life. The library's
  commands still carry the rate as a value.
- **Issuing a key cannot replay its secret.** The first answer carries `secret`, and
  the stored answer does not (ADR-0026), so a retry gets the key's ID alone. An
  operator who lost the first answer issues another key. The unused one is harmless
  and can be revoked.
- **Dependencies:** `sha2` for the digests, `subtle` for the operator's comparison,
  and `getrandom` for the secret.

## Consequences

- A stolen key acts as its merchant until it is revoked, and nothing can tell the
  two apart. Keys do not expire on their own.
- No lockout or rate limit defends against guessing, because 256 random bits cannot
  be guessed online. Rate limits for other reasons are out of scope.
- Each request costs one more indexed read, on the system pool (ADR-0025).
- A Bearer secret sent in the clear is a stolen key, so TLS in front of the service
  is required in any real deployment, and the server must never log the
  `Authorization` header.
- Rotating the operator key means restarting the service with a new digest.
- Not built: users and sessions, per-key permissions, and key expiry.
- ADR-0017's `merchants` gains a column, and ADR-0020's rate gets an owner.

## References

- M. Jones and D. Hardt, [RFC 6750: The OAuth 2.0 Authorization Framework: Bearer Token Usage](https://www.rfc-editor.org/rfc/rfc6750.html) (IETF, 2012).
- NIST, [SP 800-63B-4: Digital Identity Guidelines: Authentication and Authenticator Management](https://pages.nist.gov/800-63-4/sp800-63b.html) (2025), section 3.1.2.2.
