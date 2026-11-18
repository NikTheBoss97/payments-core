<!-- Author: Nicola Antonello -->

# ADR-0061: The URL check reads the host and the port

- Status: accepted
- Date: 2026-11-18
- Supersedes: in ADR-0055's Decision, the authority of "The URL's CHECK"

## Context

ADR-0060 has the server refuse an endpoint's URL whose host is empty, or whose port is not
decimal digits that fit in 16 bits: the `http` crate parses both, and hyper-util's client,
given no port that it can read, connects to port 80, so such a URL would send a merchant's
events to another port than the one it names. The database's check of ADR-0055 does not read
the host or the port, so it takes `http://:80/` and `http://merchant.example:port/`.

Only the runtime role writes endpoints, through the route, so only a bug of ours could store
such a row: a code path that skips the parser, or a data migration. The project enforces each
rule twice, in Rust and in the database, and this one, that an endpoint's URL names where its
events go, stood in the database only in part.

## Options

| Option | Why not |
|---|---|
| **The check reads the host and the port, and the client parses each stored URL** | Chosen |
| The client alone parses each stored URL | A row that the rules refuse is taken, and found only at delivery, which ends three days later with its endpoint disabled as `failing`, as if the merchant had failed |
| The check alone | It cannot say all that the parser refuses, such as a `%` in a host, so the client would still send what the `http` crate reads its own way |
| Neither | A row written past the route could send a merchant's events to port 80 |

## Decision

- **The authority** is a host, then nothing, or a colon and a port:
  - the host is an IP literal, a `[`, one or more characters that are not `]`, `/`, `?`, `#`,
    `@`, whitespace or a control character, and a `]`; or a name of one or more characters
    that are none of those, nor `:` or `[`;
  - the port is decimal digits, none or more, whose value is at most 65535; none means port
    80, as RFC 9110 says.
- **The rest** is ADR-0055's: the scheme `http` in lowercase, no user information, then
  nothing, or a `/` or a `?` and no `#`, whitespace or control character.
- **The check** is a regular expression for the shape and a numeric comparison for the port's
  range, which a regular expression of the digits would only obscure. It keeps its name,
  `webhook_endpoints_url_check`.
- **The client** parses each stored URL with ADR-0060's rules before each attempt (ADR-0053),
  for what the check cannot say.

## Consequences

- Every URL that the route stores, the check takes, and every host and port that the route
  refuses, the check refuses: a test runs the parser's own cases against the check, both
  ways, so a change to one fails it until the other follows.
- Aligning the two found that the route took an empty IP literal, `http://[]/`, which it now
  refuses as an empty host.
- A migration replaces the check, and PostgreSQL checks every stored row as it adds it.

## References

- T. Berners-Lee, R. Fielding and L. Masinter, [RFC 3986: Uniform Resource Identifier (URI): Generic Syntax](https://www.rfc-editor.org/rfc/rfc3986.html) (IETF, 2005), sections 3.2.2 (the host, whose IP literal holds an address) and 3.2.3 (the port).
- R. Fielding, M. Nottingham and J. Reschke, [RFC 9110: HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html) (IETF, 2022), section 4.2.1 (the `http` URI).
- PostgreSQL 18 manual, [POSIX Regular Expressions](https://www.postgresql.org/docs/18/functions-matching.html#FUNCTIONS-POSIX-REGEXP).
