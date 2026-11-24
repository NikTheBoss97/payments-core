<!-- Author: Nicola Antonello -->

# ADR-0071: What phase 6 promises, and how it is tested

- Status: accepted
- Date: 2026-11-24

## Context

Each invariant of this service has a stated enforcement and a test that tries to break it,
and the guards stand twice, in Rust and in PostgreSQL, so that a test can break each one
alone (DESIGN.md). ADR-0066 to ADR-0070 make three new promises possible: each batch booked
once and as reported, each disagreement recorded, and each discrepancy closed once.

The failure model takes the PSP's history to be true and never to change what it reported.
The settlement files are the PSP's second word, the account of what it paid, and checking
that account is what reconciliation is for. No source read says which word a real PSP
writes first; the one hint, that Stripe prepares its report data twice a day, points to
reports that follow the events.

The outbox's failure tests set the pattern: random runs from fixed seeds against a model,
over PostgreSQL and real HTTP, without shrinking, since a run reads the whole database and
a failure that hangs on thread timing may not recur (ADR-0065).

## Options

A file that runs ahead of the PSP's history:

| Option | Why not |
|---|---|
| **Assumed to happen: its line waits in suspense and settles late once the fact arrives** | Chosen |
| Assumed not to happen, with no late match | If it happened anyway, the line would wait for an operator whose only outcomes are wrong once the fact arrives, and its movement, with no line, would go overdue: two discrepancies and two wrong outcomes for one timing difference |

The new rows of the table of invariants:

| Option | Why not |
|---|---|
| **Three: booking, recording and closing** | Chosen |
| Two, recording and closing in one row | One promise with two families of guards, so a failing test would name less precisely what broke |
| One, that the ledger agrees with the PSP's batches | One row listing every guard of the phase |

The random runs:

| Option | Why not |
|---|---|
| **From fixed seeds against a model, over PostgreSQL and real HTTP, as ADR-0065's** | Chosen |
| proptest with shrinking | Each case would need a database of its own, and a failure that hangs on thread timing may not shrink |
| A pure model, without PostgreSQL or HTTP | Blind to the keys, the deferred checks and the races that the runs are for |

Where the faults come from:

| Option | Why not |
|---|---|
| **The fake builds each batch with its own fee code, and its handle plants a fault in one** | Chosen |
| The tests write the files | The test would compute the fee, so the server's formula would be checked against nothing independent |
| Faults in targeted tests only, honest batches in the random runs | No fault would meet a crash, a race or a restart |

When the model checks the balances:

| Option | Why not |
|---|---|
| **After every operation** | Chosen |
| At the end of a run, as ADR-0065 checks the outbox | There, a copy sent for no reason is allowed; here a posting is wrong whenever it lands, even if a later step cancels it |

What phase 6 leaves out:

| Option | Why not |
|---|---|
| **Merchant payouts, chargebacks and disputes, settlement reversals, and a bank statement** | Chosen |
| Settlement reversals brought in | A later word from the PSP that a payout failed, a new line and posting, and a mock that models it: worth it mostly beside a bank statement, which would show the failure itself |
| Chargebacks brought in | A fact kind, a payment state and its moves, a posting and a line kind: work in phases 2, 4 and 6 for a feature that the scope does not list |

## Decision

- **The failure model** assumes settlement batches published late, out of order or past a
  gap, listed again, or as files that cannot be read; batches that disagree with our
  records; a movement that the PSP never settles; an operator's outcome on a discrepancy
  while the job closes it; and a file that runs ahead of the PSP's history. It assumes that
  the PSP never changes a batch once it could be read; one that cannot be read is read again
  until it can (ADR-0070). The PSP's history stays its true word on each payment (I8).
- **I11, new:** "Every batch that the PSP publishes is booked once, whole and as reported,
  once it can be read: each of its lines settles the one movement of ours that it agrees
  with, or goes to suspense as a discrepancy, and no movement is settled twice." In
  PostgreSQL, the batch's key; each matched line's foreign keys to its fact, its request's
  decision, its currency and a capture's `settled` fact, unique per fact; `LG009` once per
  batch, with each line tied to its batch's transaction; and one settling step per movement
  (ADR-0067, ADR-0068, ADR-0069). In Rust, the matching and the convergence by the batch's
  key (ADR-0070).
