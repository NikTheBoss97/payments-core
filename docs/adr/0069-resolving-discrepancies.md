<!-- Author: Nicola Antonello -->

# ADR-0069: Resolving discrepancies

- Status: accepted
- Date: 2026-11-24

## Context

Booking records each disagreement with the PSP as a discrepancy and holds its unexplained
money in `settlement_suspense` (ADR-0067, ADR-0068). The owner chose to resolve them in
phase 6, not only to list them. Two actors can then move the same money: an operator,
through routes, and the job, as the evidence arrives.

Some disagreements are timing differences, which the other side's record will settle: in
a bank reconciliation, a deposit in transit is on the books before the bank records it,
and only the book side gets entries (OpenStax, 8.6). Others are errors, which no record
will settle. A recovery of an account written off is two entries: the write-off reversed,
then the payment recorded (OpenStax, 9.2). Without distributed transactions, a business
handles an error by writing it off, retrying or compensating (Hohpe). Clark-Wilson lets
data change only through well-formed transactions, each leaving a record (Anderson,
chapter 12).

The failure model assumes code that skips a payment's lock (DESIGN.md), so a guard that
keeps two actors from moving one amount twice must hold without a lock. A deferred check
sees only committed rows: two transactions that each read the other's work as absent both
pass, and both commit.

## Options

The outcomes:

| Option | Why not |
|---|---|
| **A fixed set, each with one shape of posting that the database can check** | Chosen |
| A claim or a write-off only | A fee off the terms could not be accepted, nor a missing batch closed, without moving money that never moved |
| A free adjustment, its accounts and amount chosen by the operator | The database cannot tell a right adjustment from a wrong one: the opposite of a well-formed transaction |

Who closes a discrepancy:

| Option | Why not |
|---|---|
| **The job closes a timing difference when its evidence arrives, and an operator decides the rest; evidence that comes after an operator's outcome reverses it** | Chosen |
| An operator closes every one, the job showing the evidence | A person adds nothing to a closure that the database proves: a line matched at booking needs nobody, while the same line a week late would need someone |
| As chosen, but evidence after an outcome is a new discrepancy | A line for a claimed movement could not come off `psp_receivable`, which the claim already cleared, so it would land in suspense although it agrees with our records, and need a second decision |

The operator's routes:

| Option | Why not |
|---|---|
| **A route per outcome; the same outcome again a 200; the open ones listed in pages with a cursor; a reason required** | Chosen |
| One route with the outcome in the body | Against the routes so far, each its own action, and the outcome becomes a label to validate |
| The same outcome again a 409 | Two operators, or a script that lost its key, would get a conflict for what they asked for |
| Every open one in one answer, or a capped page without a cursor | Unbounded when a PSP stops settling; or everything behind the first 100 hidden, such as overdue movements left open while the PSP is asked |
| A reason optional, or none | With one operator credential the record cannot say who decided, so it says why |

The guards against two actors closing one amount:

| Option | Why not |
|---|---|
| **A chain of steps per movement and per discrepancy, as a payment has** | Chosen |
| Locks, with deferred checks that read other rows | Two writers that skip the lock both pass the checks and both commit, and a movement's receivable is cleared twice |
| Unique keys on the closings | A row cannot be deleted, so a movement that an operator claimed could never be settled by its line |

## Decision

- **The outcomes, by kind of discrepancy**, each with one posting:

  | Kind | Outcomes | Their postings |
  |---|---|---|
  | A line that settles nothing of ours | claim, write-off | its net from `settlement_suspense` to `psp_claims`, or to `settlement_writeoffs` |
  | A payout that differs from its nets | claim, write-off | the difference, likewise |
  | A fee off the agreed terms | claim, accept | the difference from `psp_fees` to `psp_claims`; none |
  | An overdue movement | claim, write-off | its amount from `psp_receivable` to `psp_claims`, or to `settlement_writeoffs` |
  | A missing batch | accept | none |
  | A posting that nothing causes (ADR-0070) | accept | none |

  Two new kinds, seeded as ADR-0020 says: `psp_claims` (debit, party the PSP), what the
  PSP owes outside settlement, negative when we owe it; and `settlement_writeoffs` (debit,
  an expense, party `platform`), negative for a gain.
- **Timing differences close on their evidence, through the job:** a missing batch, when
  the batch is booked; an overdue movement, when its line comes in a later batch; a line
  whose fact or `settled` fact the PSP's history lacked, when the fact arrives, by a late
  match, only once the line agrees with our records, proven by the same foreign keys as a
  batch's line (ADR-0068). The late match moves the line's net out of suspense, books its
  fee to `psp_fees` and its gross off `psp_receivable`, and checks its fee against the
  agreed terms, recording a fee that differs as a batch's line would. No
  person settles a line late, as no person applies a fact (ADR-0034). Every other kind is
  an error, which waits for an operator.
