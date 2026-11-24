<!-- Author: Nicola Antonello -->

# ADR-0067: Booking a batch

- Status: accepted
- Date: 2026-11-24
- Supersedes: in ADR-0011's Decision, "When one posting covers several business objects,
  such as a settlement batch, each object references its own entry, `ledger_entries
  (transaction_id, line_no)`", and in its amendment of 2026-10-28, "and a phase-6 batch
  posting one per entry"; in ADR-0019's Decision, "and phase 6 settles a batch, in the same
  transaction as the steps (invariant I6)"; in ADR-0021's Decision, "A phase-6 batch posting
  will have one cause per entry: each settlement row references `ledger_entries
  (transaction_id, line_no)` under a unique key of its own, with an `LG006`-style check of
  the entry (ADR-0011)".

## Context

A capture debits `psp_receivable` with its amount and a refund credits it, so the account
holds what the PSP owes the platform, gross (ADR-0020). A batch (ADR-0066) says which of
those movements the PSP has settled, what it charged for each, and what it paid out.

Three accepted records planned how a batch would be booked. ADR-0011 and ADR-0021 gave a
batch's posting one cause per entry, each settlement row referencing its own ledger entry
under a check like `LG006`. ADR-0019 planned that phase 6 would settle a batch in the same
transaction as the payments' steps. Since phase 4 only the catch-up writes a step of the
PSP's, from a fact of its history (ADR-0034), so a batch cannot write steps.

The accounting sources set the shape. In a subsidiary ledger the detail lives in its own
records, and their total must equal the control account in the general ledger (OpenStax,
7.2). Books record a deposit before the bank does, and a bank reconciliation explains the
difference (OpenStax, 8.6); here no bank statement exists, and at Stripe a payout reported
as paid can still fail later. IFRS 18 forbids offsetting income against expenses unless a
standard requires or permits it (paragraph 44). At Stripe's destination charges the
platform pays Stripe's fee. A suspense account holds an entry until its correct account is
known (ACCA).

## Options

How the ledger records a batch:

| Option | Why not |
|---|---|
| **One ledger transaction per batch, one entry per account, the payout to `psp_payouts`** | Chosen |
| The same, with the payout to `bank` | It would claim that the cash arrived, which no document that we hold says. Chosen for later, once a bank statement joins the scope |
| One transaction with entries per line | The ledger would copy the lines' detail without saying which line each entry is for, while the batch's lines already hold that detail. This was the plan of ADR-0011 and ADR-0021 |
| Two transactions through a clearing account, the settlement and then the payout | The second would have no cause until a bank statement exists |

Who bears the PSP's fee:

| Option | Why not |
|---|---|
| **The platform, as an expense of its own kind** | Chosen |
| The platform, netted against `fee_revenue` | It offsets an expense against income, which IFRS 18 forbids, and breaks the identity that `fee_revenue` equals the platform's fees charged on captures |
| The merchant | It changes the merchant's share after capture, where ADR-0020 fixes it |

A line that settles nothing of ours (ADR-0068):

| Option | Why not |
|---|---|
| **Booked as the PSP reports it, the net that reached us to `settlement_suspense`, its fee booked only once the line matches, and recorded as a discrepancy** | Chosen |
| The same, but its gross to `settlement_suspense` and its fee to `psp_fees` as charged | A claim would then say that we owe the PSP the gross, though it paid us the net, and a fee that may not be ours would stand as an expense that nothing records or checks |
| The whole batch held until an operator decides | The books then miss the payout and keep every movement of the batch in the receivable, not only the bad one, and later batches wait or go out of order |
| Only the matched lines booked, with the payout cut to what they explain | The books then disagree with what the PSP paid |
| The file refused | Right for a file that cannot be read (ADR-0066), wrong for one that the PSP stands by |

How the database checks a booking:

| Option | Why not |
|---|---|
| **`LG009` once per batch at `COMMIT`, with every line tied to its batch's transaction** | Chosen |
| `LG009` on the batch row's insert alone | It never sees a line inserted for the batch in a later transaction, so the lines and the posting could drift apart |
| `LG009` on every line's insert as well | It sums the batch again for each line: quadratic at `COMMIT` |
| Rust alone, or stored totals with `CHECK`s | Rust alone is not a second guard, and a `CHECK` sees one row |

## Decision

- **The posting.** One ledger transaction per batch, in its currency, written with `post()`
  in the booking's database transaction (ADR-0011), with one entry per account:
  - `psp_payouts` debited with the payout;
  - `psp_fees` debited with the sum of the matched lines' fees;
  - `psp_receivable` credited with the sum of the matched lines' gross, a refund's being
    negative;
  - `settlement_suspense` credited with the sum of the unmatched lines' nets, plus the
    payout less the sum of all the nets;
  - an entry whose amount is zero left out, since the ledger refuses one.

  Example: two captures of 10,000 and 4,000 with fees of 165 and 81, a refund of 2,500, and
  a line of 500 with a fee of 32 and a net of 468 that settles nothing of ours, so a payout
  of 11,722. The batch books `psp_payouts` +11,722, `psp_fees` +246, `psp_receivable`
  -11,500 and `settlement_suspense` -468.
