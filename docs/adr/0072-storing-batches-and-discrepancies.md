<!-- Author: Nicola Antonello -->

# ADR-0072: Storing batches and discrepancies

- Status: accepted
- Date: 2026-11-24

## Context

ADR-0066 to ADR-0071 decide what phase 6 keeps: the terms agreed with a PSP, each batch
with its lines and its posting, which lines match a movement of ours, the chains of
movements and of discrepancies, and the records that post when an operator or the evidence
closes one. They leave the tables open.

The schema's rules shape the answer. Every column is `NOT NULL` (ADR-0017), so a row that
may or may not have a companion keeps it in a table of its own. A foreign key can prove a
copied column true (ADR-0037), and so carry a kind or an amount from one row to the next. A
chain's first step points to itself, and each step's foreign key to its predecessor carries
the predecessor's state, so that an edge check can trust it (ADR-0017). A record's primary
key is its step's ID, its `kind` is generated, and deferred checks refuse a step without its
record (`LG004`) and a record whose posting is not exactly its lines (`LG006`, ADR-0021). A
row tied by a composite foreign key to its parent's `(id, xact_id)` can only be written in
the parent's transaction (ADR-0052, ADR-0055). A row that commits with another has no
`recorded_at` of its own (ADR-0037).

## Options

How a batch's lines are stored:

| Option | Why not |
|---|---|
| **One table of every line, and two tables of matches, one for captures and one for refunds, keyed by the line** | Chosen |
| Three full tables: capture lines, refund lines, lines that settle nothing of ours | Nothing would stop one line's number from standing in two of them, and `LG009` would sum three tables |

Where a movement's chain starts:

| Option | Why not |
|---|---|
| **A first step, `open`, written by the first writer that moves the movement, in that writer's transaction** | Chosen |
| The catch-up writes it as it applies a capture's or a refund's fact | Phase 4's catch-up, its crash tests and the code behind I8 would change |
| No state in the predecessor's key | The edge check would trust whatever state a writer claims |

How the six kinds of discrepancy are stored:

| Option | Why not |
|---|---|
| **One table of every discrepancy, and one table per kind for its subject** | Chosen |
| One table per kind, none in common | The listing would union six tables, and a discrepancy's chain could not name its discrepancy by one foreign key |

How an outcome holds a posting that only some outcomes have:

| Option | Why not |
|---|---|
| **One table of outcomes, and a row linking an outcome to its posting, present exactly when its kind posts** | Chosen |
| One table per kind of outcome | Three tables for one family of routes, which a discrepancy's history would join |

The owner chose the first of each.

## Decision