- **I12, new:** "Every disagreement is recorded once: a line that settles none of our
  movements, a fee off the agreed terms, a payout that differs from its lines, a missing
  batch, a movement left unsettled past the agreed delay, and a ledger transaction that no
  record causes." In Rust, the matching, the overdue check as of the last complete pass,
  and the query of postings that nothing causes (ADR-0068, ADR-0070). In PostgreSQL, a key
  on each discrepancy's subject, and the movement's chain, which records it overdue once.
- **I13, new:** "Each discrepancy closes only by its evidence or by one operator's outcome,
  which that evidence reverses if it comes later; so `psp_receivable`,
  `settlement_suspense` and `psp_claims` each hold exactly what is still open." In
  PostgreSQL, the chains with each kind's outcomes in their edge checks, `LG010` and
  `LG011` (ADR-0069). In Rust, the evidence step at every pass (ADR-0070). The balances
  hold by construction, since every posting to those accounts is fixed by `LG006`, `LG009`
  or `LG010`, and the tests check them; a posting that nothing causes falls outside them,
  and is I12's to record.
- **I7, extended:** a merchant is told nothing of reconciliation: the merchant's role has no
  privilege on its tables. I8 stays as it is, since booking writes no payment step.
- **The tests that try to break them:**
  - random runs from fixed seeds against a model, over PostgreSQL and real HTTP: the
    fake's settlement runs, some with a planted fault; the reconciler's passes, some killed
    at a crash point; operators' outcomes, some racing the booking that closes them;
    restarts of the task; the fake's listing or files failing for a while; time moving on;
    and postings that nothing causes, written in raw SQL. The fake's handle plants one fault
    in a batch before it is published: a line altered, added, repeated or withheld, a fee or
    the payout shifted, a number skipped, a file made unreadable, or a line published before
    its `settled` fact. A number skipped or a line withheld is drawn as published later in
    the run or never. The model knows what each fault must cause, from the fault and the
    run's timing. After every operation the balances of I13 hold. At the end of a run, with
    the fake answering, what it held back published unless drawn as never, every file
    readable, time past every deadline, and passes run until nothing changes within a
    bound: every batch is booked once; the discrepancies recorded are exactly those that the
    model predicts, each once, and each posting that nothing causes is recorded once; each
    timing difference whose evidence was published is closed by it, its early outcome
    reversed; the rest are open, or closed by the run's outcomes. Across the runs, every
    kind of fault, outcome and failure must come up;
  - the backend killed before and after each new insert, and during the `COMMIT`, of a
    booking, an outcome, the evidence step and the overdue step, each test ending with the
    books whole;
  - each database guard broken alone, in raw SQL as the system role, and identified by its
    SQLSTATE and constraint: a line that disagrees with its fact, request or currency; a
    fact settled twice; a capture's line without its `settled` fact; a posting that is not
    its batch's lines (`LG009`); a line written in a later transaction than its batch's; a
    step whose posting is not its lines (`LG010`); a step without its record (`LG011`); an
    outcome that its kind does not allow; a second closing; a row changed or deleted;
  - planted bugs in the pull requests, each caught by a named test: the matching loosened,
    the evidence step removed, a reversal skipped;
  - races forced into each order by a lock that the test holds: an outcome against the
    booking that closes its discrepancy, and two passes booking one batch;
  - every move of both chains, in Rust and against the database, as I5's are tested;
  - the operator's routes tried for each answer, and with a merchant's credential;
  - catalog tests of the new tables' keys, privileges and triggers, with their `ENABLE
    ALWAYS`;
  - the server's binary: the reconciler's task ends, and the server goes on taking and
    sending payments.
- **Not built:** merchant payouts, and so collecting what a merchant owes after a refund in
  full; chargebacks and disputes; settlement reversals, a payout that fails after the PSP
  reported it; and a bank statement, against which payouts would be reconciled (ADR-0067).
  `docs/DESIGN.md`'s non-goals gain the first three as one entry and the bank statement as
  another.
- **Pages.** `docs/correctness/reconciliation.md` names the tests of each invariant when
  they land, and the README gains its rows.

## Consequences

- Phase 6 has three invariants to finish, and I7's extension. The README and
  `docs/DESIGN.md` mark them in progress until their tests land.
- What these tests do not show: a real PSP's formats and timing; a bank statement; clocks
  that disagree, which ADR-0070 argues cannot move money; two servers running at once,
  beyond two passes racing; and speed, which is phase 8's.
- The fake PSP gains fee code of its own and a way to plant faults.

## References

- John Hughes, [Experiences with QuickCheck: Testing the Hard Stuff and Staying Sane](https://publications.lib.chalmers.se/records/fulltext/232550/local_232550.pdf) (2016): testing a stateful system against a model of its state.