- **Three kinds**, each seeded by the migration that first posts to it (ADR-0020):
  `psp_payouts` (debit, party the PSP), the payouts that the PSP reports sending, which no
  bank statement confirms; `psp_fees` (debit, an expense, party the PSP); and
  `settlement_suspense` (credit, party the PSP), money that the PSP reported and that
  nothing of ours explains yet. ADR-0069 adds the kinds that resolving discrepancies needs.
- **The batch is its posting's one cause.** A batch row holds its ID, a UUIDv7 made in
  Rust; its PSP, currency and number, unique together; its settlement time, a
  `BusinessTime` (ADR-0059); and its payout. A row of its own links the batch to its
  ledger transaction, each under a unique key, as each record holds its posting
  (ADR-0021). That row exists exactly when some entry would not be zero: a batch whose
  entries would all be zero, such as a capture with no fee that is refunded in full in
  the same batch, books no posting, since a ledger transaction needs two entries and none
  may be zero. The batch's lines are its detail: lines that settle a capture, lines that
  settle a refund, and lines that settle nothing of ours (ADR-0068).
  Each line is tied to its batch's transaction by an `xid8` column with the default
  `pg_current_xact_id()`, which no role may insert into, and a composite foreign key to the
  batch's `(id, xact_id)`, as a delivery is tied to its event (ADR-0052). No line can join
  a batch after its transaction.
- **`LG009`** is a deferred constraint trigger on the batch row, built like `LG006`. At
  `COMMIT` it derives the expected entries from the batch's lines and payout, drops those of
  zero, and refuses the batch unless it has a posting exactly when some entry remains, and
  that posting's header was written by the same database transaction (its `xmin`), in the
  batch's currency, with exactly those entries, compared with `EXCEPT ALL` both ways. The ledger's `LG003` then requires them to balance.
  It reads each line once, so a batch costs linear time, as ADR-0004 intended.
- **The platform bears the PSP's fee** on its own movements, as an expense. `fee_revenue`
  keeps the platform's fees charged on captures, each tied to its record by `LG006`, and
  each merchant's share stays as fixed at capture (ADR-0020). A line that settles nothing of
  ours has no movement to bear a fee: suspense holds the net that reached us, and the fee is
  booked if and when the line matches (ADR-0069).
- **Booking writes no payment step.** A payment's `settled` step comes from the PSP's
  `settled` fact through the catch-up, so a batch books money, and the PSP's history moves
  the payment.

## Consequences

- `psp_receivable` falls by what the PSP settled, and `psp_payouts` keeps every payout. It
  is the account that a bank statement would clear, payout by payout, to `bank`. A later
  need: when a bank statement joins the scope, payouts are booked to `bank` and `bank` is
  reconciled against it, the second option above.
- A batch adds three or four entries to the ledger whatever its size. The detail stays in
  its lines, whose totals `LG009` proves equal to the entries, as a subsidiary ledger's
  total equals its control account.
- A capture refunded in full before it settles has a capture line and a refund line. Its
  PSP fee shows as an expense, while `fee_revenue` keeps the platform's fee: a capture of
  1,000 with the platform's fee 25 and the PSP's 39, refunded in full, books `psp_payouts`
  -39 and `psp_fees` +39, a loss of 14 shown as income of 25 and an expense of 39.
- ADR-0021's query of postings that nothing causes gains the batches as a cause
  (ADR-0070).

## References

- Mitchell Franklin, Patty Graybeal and Dixon Cooper, Principles of Accounting, Volume 1:
  Financial Accounting (OpenStax, 2019): [7.2, special journals and subsidiary ledgers](https://openstax.org/books/principles-financial-accounting/pages/7-2-describe-and-explain-the-purpose-of-special-journals-and-their-importance-to-stakeholders)
  and [8.6, bank reconciliation](https://openstax.org/books/principles-financial-accounting/pages/8-6-define-the-purpose-of-a-bank-reconciliation-and-prepare-a-bank-reconciliation-and-its-associated-journal-entries).
- IFRS Foundation, [IFRS 18 Presentation and Disclosure in Financial Statements](https://www.ifrs.org/content/dam/ifrs/publications/html-standards/english/2026/issued/ifrs18.html), paragraph 44.
- ACCA, [Suspense accounts and error correction](https://www.accaglobal.com/uk/en/student/exam-support-resources/foundation-level-study-resources/ffa/ffa-technical-articles/suspense-accounts-error-correction.html).
- Stripe, [Create destination charges](https://docs.stripe.com/connect/destination-charges), and [Payouts](https://docs.stripe.com/api/payouts) in the Stripe API reference.
