<!-- Author: Nicola Antonello -->

# ADR-0030: Signed notifications

- Status: accepted
- Date: 2026-11-07
- Supersedes: in ADR-0023's Decision, "Every `POST` requires an idempotency key
  (ADR-0026)", for the route that receives the PSP's notifications.

## Context

The PSP tells us that something changed by posting a notification to a public URL,
where anyone can post. Before acting on one, the server must know that the PSP sent it,
that no byte changed on the way, and that it is not an old message played again. TLS
does not settle the first two: it proves the server's identity to the caller, not the
caller's to the server. So the proof travels inside the message.

A MAC such as HMAC-SHA256 needs one secret key on both sides: without it nobody can make
a valid tag for a new message, and Bellare's proof shows that HMAC's security does not
rest on the hash being collision-resistant. A digital signature such as Ed25519 needs a
private key only at the sender, so the receiver can check a signature but never make
one.

The Standard Webhooks specification (v1.0.0) signs `msg_id.timestamp.payload`, because
"A secure signature scheme should verify the authenticity of both the payload, and the
additional metadata". It offers HMAC-SHA256 (`v1`) and Ed25519 (`v1a`), and says "Prefer
asymmetric signature schemes over symmetric ones", warning: "If you do not control the
security of both the producer and consumer, it is recommended you use an asymmetric
signature instead." Both PSPs checked sign with HMAC-SHA256: Stripe over
`timestamp.payload`, Adyen over eight named fields, which leaves the rest of the payload
unsigned.

## Options

The scheme:

| Option | Why not |
|---|---|
| **Standard Webhooks `v1`: HMAC-SHA256 over `msg_id.timestamp.payload`** | Chosen |
| Standard Webhooks `v1a`: Ed25519 | The spec prefers it, and our side would keep no key that can sign. But Stripe and Adyen sign with HMAC, the spec's Rust reference library implements only `v1` and publishes no `v1a` test vector, and `ed25519-dalek` 3.0 brings crates of its own. The header's list lets `v1a` join later |
| RFC 9421, HTTP Message Signatures | The IETF's general scheme, with more to canonicalize: chosen components, and the body through `Content-Digest`. Neither PSP checked uses it |

Replays:

| Option | Why not |
|---|---|
| **Refuse an attempt more than 5 minutes off our clock, either way, and keep every event ID forever** | Chosen |
| No timestamp check, and IDs kept forever | Departs from the spec and from Stripe's libraries for no saving |
| The spec's example: keep IDs for 5 minutes | A notification that we recorded but whose `2xx` was lost comes back at 00:05:05 in the spec's own schedule, after its ID was forgotten |

The keys:

| Option | Why not |
|---|---|
| **A list in an environment variable, rotated by a restart** | Chosen |
| A database table | Rotation without a restart, but the secrets would sit in the database and its backups |

## Decision

- **The route.** `POST /webhooks/{psp}`, with `mock` the only PSP, takes the PSP's
  notifications. It needs no API key and no `Idempotency-Key`: the signature
  authenticates it, and its event ID tells its copies apart (ADR-0031). It runs on
  `payments_app` (ADR-0025).
- **The headers** are `webhook-id`, `webhook-timestamp`, in whole Unix seconds, and
  `webhook-signature`, a space-separated list of `v1,<base64>`.
- **Verification,** in this order, before the body is parsed:
  1. The ID has one fixed shape and the timestamp holds only digits. Neither may hold a
     `.`, so `msg_id.timestamp.payload` reads one way only. This is Abadi and
     Needham's first principle: "Every message should say what it means: the
     interpretation of the message should depend only on its content." Otherwise
     `a.1.2.{...}` could be the ID `a.1` at time `2`, or the ID `a` at time `1` with a
     body that starts `2.`, and one signature would vouch for both.
  2. The timestamp is within 5 minutes of our clock, either way: the tolerance of the
     spec's reference library and of Stripe's libraries.
  3. One listed `v1` signature verifies under one key that we hold. The MAC is computed
     over the bytes received, because "Any manipulation to the raw body of the request
     causes the verification to fail" (Stripe), and is compared through `digest`'s
     `CtOutput`, which "provides a safe Eq implementation that runs in constant time".
     An early-exit comparison would let timing reveal how much of a guessed tag is
     right, and "turn them into signing oracles" (the spec): remote timing has been
     measured to 15 to 100 µs across the Internet, and to 100 ns over HTTP/2 with
     concurrent requests.

  Only then is the JSON parsed. A failure is a 400 (ADR-0032).