- **The tables.** All are append-only (`LG001`), named with every constraint, read by
  `payments_app` and written by it alone, granted by column; the merchant's role has no
  privilege on any, so none is under row-level security. `psp_terms` is written only by
  migrations. Amounts stay within the ledger's bound of 10^15 (ADR-0003), and business
  times within RFC 3339's years (ADR-0059).

  Booking's tables, the first pull request:

  | Table | One row per | Key and guards |
  |---|---|---|
  | `psp_terms` | PSP, currency and start | `(psp, currency, starts_at)`; the fee's rate in basis points, 0 to 10,000, and fixed part; the delay within which a movement settles, `settle_within`, in seconds, 1 to 366 days |
  | `settlement_batches` | batch | `id`; `(psp, currency, number)` unique; the settlement time and the payout; `xact_id`; `(id, xact_id)`, `(id, psp)` and `(id, currency)` unique, the targets of its rows' keys |
  | `settlement_batch_postings` | batch that posts | `batch_id`; its ledger transaction, unique; tied to its batch's transaction |
  | `settlement_lines` | line of a file but the payout's | `(batch_id, line_no)`, the line's number in the file, the header being line 1; `kind` `capture` or `refund`, with the gross's sign; the fee; the net, which a CHECK keeps equal to the gross less the fee; tied to its batch's transaction |
  | `settlement_capture_matches` | capture line that settles a capture of ours | `(batch_id, line_no)`, with its kind and gross carried from the line; the fact, the `settled` fact, the request's decision, the currency and the movement's `settle` step, each by a foreign key of its own (ADR-0068); `(psp, psp_fact_id)`, `(payment_id, settled_position)` and `step_id` unique; tied to its batch's transaction |
  | `settlement_refund_matches` | refund line that settles a refund of ours | as the captures', without a `settled` fact; a generated amount, the gross negated, carries the fact's amount |
  | `settlement_movement_steps` | step of a movement's chain | `(payment_id, position, seq)`, the movement being its capture's or refund's fact, whose kind and amount the step carries by a foreign key; `id`, unique; the predecessor's key and the edge check below |
  | `settlement_discrepancies` | discrepancy | `id`; its `kind`, one of six; `(id, kind)` unique, the target of its subject's and its steps' keys |
  | `settlement_unmatched_lines` | line that settles nothing of ours | `discrepancy_id`; `(batch_id, line_no)` unique, with the line's net carried across; tied to its batch's transaction |
  | `settlement_payout_differences` | payout that differs from its nets | `discrepancy_id`; `batch_id` unique; the difference, not zero; tied to its batch's transaction |
  | `settlement_fee_differences` | fee off the agreed terms, on a line that matches | `discrepancy_id`; `(batch_id, line_no)` unique, with the line's fee carried across as charged; the agreed fee, which differs |
  | `settlement_missing_batches` | missing batch | `discrepancy_id`; `(psp, currency, number)` unique; the cause, `not_listed` or `unreadable` |
  | `settlement_overdue_movements` | overdue movement | `discrepancy_id`; `(payment_id, position)` unique; the movement's `overdue` step; the as-of time that it was judged at (ADR-0070) |
  | `settlement_uncaused_postings` | posting that nothing causes | `discrepancy_id`; its ledger transaction, unique |
  | `settlement_discrepancy_steps` | step of a discrepancy's chain | `(discrepancy_id, seq)`, carrying the discrepancy's kind by a foreign key; `id`, unique; the predecessor's key and the edge check below |

  Resolving's tables, the second pull request:

  | Table | One row per | Key and guards |
  |---|---|---|
  | `settlement_late_capture_matches`, `settlement_late_refund_matches` | line that settles late (ADR-0069) | `(batch_id, line_no)`, through its unmatched line's discrepancy; the foreign keys of a match; the movement's `settle_late` step and the discrepancy's `evidence` step; its posting, unique |
  | `settlement_movement_outcomes` | claim or write-off on an overdue movement | `step_id`, the movement's step; the reason; its posting, unique |
  | `settlement_discrepancy_outcomes` | outcome on any other discrepancy | `step_id`, the discrepancy's step; `claim`, `write_off` or `accept`; the reason |
  | `settlement_discrepancy_outcome_postings` | claim or write-off on one | `step_id`; its posting, unique |
  | `settlement_movement_reversals`, `settlement_discrepancy_reversals` | outcome that evidence reversed | `step_id`, the step that carries it; the outcome's step, unique; its posting, unique |
  | `settlement_batch_arrivals` | missing batch that arrived | `step_id`, the discrepancy's `evidence` step; the batch, by `(psp, currency, number)` |

- **Earlier tables gain three keys,** each the target of a match's foreign key and each
  unique already through a narrower key: `payment_facts (psp, psp_fact_id, payment_id,
  position, kind, amount)`, `payment_request_decisions (request_id, payment_id, position)`
  and `payments (id, currency)`.
- **Account kinds:** `psp_payouts`, `psp_fees`, `psp_claims` and `settlement_writeoffs`
  are debit-normal, `settlement_suspense` credit-normal, seeded with the first table.
- **The mock's terms** are seeded for the six currencies of `currencies`: 140 basis points
  and 25 minor units on each capture, settling within two days, from RFC 3339's year 0000.
