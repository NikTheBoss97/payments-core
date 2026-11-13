<!-- Author: Nicola Antonello -->

# Correctness: payments

The [README](../../README.md#correctness) lists every invariant of the project; this page shows how the payment lifecycle's are tested.

Phase 2 guarantees that a payment never makes an invalid move, and that a state
change and its ledger entries commit together. As with the ledger, each is
enforced twice: Rust's table of moves and types decide first, and PostgreSQL
refuses a wrong row itself, so the rules hold even for a buggy code path or SQL
typed by hand as the service's own database role. Under failure, a command's one
rule for its caller is to send the identical command again.
[`docs/DESIGN.md`](../DESIGN.md#invariants) states how each invariant is enforced,
and [ADR-0016](../adr/0016-payment-lifecycle.md) to
[ADR-0022](../adr/0022-business-times.md) say why.

| | Invariant | Tested by |
|---|---|---|
| I5 | A payment never makes an invalid state transition. | All 63 pairs of status and event against a table written out by hand, and random command sequences against a model with no table of moves ([`lifecycle.rs`](../../crates/payments-core/src/lifecycle.rs)). All 441 triples of status, kind and status against the database, as the runtime role, compared with Rust's table in both directions ([`payment_history.rs`](../../crates/payments-core/tests/payment_history.rs)). Each move from every status where it can come: a merchant's request, which the table refuses before its limit and before anything is recorded ([`authorizing_payments.rs`](../../crates/payments-core/tests/authorizing_payments.rs), [`capturing_payments.rs`](../../crates/payments-core/tests/capturing_payments.rs), [`refunding_payments.rs`](../../crates/payments-core/tests/refunding_payments.rs)), the PSP's lapse and settlement as facts that a catch-up applies or stops at, and the merchant's cancel ([`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs)). Random histories of three merchants' requests and the PSP's decisions, at the same time, against a model that predicts each answer, each status and the money, and that holds the requests in flight ([`payment_model.rs`](../../crates/payments-core/tests/payment_model.rs)). A `compile_fail` doctest that code outside the module cannot forge a move. |
| I6 | A payment's state change and its ledger entries commit atomically. The outbound event joins them in phase 5. | The backend killed before and after each write and during `COMMIT`, for a create and for the catch-up of each move of the PSP's: nothing written in its transaction stays, and the identical command, or the next catch-up, then applies it once ([`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs)). Raw SQL that writes a step without its record, a payment without its first step, or a record whose posting is wrong ([`payment_records.rs`](../../crates/payments-core/tests/payment_records.rs), [`payment_history.rs`](../../crates/payments-core/tests/payment_history.rs), [`payment_postings.rs`](../../crates/payments-core/tests/payment_postings.rs)). Triggers that lose or alter a row the types allowed, so that only a check at `COMMIT` can refuse it ([`capturing_payments.rs`](../../crates/payments-core/tests/capturing_payments.rs), [`refunding_payments.rs`](../../crates/payments-core/tests/refunding_payments.rs)). |

The payment flows also keep the ledger's invariants. After each flow every currency
sums to zero (I1), each account's balance, derived from its entries, matches the
exact lines of [ADR-0020](../adr/0020-chart-of-accounts-and-payment-postings.md) or
the model's plain arithmetic (I3), and a query finds no posting without a cause
([ADR-0021](../adr/0021-what-causes-a-posting.md)).

The tests also attack the guards themselves:

- **One posting for two causes.** A posting linked as one payment's capture and another's refund cannot match both shapes, because a capture debits the PSP's receivable and a refund credits it ([`payment_postings.rs`](../../crates/payments-core/tests/payment_postings.rs)).
- **An older posting with the right lines.** A refund linked to a posting that nothing caused is refused by the test of who wrote it, its header's `xmin`, since every amount matches ([`payment_postings.rs`](../../crates/payments-core/tests/payment_postings.rs)).
- **A refund that lies about its predecessor.** A step that claims more was refundable than its predecessor left fails on the chain of foreign keys ([`payment_history.rs`](../../crates/payments-core/tests/payment_history.rs)).
- **Two records for one step.** The check at `COMMIT` asks only that a record exists, so only the tables' keys refuse a second one, even with a posting of its own ([`payment_records.rs`](../../crates/payments-core/tests/payment_records.rs)).
- **A decoy table, a planted operator, and replica mode,** as for the ledger, against the checks of a record's posting ([`payment_postings.rs`](../../crates/payments-core/tests/payment_postings.rs)).
- **What behaviour cannot show.** Catalog tests ([`catalog.rs`](../../crates/payments-core/tests/catalog.rs)) pin the runtime role's privileges, each trigger with its `ENABLE ALWAYS` and `search_path`, every column, and every constraint's name. A real login, not `SET ROLE`, pins READ COMMITTED and the session timeouts. A test fails if `src/` names any advisory-lock function but the transaction-level one ([`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs)).

## Failures the commands are tested against

A command answers by identity before state, under its payment's lock
([ADR-0018](../adr/0018-serializing-a-payments-commands.md),
[ADR-0019](../adr/0019-payment-commands-retries-and-errors.md)). One pure classifier
reads each database error, by its SQLSTATE: a duplicate key looks the step up again,
a broken rule is `Invariant`, and anything else, a lost connection above all, is
`NotConfirmed`.

| Failure | Outcome the test expects | Test |
|---|---|---|
| Eight copies of one command race each other, or eight catch-ups of one capture | One `Applied` and seven `AlreadyApplied`, with the same reply, or one catch-up that applies the fact; money moves once | [`creating_payments.rs`](../../crates/payments-core/tests/creating_payments.rs), [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs), [`capturing_payments.rs`](../../crates/payments-core/tests/capturing_payments.rs) |
| A retry arrives after the payment has moved on | The original reply, though the status now refuses the event, and an older step's own reply, not the newest's | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| A retry names another kind of step, or another payment | `Mismatch`; nothing changes | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| A command is refused | Decided again: rejections are not stored, so its ID stays free for a command that applies | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| Two commands race, or a capture's request and a void's | One winner, and the loser gets a domain answer with no database error; money moves only if the capture won | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs), [`capturing_payments.rs`](../../crates/payments-core/tests/capturing_payments.rs) |
| Twelve refunds of 1 are asked for against 5 left | Exactly 5 are recorded and refunded, and the rest are told so by the limit, which counts the refunds in flight | [`refunding_payments.rs`](../../crates/payments-core/tests/refunding_payments.rs) |
| The PSP refunds and settles, in either order | Partial: both apply. Final: refunded either way, with the same money | [`refunding_payments.rs`](../../crates/payments-core/tests/refunding_payments.rs) |
| A batch locks two payments in key order while commands run | It waits behind a command, then a command waits behind it | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| A writer skips the lock and commits first | The locked command is reported as a bug; nothing is written wrongly | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| A retry waits behind a first attempt that has not committed | `NotConfirmed` at the lock timeout; `AlreadyApplied` once the first attempt commits, or `Applied` if it rolled back | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| The first attempt is left idle inside its transaction | The server ends it before the retry gives up, and the retry applies | [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs) |
| The backend dies before or after any write, or during `COMMIT` | `NotConfirmed`; nothing written in its transaction stays; the same command returns `Applied`, then `AlreadyApplied`, and the next catch-up applies the fact once, then finds it held. Accounts that `create_payment` had already opened stay, committed on their own, and the retry finds them | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| The backend dies while waiting for the lock | `NotConfirmed`; the next catch-up applies once the lock is free | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A `COMMIT` commits, then the session dies | `NotConfirmed`; the next catch-up finds the fact held, and money moved once | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A command needs a second connection while it holds one | It cannot wait for itself: retries, lookups and catch-ups run on a pool of one connection | [`creating_payments.rs`](../../crates/payments-core/tests/creating_payments.rs), [`payment_steps.rs`](../../crates/payments-core/tests/payment_steps.rs), [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A time has nanoseconds or a leap second, or lies outside what can be stored | It comes back equal, so a retry settles; one that cannot be stored cannot be built, and the columns refuse the rest | [`business_times.rs`](../../crates/payments-core/tests/business_times.rs), [`payment_records.rs`](../../crates/payments-core/tests/payment_records.rs) |

The crash tests end one session, as a lost connection would. A dead Rust process
ends its session too, and [PostgreSQL rolls back any open transaction](https://www.postgresql.org/docs/18/protocol-flow.html#PROTOCOL-FLOW-TERMINATION)
however a session ends. No test here kills the process itself, and the
[API's tests](api.md) do, with commands in flight. PostgreSQL's own recovery is outside the
[failure model](../DESIGN.md#failure-model), which assumes that committed data is
not lost.

## What these tests have found

- **A retry reported as a bug.** A retry racing its first attempt sends the same candidate account ID, and `ON CONFLICT` covers only the key it names, so the second could fail on the primary key. Eight concurrent copies hit this in about one run in ten, after a first green run in CI ([ADR-0009](../adr/0009-account-identity.md)). Concurrency tests now run in a loop before a pull request opens.
- **A leap second that broke retries.** chrono parses `23:59:60.5`, and PostgreSQL returned the next day's `00:00:00.5`, so every retry of that command would have been a `Mismatch` ([ADR-0022](../adr/0022-business-times.md)).
- **A panic on hand-written data.** A `timestamptz` also holds `infinity`, which sqlx panics on when it decodes one, so the columns now refuse what Rust cannot read ([ADR-0022](../adr/0022-business-times.md)).
- **A wrong belief about locks.** A transaction's locks go when a statement aborts it, not at `ROLLBACK` ([ADR-0018](../adr/0018-serializing-a-payments-commands.md)).
- **Tests that tested too little.** `query!` checks SQL against the schema at compile time, so most of these tests passed on their first run, and each command's rules were then broken on purpose to show that they can fail. That found a retry that no test sent, and a posting link that a test checked only by coincidence. The first model test never reached `refunded`, so it now fails unless its histories end in every status. Moved onto requests and the PSP's decisions, it settled no payment refunded in full, which no status shows, so it now fails unless a history does. A last read of the decision records against the tests found that nothing pinned the constraints' names, and that nothing attacked the keys that allow one record per step.
