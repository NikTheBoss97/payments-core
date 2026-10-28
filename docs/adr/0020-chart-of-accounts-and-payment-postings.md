<!-- Author: Nicola Antonello -->

# ADR-0020: Chart of accounts and payment postings

- Status: accepted
- Date: 2026-10-28
- Supersedes: in ADR-0009's Decision, the words "then becomes the only
  producer of keys", narrowed below to the keys the library builds

## Context

Phase 2 is the first phase that posts on business events. ADR-0009 left the
chart of accounts to it: an append-only `account_kinds` table, a foreign key
to it from `accounts`, and a typed `AccountRef` in Rust. ADR-0003 left it the
fee's rounding rule.

The platform collects payments for merchants through a PSP and keeps a fee.
In Stripe's destination charges, the closest documented model, the full
charge goes to the merchant's account at capture and the platform's fee
comes back to the platform. On a refund, by default, the platform keeps its
fee and pays for the refund itself; charging the merchant is an option
(`reverse_transfer`). Square instead returns a proportional share of the
platform's fee on each partial refund.

Two rules of the ledger shape every posting: an entry is never zero, and a
ledger transaction has at least two entries.

## Options

When to post:

| Option | Why not |
|---|---|
| **Captures and refunds only** | Chosen |
| Pending postings at authorization, released on void or expiry | Extra transactions, account kinds and an expiry job, for money that never becomes the platform's |
| Each settlement as a posting per movement, already in phase 2 | PSPs settle net, in batches, so gross postings per movement would not match the settlement file; it also starts phase 6 early |

Who pays for a refund:

| Option | Why not |
|---|---|
| **The merchant, while the platform keeps its fee** | Chosen |
| The merchant, with a proportional share of the fee returned, as at Square | Each refund's posting then depends on the earlier refunds. Rounding each share on its own returned more than the fee in 211 of 200,000 simulated pairs of refunds, so the shares must be computed cumulatively |
| The platform, Stripe's default for destination charges | Needs an expense account kind and another posting, and turns every refund into the platform's loss |

Rounding the fee:

| Option | Why not |
|---|---|
| **Half up, an exact half going to the platform** | Chosen |
| Half down, an exact half going to the merchant | Also the allocation function, with the shares listed the other way round. Stripe rounds its own fees half up |
| Half to even | Unbiased, but it needs a formula of its own beside the allocation function |
| Down (floor) | Every fraction goes to the merchant |

Keeping account keys typed:

| Option | Why not |
|---|---|
| **`AccountRef` builds every key the library uses; `AccountKey` stays public for ledger tests** | Chosen |
| `AccountKey` with private fields, so accounts open only through `AccountRef` | Rewrites five phase-1 test files, which open accounts of made-up kinds such as `kind_1` to `kind_8` |
| Drop the sentence and rely on the new foreign key | The key checks a kind and its side, not that a party suits the kind, which ADR-0009 leaves to Rust |

## Decision

- **Chart of accounts.** `account_kinds (kind, normal_side)` is append-only,
  guarded by `reject_change()` (ADR-0004), and seeded by its migration with
  exactly the kinds that phase 2 posts to:
  - `psp_receivable`, debit: what the PSP owes the platform;
  - `merchant_payable`, credit: what the platform owes a merchant;
  - `fee_revenue`, credit: the platform's fees.

  `accounts_kind_normal_side_fkey` makes every account's
  `(kind, normal_side)` a row of this table. A later phase seeds a kind in a
  migration of its own when it first posts to it. The runtime role can only
  read the table.
- **Typed account references.** `AccountRef` has three variants:
  `PspReceivable(Psp, Currency)`, `MerchantPayable(MerchantId, Currency)` and
  `FeeRevenue(Currency)`. Their parties are the PSP's label, the merchant's
  ID and `platform` (ADR-0009). Every key that the library builds comes from
  an `AccountRef`, while `AccountKey` stays public, so that ledger-level
  tests can open accounts of any kind they seed.
