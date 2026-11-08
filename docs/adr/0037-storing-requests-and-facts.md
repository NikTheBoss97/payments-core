<!-- Author: Nicola Antonello -->

# ADR-0037: Storing requests and facts

- Status: accepted
- Date: 2026-11-08

## Context

ADR-0028, ADR-0031 and ADR-0034 decide what the service keeps in phase 4: the requests
to the PSP and its answers to each send, its notifications, its facts as a chain by
position, one decision per request, a link from each step of the PSP's to the fact that
made it, which `LG007` will check, and a stop for an operator. They leave the tables
open, and three choices had real alternatives.

Two rules of the schema shape the answer. Every column is `NOT NULL` (ADR-0017), so a
relation that only some rows have, such as the request that a fact answers, cannot be a
column. And a foreign key can prove a copied column true, as `payment_authorizations`
proves its copy of the requested amount, which lets one row tie two others that must
agree.

## Options

The requests:

| Option | Why not |
|---|---|
| **One table, with a kind** | Chosen |
| One table per kind, as the records of steps are | The request route finds a request by its ID, whatever its kind, and every kind has the same columns |

What a fact keeps:

| Option | Why not |
|---|---|
| **Its place in the chain, the PSP's ID for it, its kind and its amount** | Chosen |
| A table per kind, with the PSP's business times | A full copy of the history, but each time that makes a step is stored twice, on the fact and on the step's record (ADR-0022), and the schema gains several tables |
| The first, plus the PSP's JSON for each fact | An audit copy, but no constraint can check JSON, and the project stores typed columns |

The PSP's answer to a send:

| Option | Why not |
|---|---|
| **One table, whose answer is `acknowledged` or `malformed`** | Chosen |
| ADR-0028's acknowledgements, and a table of flags apart | Two keys for one rule: a request gets one answer, and either answer stops the sender |

## Decision

- **The tables.** All are append-only (`LG001`), granted by column, and named with every
  constraint:

  | Table | One row per | Key and guards | Written by |
  |---|---|---|---|
  | `payment_requests` | request to the PSP | `id` is the `RequestId`; kind and amount checked by kind; one `authorize` per payment, by a partial unique index | `payments_tenant` only |
  | `payment_request_answers` | request that the PSP answered | `request_id` | `payments_app` |
  | `psp_notifications` | notification | `(psp, event_id)`; no foreign key, since it may name a payment that the service does not know | `payments_app` |
  | `payment_facts` | fact of the PSP's history | `(payment_id, position)`, with a foreign key to `position - 1`, the first pointing to itself; the PSP's ID unique per PSP; `(payment_id, psp)` references the payment; kind and amount checked | `payments_app` |
  | `payment_request_decisions` | request decided | `request_id`; a fact decides at most one request; the pair of kinds checked | `payments_app` |
  | `payment_transition_facts` | step that a fact made | `transition_id`; a fact makes at most one step | `payments_app` |
  | `payment_stops` | payment that the catch-up stopped | `payment_id`, with the position and a reason: `history_changed`, `no_place` or `contradiction` | `payments_app` |

- **A fact's kind is the step that it makes,** or one of three refusals that close a
  request without a step: `capture_refused`, `void_refused` and `refund_refused`. So
  one column of `payment_transition_facts` serves both foreign keys, to the step and to
  the fact, which then agree on kind and amount, and only a kind that is both a step and
  a fact can be linked: never a creation or a cancellation, nor a refusal.
- **A decision's two kinds** come across through its foreign keys, and a CHECK allows
  only an authorization or a decline for an authorization request, and for each other
  request its own kind or its refusal.
- **Business times stay on the steps' records** (ADR-0022), where the catch-up writes
  the PSP's times. A fact that makes no step keeps none.
- **The PSP's IDs,** for events and facts, have one shape, `[0-9A-Za-z_-]{1,255}`,
  with no `.`, as ADR-0030 asks of an event's ID.
- **Refusals at request time stay in Rust,** under the payment's lock, apart from one
  authorization request per payment. A request moves no money: only a fact makes a
  step, and the steps' constraints bound every amount (ADR-0017).
- **Who sees what.** A merchant inserts requests, and reads its own payments' requests,
  answers, facts and decisions, under policies that test `EXISTS` on `payments`
  (ADR-0025, ADR-0033). It has no privilege on the notifications, the links of steps or
  the stops.

## Consequences

- These tables come before the code that writes them. The guards of I8 that refuse what
  phase 3's code still writes, `LG007`, the merchant role's revocations and the
  restrictive policy (ADR-0034), come once the server records requests instead of steps.
- Reconciliation in phase 6 adds what it needs about a settlement, since a fact keeps no
  business time.
- A request that the PSP refused as malformed is a bug of ours: its merchant keeps seeing
  `recorded` (ADR-0033) while an operator looks.
- What an operator does about a stopped payment is outside phase 4.
- Tests write every table with raw SQL as each role, break each guard alone, and assert
  its pair of SQLSTATE and constraint name; the catalog test pins the columns,
  constraints, triggers, grants and policies.

## References

- PostgreSQL 18 manual, [Partial Indexes](https://www.postgresql.org/docs/18/indexes-partial.html), example 11.3: a partial unique index.
- PostgreSQL 18 manual, [CREATE TABLE](https://www.postgresql.org/docs/18/sql-createtable.html): CHECK constraints are tested in the alphabetical order of their names.
