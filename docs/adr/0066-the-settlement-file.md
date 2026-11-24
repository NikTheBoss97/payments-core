<!-- Author: Nicola Antonello -->

# ADR-0066: The settlement file

- Status: accepted
- Date: 2026-11-24

## Context

Phase 6 compares the ledger against what the PSP says it paid. ADR-0020 decided that
settlements are booked in this phase, net and in batches, since a PSP settles that way and
gross postings per movement would not match its file, and that the PSP's own fees get
account kinds of their own. A fact keeps no business time (ADR-0037), so what phase 6 needs
to know about a settlement, such as its date, comes from the file. Today the mock PSP
settles a capture as a `settled` fact with no data, one payment at a time or every capture
made by a cutoff, and has no fees, batches, payouts or files.

Two PSPs' reports show the usual shape. Adyen's settlement details report has a line per
entry, with gross and net amounts, the commission, the PSP's reference and the merchant's,
and a batch number that is the settlement's sequence number; the payout to the bank account
is a line of its own, of the journal type `MerchantPayout`. Stripe's payout reconciliation
report itemizes every payment, refund, dispute, fee and other balance transaction that a
payout includes. Adyen announces a new report with a `REPORT_AVAILABLE` webhook that tells
where to download it; Stripe builds a report on request, from data that it prepares twice a
day.

## Options

What a line holds:

| Option | Why not |
|---|---|
| **Each settled movement, a capture or a refund, with its gross, the PSP's fee and the net, and the payout as a line of its own** | Chosen |
| The payout alone, one amount per batch, as a bank statement shows it | A shortfall could be a fee, a missing refund or both, and nothing could tell which |
| Each movement without its fee, the fees showing only in the payout's shortfall | A batch's fees become one lump, so one wrong fee hides among the rest |

The mock's fee:

| Option | Why not |
|---|---|
| **A rate plus a fixed part on each capture, nothing on a refund, kept when the capture is refunded** | Chosen |
| A rate alone | No capture would ever net below zero, a case that real fees produce |
| A fee on refunds as well | A second rule for a case that neither report shows |

Batches:

| Option | Why not |
|---|---|
| **One per currency per settlement run, numbered in sequence per currency, with a settlement time** | Chosen |
| One across currencies | A ledger transaction holds one currency (ADR-0010) |
| One per payment, with no netting | Against ADR-0020: a PSP settles net, in batches |

How a batch reaches the server:

| Option | Why not |
|---|---|
| **The server lists the batches after those it holds and downloads each, on a timer** | Chosen |
| A notification carrying the file's URL, as at Adyen, with the timer as a fallback | A second path to the same files, and a lost notification must be covered by the timer anyway |
| A report run requested and then downloaded, as at Stripe | Two calls and a wait per batch, for files that the PSP builds anyway |

## Decision

- **A batch** is one settlement run in one currency: a number, in sequence from 1 per PSP
  and currency, a settlement time, and its lines. A line is a capture or a refund that the
  run settles, with the payment's ID, the PSP's fact ID of the movement, the ID of the
  request that it answered (the reference that the sender gives the PSP, ADR-0028), the
  gross, the PSP's fee and the net, which is the gross less the fee. A refund's gross and
  net are negative. One more line, the payout, holds the sum of the nets, which may be
  negative.
- **The mock's fee** on a capture of C is `(C * r + 5000) div 10000 + f`, computed in
  `i128`, with its rate `r` in basis points and its fixed part `f` set per currency: half
  up, as the platform's fee is (ADR-0020). A refund carries no fee, and a refunded capture
  keeps its fee, so a capture smaller than its fee nets below zero.
- **The mock's settlement run** settles the captures made by its cutoff and the refunds
  made since its last run, adds each capture's `settled` fact to its history, and then
  publishes one batch per currency that has any. A test can script the opposite order
  through the fake's handle (ADR-0071).
- **The wire.** The mock lists the batches of one currency after a number, `GET
  /settlement-batches?currency=EUR&after=7`, answering their numbers in ascending order in
  JSON, and serves each batch as CSV, `GET /settlement-batches/EUR/8`, behind the API key
  that its other routes take. The CSV's header is fixed:
  `batch,currency,settled_at,kind,payment_id,psp_fact_id,request_id,gross,fee,net`. Its
  `kind` is `capture`, `refund` or `payout`; the three references are empty on the
  payout's line only, where the fee is 0 and the gross equals the net; `batch`, `currency`
  and `settled_at` repeat on every line, the time in RFC 3339 and UTC.
- **A file that cannot be read** is refused whole, never booked in part: a header that
  differs in any column, a line that breaks the rules above, a capture whose gross is not
  positive or a refund whose gross is not negative, a net that is not the gross less the
  fee, amounts whose absolute values add up to more than the ledger holds in one entry,
  10^15 minor units (ADR-0003), since each entry of the batch's posting is a sum of them, a
  batch, currency or time that differs from the request or from another line, or no
  payout line or two. ADR-0070 says what happens to it.
- **The listing starts at the first gap.** For each currency that has terms agreed with
  the PSP (ADR-0068), the server asks for the batches after the highest number below which
  it holds every batch, and skips by key those that it holds above that number. A number
  that the listing skips while a later one is listed is a missing batch (ADR-0068). A gap
  stays the start while its batch is not held, even once an operator has accepted it as
  absent, so a batch that arrives later is still booked and its acceptance reversed
  (ADR-0069); the listing then carries every number after the gap, one more per run.

## Consequences

- The mock gains fees, batches, a listing and files, and its binary's settlement timer
  publishes batches. Its fee code and the server's check of that fee are written apart
  (ADR-0029), so a wrong formula in either shows as a discrepancy.
- Booking (ADR-0067) and matching (ADR-0068) read only the file and the PSP's history: the
  listing says which batches exist, and each file says what its batch settles.
- The payout's line is only a total. Whether its money arrived would take a bank
  statement, which is not built (ADR-0071).
- Not built: a notification of a new batch, reports run on request, and other formats.

## References

- Adyen, [Settlement details report](https://docs.adyen.com/reporting/settlement-reconciliation/transaction-level/settlement-details-report).
- Adyen, [Get reports automatically](https://docs.adyen.com/reporting/automatically-get-reports).
- Stripe, [Payout reconciliation report](https://docs.stripe.com/reports/payout-reconciliation).
- Stripe, [Run a report from the Reports v1 API](https://docs.stripe.com/reports/v1-api).