- **The PSP.** `Psp` is a closed set of labels, parsed once. Phase 2 has
  one, `mock`.
- **Postings.** Only captures and refunds post. With C the captured amount,
  F the fee and R a refund, the lines are below. A line whose amount would be
  zero is left out, so a fee of 0 or of the whole capture gives two lines.

| Event | Debit | Credit |
|---|---|---|
| Capture | `psp_receivable` of the PSP, +C | `merchant_payable` of the merchant, −(C − F); `fee_revenue` of the platform, −F |
| Refund | `merchant_payable` of the merchant, +R | `psp_receivable` of the PSP, −R |

- **The fee.** F = (C × r + 5000) div 10000, computed in `i128` on the
  positive captured amount, with r the payment's fee rate in basis points,
  0 to 10,000, fixed when the payment is created (ADR-0017). This is the
  fee's share when ADR-0003's allocation function splits C into the shares
  [r, 10000 − r]: rounded half up, an exact half going to the platform. So
  0 ≤ F ≤ C, and the capture's lines sum to zero.
- **Refunds.** The merchant pays for a refund and the platform keeps its
  fee, so a refund's posting depends on R alone. After a full refund, the
  merchant's payable stands at +F: the merchant owes the platform its fee,
  to be recovered once payouts exist.
- **Not posted in phase 2:** authorizations, declines, cancellations,
  voids, expiries and settlements, and the PSP's own fees, which phase 6
  books with kinds of its own.
- **Accounts are opened before any unit of work.** Creating a payment opens
  its three accounts (ADR-0019), so a capture or a refund only looks them
  up by key, and its posting refers only to committed accounts (ADR-0005).
- **Classification.** When an account is opened, `23503` on
  `accounts_kind_normal_side_fkey` is `Invariant`: library code builds keys
  only through `AccountRef`, so an unknown kind is a bug.

## Consequences

- After a capture of C with a fee F, `psp_receivable` has moved by +C,
  `merchant_payable` by −(C − F) and `fee_revenue` by −F. After a full
  refund, `psp_receivable` is back at 0, `merchant_payable` stands at +F and
  `fee_revenue` at −F. Tests check each account's change, not only the
  total (ADR-0003).
- A posting depends only on its command and the payment's fixed rate, so a
  retry compares it exactly (ADR-0019).
- The PSP's fees stay out of the books until phase 6.
- Every capture and refund share-locks the PSP's receivable, and every
  capture the platform's fee revenue, through the entries' foreign key:
  the first rows that all payments in a currency share, for phase 8 to
  benchmark (ADR-0002).
- Phase-1 tests that use kinds outside the chart, such as `cash` and
  `kind_1` to `kind_8`, seed those kinds as the owner first.
- ADR-0003's open rounding rule is settled.

## References

- David P. Ellerman, [The Mathematics of Double Entry Bookkeeping](https://doi.org/10.1080/0025570X.1985.11977191) (Mathematics Magazine, 1985).
- Friedrich Pukelsheim, [Proportional Representation: Apportionment Methods and Their Applications, 2nd edition](https://doi.org/10.1007/978-3-319-64707-4) (Springer, 2017), chapter 5 (quota methods).
- Mitchell Franklin, Patty Graybeal and Dixon Cooper, [Principles of Accounting, Volume 1: Financial Accounting](https://openstax.org/details/books/principles-financial-accounting) (OpenStax, 2019), chapter 3 (normal balances, the chart of accounts).
- Ilya Ganelin, [Ledger: Stripe's system for tracking and validating money movement](https://stripe.dev/blog/ledger-stripe-system-for-tracking-and-validating-money-movement) (Stripe, 2024).
- Stripe, [Create destination charges](https://docs.stripe.com/connect/destination-charges) (collecting fees, refunds, refunding application fees).
- Stripe, [Rounding rules for Stripe fees](https://support.stripe.com/questions/rounding-rules-for-stripe-fees).
- Square, [Refund a payment with an application fee](https://developer.squareup.com/docs/payments-api/collect-fees/payment-with-app-fee-refund).
