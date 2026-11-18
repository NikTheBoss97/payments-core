<!-- Author: Nicola Antonello -->

# ADR-0055: Storing events and deliveries

- Status: accepted
- Date: 2026-11-16

## Context

ADR-0049 to ADR-0054 decide what phase 5 keeps: the operator's webhook endpoints and
their disablements, one event for each fact that the catch-up applies, one delivery for
each endpoint enabled when the event is written, and a success for each delivery that
an endpoint answered with a `2xx`. They leave the tables open, and two choices had real
alternatives.

Three rules of the schema shape the answer. Every column is `NOT NULL` (ADR-0017). A
foreign key can prove a copied column true, as `payment_facts` proves its copy of the
payment's PSP, which lets one row tie two others that must agree (ADR-0037). And a row
that commits with another needs no `recorded_at` of its own, as a decision commits with
its fact (ADR-0037).

ADR-0052 has each event and each delivery record its transaction's ID with
`pg_current_xact_id()`, which "If executed in a subtransaction, this will return the
top-level transaction ID", as `xid8`, whose values "increase strictly monotonically and
cannot be reused in the lifetime of a database cluster". Measured on PostgreSQL 18.6 on
2026-11-15, on temporary tables: `xid8` has btree and hash operator classes, so it can
stand in a unique key and a foreign key; inside a savepoint the function gave the
top-level transaction's ID, 7357789, while the row's `xmin` was the subtransaction's,
7357790; and a delivery written in a later transaction than its event failed with
`23503` on a foreign key to the event's ID and transaction ID, while one written with
its event succeeded.

## Options

An endpoint's URL, which ADR-0050 wants absolute and `http`, without user information
and without a fragment:

| Option | Why not |
|---|---|
| **A CHECK on those rules, behind the server's own parsing** | Chosen |
| The server's parsing alone | Raw SQL run as `payments_app`, which the failure model assumes, could store a credential as user information |
| A CHECK on the scheme alone | Simpler, but it leaves the credential to the server alone |

How a delivery is tied to its event and its endpoint:

| Option | Why not |
|---|---|
| **One foreign key per rule: the event's merchant, the endpoint's merchant, the event's transaction** | Chosen |
| One key over the event's ID, merchant and transaction | One key fewer, but a delivery to another merchant and one written in a later transaction would report the same constraint, so no test could tell which guard fired (ADR-0013) |

## Decision

- **The tables.** All are append-only (`LG001`), granted by column to `payments_app`
  alone, and named with every constraint:

  | Table | One row per | Key and guards |
  |---|---|---|
  | `webhook_endpoints` | endpoint | `id` is the `EndpointId`; its merchant; its URL, checked; `(id, merchant_id)` unique, for the deliveries' key |
  | `webhook_endpoint_disablements` | disabled endpoint | `endpoint_id`, with a reason: `operator`, `gone` or `failing` |
  | `payment_events` | applied fact | `id` is the `EventId`; `(payment_id, position)` unique and referencing the fact; `(payment_id, merchant_id)` referencing the payment; `xact_id`; `(id, merchant_id)` and `(id, xact_id)` unique, for the deliveries' keys |
  | `webhook_deliveries` | event and endpoint | `id` is the `DeliveryId`; `(event_id, endpoint_id)` unique; `(event_id, merchant_id)` referencing the event, `(endpoint_id, merchant_id)` the endpoint and `(event_id, xact_id)` the event again; its own `xact_id` |
  | `webhook_delivery_successes` | delivery that succeeded | `delivery_id` |

- **`payments` gains `(id, merchant_id)` unique,** the target of the events' key, as it
  gained `(id, psp)` for the facts.
- **`xact_id`** is `xid8`, `NOT NULL DEFAULT pg_current_xact_id()`, and no role may
  insert into it, so only the database stamps it, as it stamps `recorded_at`
  (ADR-0012). Its name keeps it apart from `transaction_id`, which here names a ledger
  transaction. Rust reads it as `xact_id::text::bigint` and binds a mark as
  `$1::bigint::text::xid8`, with no sqlx type override (ADR-0009).
- **A delivery commits with its event,** so it has no `recorded_at`, and its `xact_id`
  must be its event's: one written in any later transaction has no event to match. A
  savepoint does not change the ID, so this guard holds where ADR-0021's test of `xmin`
  cannot.
- **The URL's CHECK:** `http://`, then an authority that is not empty and holds no `/`,
  `?`, `#` or `@`, then nothing that is a `#`, whitespace or a control character. The
  server parses first, answers 422, and stores the scheme in lowercase. The build that
  adds TLS changes the CHECK with its parser (ADR-0050).
- **An index on `webhook_deliveries (xact_id)`** serves the relay's search (ADR-0052).
  Successes and disablements are found by their primary keys.
- **Who sees what.** The merchant's role has no privilege on any of the five, so none is
  under row-level security, as neither `api_keys` nor `psp_notifications` is (ADR-0025).

## Consequences

- These tables come before the code that writes them. `LG008`, which refuses a fact
  without its event, comes once the catch-up and the test fixtures write events
  (ADR-0049).
- A delivery to an endpoint of another merchant, or written outside its event's
  transaction, fails on a foreign key whatever SQL the runtime role runs: the guards of
  I7 and I10 in PostgreSQL (ADR-0054).
- Nothing in the database proves that every endpoint enabled when an event was written
  got its delivery: a check at `COMMIT` would race with a registration (ADR-0054).
- Tests write every table with raw SQL as each role, break each guard alone and assert
  its pair of SQLSTATE and constraint name, and write a delivery inside a savepoint; the
  catalog test pins the columns, constraints, defaults, triggers and grants.

## Amendments

- 2026-11-18: "The server parses first, answers 422" reads as ADR-0060 decides: `400
  malformed-body` for text that is not an absolute URL, and `422 unusable-url` for an
  absolute URL that the rules refuse. The check below stays as it is.

## References

- PostgreSQL 18 manual, [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html): `pg_current_xact_id()`.
- PostgreSQL 18 manual, [Object Identifier Types](https://www.postgresql.org/docs/18/datatype-oid.html): `xid8`.