- **An early outcome is reversed.** An operator may close a timing difference before its
  evidence arrives. If the evidence then arrives, the job reverses the outcome with its
  mirror posting, closes the discrepancy as the evidence says, and books as usual, in one
  database transaction. The batch's posting keeps one rule, so `LG009` never depends on
  outcomes.
- **The routes**, behind the operator's credential (ADR-0024), on `payments_app`, with keys
  in `operator_idempotency_records` (ADR-0026):
  - `POST /discrepancies/{id}/claim`, `/write-off` and `/accept`, each with an
    `Idempotency-Key` and a `reason` of 1 to 500 characters, which is kept with the outcome
    and shown in its history, not in the stored answer. The answer is a 200 with
    `discrepancy_id` and the outcome. The same outcome again, under any key, is a 200, since
    it already holds. Another outcome on a closed discrepancy, closed by an outcome or by its
    evidence, or an outcome that its kind does not allow, is a stored 409 with a problem type
    of its own. An unknown ID is a 404, not stored.
  - `GET /discrepancies` lists the open ones in ID order, in pages of 100 with an `after`
    cursor, each with its kind, PSP, currency, amount, what it concerns (a batch, a line, a
    movement or a ledger transaction), the outcomes that it allows and when it was found. A
    pass over the pages sees every discrepancy that was open when it began and still is when
    its page is read; one found meanwhile can sort before the cursor, since an ID is made
    before its transaction commits, and shows on the next pass.
  - `GET /discrepancies/{id}` answers one, open or closed, with its history: its outcome
    and reason, a reversal, its evidence.
  - The merchant's role has no privilege on any table of reconciliation.
- **Chains.** Every amount that two actors can move has a chain of steps, as a payment has
  (ADR-0017): a primary key of the subject and `seq`, a foreign key to the step before, and
  an edge check that carries the subject's kind, so that an outcome that a kind does not
  allow fails a named check. The state is the newest step, never a status column.
  - A movement: from its start to settled, settled late or overdue; from overdue to
    settled, settled late, claimed or written off; from claimed or written off to settled or
    settled late, carrying the outcome's reversal.
  - A discrepancy: from found to an outcome that its kind allows, or to its evidence; from
    an outcome to its evidence, carrying the reversal.
  - An overdue movement is a discrepancy whose chain is its movement's: the overdue step
    finds it, a claim or a write-off is its outcome, and a later settling step is its
    evidence.
  - A late match writes a step in both chains. A closing by evidence is a step that names
    its proof: the batch, the line, the late match.
  - Two writers of one step cannot both commit: the second gets `23505`, drops its
    transaction and reads again (ADR-0019). The same key keeps a movement from being found
    overdue while its line is booked.
- **Records hold the postings,** never steps, as a capture's record holds its posting
  (ADR-0021): an outcome, a reversal, which mirrors its outcome, or a late match. A record
  references, by foreign key, each step that it moves, one in each chain that it touches, so
  it cannot exist without them. **`LG011`** refuses a step that needs a record and has none,
  such as a settled step without its line, like `LG004`. **`LG010`**, once per record like
  `LG006`, refuses a record whose posting is not exactly its lines, written in the same
  database transaction. A batch's matched line and a late match each reference their
  movement's settling step, of which the chain allows one, so no fact is settled twice,
  whether by two batches or by a batch and a late match.
- **The balances.** With these guards, `settlement_suspense` holds the money of the open
  discrepancies that hold any, `psp_receivable` the movements neither settled, claimed nor
  written off, and `psp_claims` the open claims. Tests check those identities (ADR-0071). A
  constraint would sum a whole account at each commit and, under concurrent writers, miss
  one.

## Consequences

- Two chain tables, a step for each settled movement and one or two for each discrepancy,
  and tables for the records that post.
- A discrepancy's history says whether the job or the operator closed it, and why.
- An outcome that loses a race with its evidence is a 409; one that wins is reversed when
  the evidence lands.
- Later needs: an operator undoing an outcome of its own, by a mirror posting like the
  job's reversal; routes that list batches and their lines; a way to correct the ledger
  after a posting that nothing causes.

## References

- Ross Anderson, [Security Engineering, 3rd edition](https://www.cl.cam.ac.uk/archive/rja14/book.html) (Wiley, 2020), chapter 12 (the Clark-Wilson model).
- Mitchell Franklin, Patty Graybeal and Dixon Cooper, Principles of Accounting, Volume 1:
  Financial Accounting (OpenStax, 2019): [8.6, bank reconciliation](https://openstax.org/books/principles-financial-accounting/pages/8-6-define-the-purpose-of-a-bank-reconciliation-and-prepare-a-bank-reconciliation-and-its-associated-journal-entries)
  and [9.2, uncollectible accounts](https://openstax.org/books/principles-financial-accounting/pages/9-2-account-for-uncollectible-accounts-using-the-balance-sheet-and-income-statement-approaches).
- Gregor Hohpe, [Your Coffee Shop Doesn't Use Two-Phase Commit](https://www.enterpriseintegrationpatterns.com/docs/IEEE_Software_Design_2PC.pdf) (IEEE Software, 2005).
