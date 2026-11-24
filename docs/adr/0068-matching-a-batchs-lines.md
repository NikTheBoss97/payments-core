<!-- Author: Nicola Antonello -->

# ADR-0068: Matching a batch's lines

- Status: accepted
- Date: 2026-11-24

## Context

Each line of a batch names a payment, the PSP's fact of the movement that it settles, and
the request that the movement answered (ADR-0066). What we hold to compare it with:

- `payment_facts`, unique on `(psp, psp_fact_id)`, on `(payment_id, position, kind)` and
  on `(payment_id, position, kind, amount)`, with positive amounts for captures and
  refunds; the link of each step to its fact shares the kind and the amount, so a step and
  its fact agree on both, and a fact makes at most one step (ADR-0037, ADR-0045);
- `payment_request_decisions`, keyed by `request_id` and unique on `(payment_id,
  position)`: a request is decided by at most one fact;
- `payments`, with keys on `(id, amount)`, `(id, psp)` and `(id, merchant_id)`, each added
  so that another table could carry a column across.

The PSP has two words about a settlement: its history's `settled` fact, which the catch-up
applies as the `settle` step, or records without a step on a payment refunded in full
first (ADR-0016), and the file's line. Facts reach us only through the catch-up, which reads
the PSP's history (ADR-0031). ADR-0022 expected phase 6 to match lines by business times;
the PSP's fact IDs match them exactly.

## Options

When a line that names a movement we do not hold is a discrepancy:

| Option | Why not |
|---|---|
| **After the batch's payments are caught up: a fact still missing once the PSP's history was read** | Chosen |
| Held and retried until an age passes | A clock would decide what the PSP's history can tell |
| At once | Every capture settled near the run's cutoff, whose notification has not reached us yet, would be a false alarm |

What a matched line references:

| Option | Why not |
|---|---|
| **The PSP's fact, by its ID with the payment, position, kind and amount in one foreign key** | Chosen |
| The payment's step, by its step key | Two refunds of one amount on one payment look the same to the database, so a line could settle the wrong one, and the other's line would then look like a second settlement |
| The capture or refund record | Two line tables, and the same gap as the step |

What else a matched line must agree with: its request, its currency, a capture's
`settled` fact, and its fee:

| Option | Why not |
|---|---|
| **Foreign keys for the request, the currency and the `settled` fact, and the fee checked against the terms agreed with the PSP** | Chosen |
| The same checks in Rust alone | SQL that skips Rust could store as matched a line naming another request or currency, or a capture that the PSP's history never settled |
| None, or the fee accepted as charged | A payment could show as settled while its money never arrives, money could arrive for a payment shown as captured, and an overcharge would go unnoticed |
| The fees compared in total per batch | An overcharge on one payment would hide behind an undercharge on another |

A movement that no batch settles:

| Option | Why not |
|---|---|
| **Overdue once an agreed delay has passed since its business time** | Chosen |
| Overdue after a count of later batches | Silent when the PSP stops sending batches, which is when it matters, and slow for a currency with few |
| Never | A settlement that never comes would never be noticed |

## Decision

- **Catch up first, then match.** Before booking a batch, the job catches up each payment
  that a line names and whose named fact, or whose `settled` fact for a capture's line, it
  does not hold, through the catch-up, its fourth caller (ADR-0031). Then it matches. A fact
  still missing once the PSP's history was read leaves the line settling nothing of ours
  for now: the file can run ahead of the history (ADR-0071), and a late match settles it
  when the fact arrives (ADR-0069). A payment that we do not know has no history to read,
  so its line settles nothing of ours at once. A history that cannot be read leaves the
  batch for the next pass (ADR-0070).
- **Foreign keys prove a matched line.**
  - Its fact, through `(psp, psp_fact_id, payment_id, position, kind, amount)`, to a new
    unique key on `payment_facts`, so the line's payment, kind and amount are the fact's.
    The lines are unique on `(psp, psp_fact_id)`, so a fact settles once. With the link of
    a step to its fact, the record's key and `LG006`, every hop from a line of the file to
    an entry of the ledger is a constraint.
  - A capture's line also references its payment's `settled` fact, through
    `payment_facts_kind_key (payment_id, position, kind)`, unique among capture lines, so
    a capture is settled in the books only if the PSP's history says that it settled, and
    one `settled` fact backs one line. A refund has no such fact, so matched lines take two
    tables, one for captures and one for refunds, since a foreign key names one table.
  - Its request, through `(request_id, payment_id, position)` to the decision that its
    fact made, a new key on `payment_request_decisions`.
  - Its currency, to its batch's, and to its payment's through a new key `payments (id,
    currency)`, as an entry repeats its transaction's currency (ADR-0010).

  A line that fails any of them settles nothing of ours.
- **The agreed terms.** An append-only table holds, per PSP and currency, the terms agreed
  with the PSP and the time from which each holds: the fee's rate in basis points and its
  fixed part, and the delay within which a movement must settle. A migration seeds the
  mock's terms. The job checks the fee of each line that matches, at booking or at its late
  match (ADR-0069), against the terms in force at its movement's business time
  (`captured_at` or `refunded_at`), with code of its own (ADR-0029). A fee that differs is
  booked as charged (ADR-0067), since the PSP took it, and recorded as a discrepancy: money
  to claim from the PSP, not money unexplained. A line that settles nothing of ours has no
  movement to choose the terms by, so its fee is neither booked nor checked until it
  matches.
- **The discrepancies found at booking**, each recorded once under a key on its subject,
  in the booking's database transaction:
  - a line that settles nothing of ours: a payment or a fact that we do not hold, a line
    that disagrees with its fact, its request or its currency, a fact settled a second
    time, or a capture whose `settled` fact we do not hold; its net is in suspense
    (ADR-0067);
  - a fee that differs from the agreed terms, on a line that matches;
  - a payout that differs from the sum of its batch's nets, the difference in suspense.

  The job's pass also records missing batches, a number that the listing skips or a file
  that cannot be read, and overdue movements (ADR-0070).
- **Overdue.** A movement, a capture or a refund that the catch-up applied, that no line
  has settled once the agreed delay has passed since its business time, is recorded once
  as overdue. A line in suspense that names it has not settled it: if its fact is still
  missing at the deadline, the movement is overdue as well, and the late match closes both
  discrepancies (ADR-0069). Nothing is booked for an overdue movement: it stays in
  `psp_receivable`, which is true, since the PSP still owes it. The time it is judged against is an input, as ADR-0053's three days are,
  never a clock read inside the step and never `recorded_at` (ADR-0012); ADR-0070 says
  which time.

## Consequences

- Three new keys on earlier tables, on `payment_facts`, `payment_request_decisions` and
  `payments`, each already unique through a narrower key, so each costs an index.
- Booking's catch-ups spend the bucket that the sender's task shares (ADR-0070).
- A batch's lines reach the ledger only through constraints: a line that the database
  stores as matched agrees with the PSP's history on the payment, the movement, its kind,
  amount, request and currency.
- Business times choose the agreed terms and count the delay; no line is matched by time.
- A later need: a route for the operator to add terms agreed with a PSP.
