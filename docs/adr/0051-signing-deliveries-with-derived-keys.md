<!-- Author: Nicola Antonello -->

# ADR-0051: Signing deliveries with derived keys

- Status: accepted
- Date: 2026-11-15

## Context

A merchant must be able to tell our deliveries from forgeries. Standard Webhooks, which
our own webhook route follows (ADR-0030), defines `v1`, HMAC-SHA256 over
`msg_id.timestamp.payload` under a symmetric secret of 24 to 64 random bytes, written
`whsec_` plus base64, and `v1a`, Ed25519. It says "Prefer asymmetric signature schemes
over symmetric ones", because then "only the producer needs access to the private key",
that "Signing keys should be unique per endpoint", and that "Reusing keys across customers
can lead to security issues!": under one key, a delivery to one merchant would verify at
another's endpoint.

An HMAC signer needs the secret itself, so ADR-0024's answer for API keys, to store only
a digest, cannot apply. Stripe keeps endpoint secrets in a form it can show again: its
dashboard reveals one on request, and rolling a secret can keep the old one signing for
up to 7 days. ADR-0024 refused that for API keys, because "A database leak hands over
every key, or the encryption key has to live next to them", and the failure model assumes
buggy code and manual SQL run as the runtime role.

The stakes are lower than for a key that moves money. A delivery is a thin event
(ADR-0049): a forged one can only make a merchant read its own payment through our API,
with its own key, and another merchant's payment answers 404.

RFC 5869's HKDF derives keys from one strong key. When that key is already a strong one,
"one can skip the extract part and use IKM directly to key HMAC in the expand step"
(section 3.3), and the `info` input binds "the derived key material to application- and
context-specific information" (section 3.2). Krawczyk's paper gives the design and its
analysis.

## Options

| Option | Why not |
|---|---|
| **HMAC (`v1`), each endpoint's secret derived from one master key** | Chosen |
| HMAC, a random secret per endpoint stored in the database | Stripe's way, and a secret could be rotated in place, but the database would hold every merchant's signing key, readable by the role that the failure model assumes can run buggy code and manual SQL |
| Ed25519 (`v1a`), a key pair per endpoint | The spec's preference, and a merchant would hold no secret. But the private keys would still need storing or deriving, a new crate would join, and ADR-0030's reasons still hold: the spec's Rust library implements only `v1`, and the spec publishes no `v1a` test vector. With thin events, the extra layer guards a hint |
| Secrets encrypted in the database | "The encryption key has to live next to them" (ADR-0024). Derivation does the same work with no ciphertext and no new crate |

## Decision

- **The master key** is 32 bytes from the operating system, in the environment as
  `WEBHOOK_MASTER_KEY`, written `whsec_` plus base64, so that one parser reads every key
  (ADR-0030). The binary stops at once on anything else, without repeating what it read,
  as it does for `OPERATOR_KEY_SHA256` (ADR-0023's amendment).
- **An endpoint's secret** is HKDF-Expand with SHA-256, keyed by the master key, with
  `info` the bytes of `payments-core webhook endpoint v1` followed by the endpoint's 16 ID
  bytes, and 32 bytes of output, written `whsec_` plus base64. The extract step is
  skipped, as section 3.3 allows for 32 bytes from the operating system. Each endpoint gets
  its own secret, and nothing secret is stored.
- **Registering an endpoint** answers its secret once (ADR-0050). The relay derives it
  again for each signature.
- **Each attempt is signed afresh,** as Stripe signs each retry: `webhook-id` is the
  event's ID, the same at every attempt and at every endpoint; `webhook-timestamp` is the
  attempt's time in seconds; `webhook-signature` is `v1,` followed by the base64 of the
  HMAC-SHA256 of `id.timestamp.body` under the endpoint's secret. A late retry therefore
  passes a receiver's tolerance window, such as our own 5 minutes (ADR-0030).
- **One computation** signs and verifies: the server's `v1` code of ADR-0030, which the
  spec's published test vector checks.
- **Rotation.** An endpoint's secret changes by registering a new endpoint and disabling
  the old (ADR-0050). During the overlap each event reaches both, under the same
  `webhook-id`, each signed with its own secret. The master key is replaced only if it
  leaks, and every endpoint must then be registered again.
- **Dependency:** `hkdf`, which `sqlx-postgres` builds already.

## Consequences

- A database leak, a backup or a buggy query reveals endpoint IDs, not keys.
- The environment holds one more secret. A leak of it would let an attacker sign for every
  endpoint, as a leak of the database would under the stored-secret option.
- A merchant verifies with any `v1` library of Standard Webhooks.
- Tests: HKDF-Expand against RFC 5869's test case 1, which gives a pseudorandom key and
  its output; two endpoints getting different secrets; a signature against the spec's test
  vector; a retry signed with a new timestamp; no stored answer holding a secret
  (ADR-0026); a malformed master key stopping the binary.

## Amendments

- 2026-11-16: `webhook-id` is the event's ID as the API writes IDs: 36 characters of
  lowercase hexadecimal and hyphens. Our own check of a notification's ID, 1 to 255 of
  `[0-9A-Za-z_-]` (ADR-0030), and the fake merchant's (ADR-0056) accept it.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): the signature scheme and its keys.
- H. Krawczyk and P. Eronen, [RFC 5869: HMAC-based Extract-and-Expand Key Derivation Function (HKDF)](https://www.rfc-editor.org/rfc/rfc5869.html) (IETF, 2010), sections 3.2 and 3.3, and appendix A.
- Hugo Krawczyk, [Cryptographic Extraction and Key Derivation: The HKDF Scheme](https://eprint.iacr.org/2010/264) (CRYPTO, 2010).
- Stripe, [Manage webhook endpoints](https://docs.stripe.com/events/manage-webhook-endpoints) (docs): revealing and rolling an endpoint's secret.
- Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks) (docs): a new signature and timestamp for each attempt.