- **Keys** are 32 random bytes from the operating system, written as `whsec_` and
  base64. The spec allows 24 to 64 bytes, but RFC 2104 calls a key shorter than the
  hash's output "strongly discouraged", and SHA-256's is 32 bytes. The server reads its
  list of keys from the environment. To rotate, the PSP signs with the old and the new
  key during an overlap, as the spec's list allows, and the server accepts either; a
  restart changes the list.
- **Every event ID is kept forever** (ADR-0005, ADR-0031), so a replay is always a
  duplicate, and the window is defence in depth.
- **Tests** make their keys at runtime. The one fixed key is the spec's published test
  vector, from its Rust reference library, which pins both the fake and the server to
  the spec: two implementations that share no code (ADR-0029).
- **Our calls to the PSP** carry an API key that the PSP issued, from the environment,
  in `Authorization`, and the server never logs it (ADR-0024).
- **Dependencies:** `hmac`, `sha2` and `base64`, all already built for sqlx.
- **Not in phase 4: TLS.** The spec notes that signatures do not encrypt, and the binary
  speaks plain HTTP (ADR-0023).

## Consequences

- A leaked key lets anyone sign a notification. Under ADR-0031 a notification only tells
  the server to read the PSP's history, so a forged one can make the server read, never
  decide: decisions come from the PSP's own API. That weakens the case for Ed25519 made
  above, which assumed notifications carry decisions.
- A clock more than 5 minutes off refuses genuine notifications until it is fixed. The
  PSP delivers them again, and the sweep of ADR-0031 reads the history anyway.
- GitHub's secret scanning looks for Stripe webhook signing secrets in public
  repositories, so no key other than the spec's vector belongs in the repository.
- Tests: the spec's vector; each step of verification failing alone; a body changed by
  one byte; a timestamp just inside and just outside the window, as a pure function of
  `now`; two keys during a rotation; an ID with a `.`.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0), and its Rust reference library.
- Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks).
- Adyen, [Verify HMAC signatures](https://docs.adyen.com/development-resources/webhooks/secure-webhooks/verify-hmac-signatures) (docs).
- H. Krawczyk, M. Bellare and R. Canetti, [RFC 2104: HMAC: Keyed-Hashing for Message Authentication](https://www.rfc-editor.org/rfc/rfc2104) (IETF, 1997).
- Mihir Bellare, [New Proofs for NMAC and HMAC: Security without Collision Resistance](https://doi.org/10.1007/s00145-014-9185-x) (Journal of Cryptology, 2015).
- Scott A. Crosby, Dan S. Wallach and Rudolf H. Riedi, [Opportunities and Limits of Remote Timing Attacks](https://doi.org/10.1145/1455526.1455530) (ACM TISSEC, 2009).
- Tom Van Goethem, Christina Pöpper, Wouter Joosen and Mathy Vanhoef, [Timeless Timing Attacks: Exploiting Concurrency to Leak Secrets over Remote Connections](https://www.usenix.org/conference/usenixsecurity20/presentation/van-goethem) (USENIX Security, 2020).
- Martín Abadi and Roger Needham, [Prudent Engineering Practice for Cryptographic Protocols](https://doi.org/10.1109/32.481513) (IEEE Transactions on Software Engineering, 1996).
- A. Backman, J. Richer and M. Sporny, [RFC 9421: HTTP Message Signatures](https://www.rfc-editor.org/rfc/rfc9421) (IETF, 2024).
- RustCrypto, [`CtOutput`](https://docs.rs/digest/0.11.3/digest/struct.CtOutput.html) (digest 0.11.3 API docs).
