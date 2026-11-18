<!-- Author: Nicola Antonello -->

# ADR-0060: Refusing an endpoint's URL

- Status: accepted
- Date: 2026-11-18
- Supersedes: in ADR-0050's Decision, "Anything else is a malformed body."

## Context

ADR-0050 lets the operator register an endpoint whose URL is absolute and `http`, without
user information, which would store a credential, and without a fragment, and calls
anything else a malformed body, which the API answers `400 malformed-body` (ADR-0023).
ADR-0055, which puts the same rules in the database's check on the URL, says that the
server "answers 422".

The API already draws RFC 9110's line between the two. `400` says that the body is not
what the route takes; `422` says that it is well formed and that the service refuses a
value in it, as `unknown-currency` does for a code it does not know. Neither is stored, so
in both cases the key stays free (ADR-0026).

The `http` crate, which parses the URI, drops a fragment without a word: `PathAndQuery`
truncates the text at its `#`. It also takes an empty host, as in `http://:80/`, and any
text after the host, as in `http://merchant.example:port/`, whose port it cannot read.
Given no port that it can read, hyper-util's client connects to port 80, so such a URL
would send a merchant's events to another port than the one it names. RFC 9110 has a
recipient reject an `http` URI with an empty host, and RFC 3986 writes a port in decimal
digits.

## Options

| Option | Why not |
|---|---|
| `400 malformed-body` for every refused URL, as ADR-0050 says | An `https` URL is well formed and refused by this build's choice, which is the line RFC 9110 draws for `422` |
| `422` with a new problem type for every refused URL | Text that is not a URL at all is a malformed body like any other field's |
| **`400` for text that is not an absolute URL, `422 unusable-url` for an absolute URL that the rules refuse** | Chosen |

## Decision

- **Not an absolute URL: `400 malformed-body`.** The `url` is not text, or its text does
  not parse as a URI with a scheme and an authority, or the authority's host is empty, or
  what follows the host is neither nothing nor a colon and a port in decimal digits that
  fits in 16 bits. An empty port means port 80, as RFC 9110 says. A relative reference, a
  bare host, an empty authority or host, a port that is not a number up to 65535,
  whitespace and a control character all fail here, as a malformed value of any other
  field does.
- **An absolute URL that the rules refuse: `422 unusable-url`.** Its scheme is not `http`,
  in any case, it holds user information, or it has a fragment. The server looks for the
  `#` in the text itself, since the parser drops it. Like `unknown-currency`, the answer
  is not stored, so the key stays free.
- **What is stored** is the URL as written, with its scheme in lowercase, as ADR-0055
  says, and the database's check stays behind the server's.
- **The build that adds TLS** accepts `https`, as ADR-0050 says, and `unusable-url` then
  names one rule fewer.

## Consequences

- From the problem's type, an operator knows whether the text or the URL is wrong.
- The API has one more problem type, documented with the others.
- The database's check stands behind the rules on the scheme, user information and a
  fragment, and refuses an empty authority and whitespace; the tests write each of these
  to the table as the runtime role. The host and the port are the server's alone to read:
  the check takes `http://:80/` and `http://merchant.example:port/`, which only the route
  refuses.

## Amendments

- 2026-11-18: An empty IP literal, as in `http://[]/`, is an empty host: RFC 3986's IP
  literal holds an address (section 3.2.2). The `http` crate parses it, with the host `[]`.

## References

- R. Fielding, M. Nottingham and J. Reschke, [RFC 9110: HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html) (IETF, 2022), sections 4.2.1 (the `http` URI), 15.5.1 (400) and 15.5.21 (422).
- T. Berners-Lee, R. Fielding and L. Masinter, [RFC 3986: Uniform Resource Identifier (URI): Generic Syntax](https://www.rfc-editor.org/rfc/rfc3986.html) (IETF, 2005), section 3.2.3 (the port).
- The `http` crate 1.5.0, [`src/uri/path.rs`](https://docs.rs/crate/http/1.5.0/source/src/uri/path.rs): `PathAndQuery::from_shared` truncates a fragment; [`src/uri/authority.rs`](https://docs.rs/crate/http/1.5.0/source/src/uri/authority.rs): `Authority::port` reads the text after the last colon as a `u16`, or gives none.
- hyper-util 0.1.21, [`src/client/legacy/connect/http.rs`](https://docs.rs/crate/hyper-util/0.1.21/source/src/client/legacy/connect/http.rs): `get_host_port` connects to port 80 when an `http` URI has no port that it can read.
