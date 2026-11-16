<!-- Author: Nicola Antonello -->

# ADR-0058: Each endpoint's secret as one HMAC block

- Status: accepted
- Date: 2026-11-16
- Supersedes: in ADR-0051's Decision, "**Dependency:** `hkdf`, which `sqlx-postgres` builds
  already": `hmac` computes the secret, and `hkdf` is not a dependency.

## Context

ADR-0051 derives each endpoint's secret with HKDF-Expand under the master key, 32 bytes
out, and named the `hkdf` crate for it. That crate's expand step writes into a buffer that
the caller passes, and it has no other form: its README starts from
`let mut okm = [0u8; 42];`. On [#68](https://github.com/NikTheBoss97/payments-core/pull/68),
CodeQL 2.27.4 reported our zeroed buffer of 32 bytes as a critical
`rust/hard-coded-cryptographic-value`, "This hard-coded value is used as a key", on a path
through the secret to `HmacSha256::new_from_slice`. It is a false positive, since the
expand step writes all 32 bytes or panics. But CodeQL's Rust models name no function of
`hkdf`, and the query's only barrier for a buffer filled after it is made is a call to
`getrandom`, so the pattern is reported wherever an output of `hkdf` keys an HMAC. The
pull request's check fails on any result rated critical or high.

RFC 5869 defines HKDF-Expand's output as the first bytes of `T(1) | T(2) | ...`, where
`T(1) = HMAC-Hash(PRK, T(0) | info | 0x01)` and `T(0)` is empty (section 2.3). With
SHA-256 and 32 bytes out, the output is `T(1)` alone: one HMAC.

## Options

| Option | Why not |
|---|---|
| **Compute `T(1)` with `hmac`, which already computes `v1`** | Chosen |
| Keep `hkdf`, and dismiss the alert as a false positive | The code stays the crate's documented use, and a dismissal holds in every branch. But the reason would live in GitHub, not in this repository, as ADR-0027 found for its first dismissals, and the next output of `hkdf` that keys an HMAC would raise the alert again |
| A CodeQL model pack that marks `hkdf`'s output as a barrier | A pack to maintain, which ADR-0027 declined for sqlx's wrapper |

## Decision

- **An endpoint's secret** is `T(1)`: the HMAC-SHA256, under the master key, of
  ADR-0051's `info` followed by the byte 1. That is HKDF-Expand with 32 bytes of output,
  so the derivation and every secret stay as ADR-0051 decided.
- **The function** takes a key of exactly 32 bytes and returns its 32 bytes, so no buffer
  is filled after it is made.
- **Dependency:** `hmac` computes both the secrets and `v1`. `hkdf` leaves
  `payments-core`'s dependencies; `sqlx-postgres` still builds it.

## Consequences

- Before `hkdf` left, the new function and `hkdf`'s expand step gave the same 32 bytes for
  10,000 random master keys and endpoints, through `secret_of`; with the byte 2 in place
  of 1, the comparison failed at the first one.
- The test of RFC 5869's test case 1 checks the first 32 of its 42 bytes, which are `T(1)`,
  and a second test recomputes the secret from ADR-0051's inputs written out.
- The derivation is now a short function of ours over `hmac`, where it was a crate's. A
  secret longer than 32 bytes would need more blocks: `hkdf` would then return, with this
  alert to settle.

## References

- H. Krawczyk and P. Eronen, [RFC 5869: HMAC-based Extract-and-Expand Key Derivation Function (HKDF)](https://www.rfc-editor.org/rfc/rfc5869.html) (IETF, 2010), section 2.3 and appendix A.1.
- GitHub, [Hard-coded cryptographic value](https://codeql.github.com/codeql-query-help/rust/rust-hard-coded-cryptographic-value/) (CodeQL query help), and its sources and barriers in [`HardcodedCryptographicValueExtensions.qll`](https://github.com/github/codeql/blob/main/rust/ql/lib/codeql/rust/security/HardcodedCryptographicValueExtensions.qll).
- GitHub Docs, [Resolving code scanning alerts](https://docs.github.com/en/code-security/code-scanning/managing-code-scanning-alerts/resolving-code-scanning-alerts): a dismissed alert is dismissed in all branches.
- RustCrypto, [`hkdf` 0.13.0](https://docs.rs/hkdf/0.13.0/hkdf/) (docs): the expand step fills the caller's buffer.