- **The chains.** A movement's states are `open`, `settled`, `overdue`, `claimed` and
  `written_off`; its first step, `open`, goes from `open` to `open`, and then:

  | From | Kinds of step |
  |---|---|
  | `open` | `settle` or `settle_late` to `settled`; `overdue` to `overdue` |
  | `overdue` | `settle` or `settle_late` to `settled`; `claim` to `claimed`; `write_off` to `written_off` |
  | `claimed`, `written_off` | `settle` or `settle_late` to `settled` |

  A discrepancy's states are `open`, `claimed`, `written_off`, `accepted` and `closed`; its
  first step, `find`, goes from `open` to `open`, and the edge check, carrying its kind,
  allows a `claim` or `write_off` for a line that settles nothing of ours or a payout's
  difference, a `claim` or `accept` for a fee's, an `accept` for a missing batch or a
  posting that nothing causes, and an `evidence` step to `closed` from `open` or after an
  outcome for a line and a missing batch. An overdue movement's discrepancy has no steps
  of its own: its chain is its movement's (ADR-0069). The first pull request allows only the
  first steps, `settle` and `overdue`; the second adds the rest, keeping each check's name.
- **The checks at `COMMIT`,** each a deferred constraint trigger, `ENABLE ALWAYS`:
  - `LG009`, on a batch: its posting link exists exactly when an entry remains, the
    posting's header was written by this transaction, in the batch's currency, and its
    entries are exactly those that ADR-0067 derives, a line counting as matched when a match
    names it;
  - `LG011`, on a step: a step that needs a record has one, such as a `settle` step its
    match and an `overdue` step its subject;
  - `LG012`, on a discrepancy: it has its subject, and, unless it is an overdue movement,
    its first step, as `LG005` refuses a payment without its first step;
  - `LG013`, on a fee's difference: its line has a match, since only a line that matches
    has its fee booked and checked (ADR-0068); the second pull request adds late matches;
  - `LG010`, on each record that posts, in the second pull request: its posting is exactly
    its lines, as `LG006` checks a capture's.
- **`xact_id`** is `xid8`, `NOT NULL DEFAULT pg_current_xact_id()`, and no role may
  insert into it (ADR-0055). Lines, matches, the posting link, and the subjects of a line
  that settles nothing and of a payout's difference carry it, tied to their batch's, so
  none can join a batch after its transaction and escape `LG009`.

## Consequences

- Two pull requests: booking's tables, with `LG009`, `LG011` for their steps, `LG012` and
  `LG013`; then resolving's, with `LG010` and the rest of the moves. The library's booking
  can start after the first.
- About twenty-three tables, each small, each following a pattern that the schema already
  uses; outcomes, reversals and late matches take two tables each, one per chain, so that
  every posting belongs to one record.
- Tests write every table with raw SQL as the system role, break each guard alone and
  assert its pair of SQLSTATE and constraint name; the catalog test pins the columns,
  constraints, defaults, triggers and grants.
- ADR-0013's registry gains `LG009`, `LG011`, `LG012` and `LG013` with the first pull
  request and `LG010` with the second, each in the commit that first raises it, and so does
  the one function that reads a `COMMIT`'s error, since each is a deferred check that proves
  a rollback (ADR-0014). `LG012` and `LG013` are codes that ADR-0069 did not list.

## Amendments

- 2026-11-25: a discrepancy's outcome carries `xact_id`, and the link to its posting is tied
  to it, so that no link joins an outcome after its transaction, as a batch's link is tied
  to its batch. `LG010` refuses a link where no entry remains, as `LG009` does, which a claim
  on a line whose net is zero meets. `LG011` gains a second trigger, on a discrepancy's
  chain.
- 2026-11-25: `settlement_unmatched_lines` gains `(discrepancy_id, batch_id, line_no)`
  unique, the target of a late match's key, which ties the late match to its line's
  discrepancy.
- 2026-11-25: `settlement_movement_outcomes` gains `(step_id, payment_id, position)` unique
  and `settlement_discrepancy_outcomes` `(step_id, discrepancy_id)` unique, the targets of a
  reversal's key to an outcome of its own subject. A discrepancy's reversal also names its
  outcome's posting link, so that only an outcome that posted is reversed, and a movement's
  reversal carries its step's kind, `settle` or `settle_late`.
- 2026-11-25: `settlement_missing_batches` gains `(discrepancy_id, psp, currency, number)`
  unique, the target of an arrival's key to the batch that its discrepancy missed; the
  arrival also names the booked batch by its `(psp, currency, number)`.
