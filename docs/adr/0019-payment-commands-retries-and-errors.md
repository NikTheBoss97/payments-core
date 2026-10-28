<!-- Author: Nicola Antonello -->

# ADR-0019: Payment commands, retries and errors

- Status: accepted
- Date: 2026-10-28

## Context

A command can end with an unknown outcome: the connection drops, a lock
wait times out, or a `COMMIT` is answered with an error that proves nothing
(ADR-0014). The ledger settles such an outcome by sending the identical
posting again, with the same IDs, and comparing what is stored (ADR-0005,
ADR-0015). Payment commands need the same property: a capture retried after
a lost `COMMIT` must neither capture twice nor be refused as illegal
because the payment is already `captured`.

Phase 3 adds idempotency keys for clients. Until then, the caller of the
library builds each command and is the one that retries it.

## Options

Identity of a command:

| Option | Why not |
|---|---|
| **A caller-made `TransitionId` on every command, checked before the state** | Chosen |
| The same, and rejected commands remembered by their ID, as TigerBeetle does with `id_already_failed` | A table and a write on every rejection, for final answers that phase 3 stores per idempotency key anyway |
| Natural keys, such as (payment, kind) | A retry cannot be told from a second command of the same kind, and repeated partial refunds become impossible |

Shape of the errors:

| Option | Why not |
|---|---|
| **One error enum per operation shape, holding only the variants it can return** | Chosen |
| One generic `TransitionError<R>` for every command | Decline, cancel, void, expire and settle would carry an accounts variant that they can never return |
| A duplicate key read as "conflict, retry", with "contended" after a few rounds | Needed only without the lock of ADR-0018, and the answer would depend on which unique index PostgreSQL happens to check first |

## Decision

- **Commands are plain data and carry every ID.** Each command carries a
  `TransitionId` that the caller creates with `TransitionId::now_v7()`, and
  a capture or refund also carries the `TransactionId` of its posting.
  Creating a payment carries the payment's `PaymentId` and its genesis
  step's `TransitionId`. A retry is the identical value sent again.
- **One function per command.** Each runs as one unit of work at READ
  COMMITTED, in this order:
  1. for a capture or refund, outside any database transaction: read the
     payment's immutable terms, look its accounts up by key, and build the
     posting with the fee rule of ADR-0020;
  2. `BEGIN`, then the payment's advisory lock (ADR-0018);
  3. one read that fetches the step with the command's `TransitionId`, on
     any payment, together with its record, and the payment's newest step;
  4. identity first: if that step exists, the stored command is compared
     with this one, and the answer is `AlreadyApplied` with the stored
     reply when they are equal, `Mismatch` when they differ;
  5. otherwise the transition table and the amount rules decide
     (ADR-0016), and a rejection drops the transaction and is returned;
  6. insert the step, then the posting through `post_in` (ADR-0011), then
     the record, after the ledger rows it references;
  7. `COMMIT` through the one function of ADR-0014.
- **Creating a payment** first checks that its merchant is registered, then
  opens the payment's three accounts through the pool, in sorted key order
  (ADR-0009, ADR-0020), and only then inserts the payment and its genesis
  step in one transaction. It needs no lock: a concurrent insert of the same
  payment waits on the primary key. Checking the merchant first means an
  unknown merchant never gets a payable account, and the check cannot go
  stale, because merchants are append-only.
- **Why identity comes first.** A retry that waited behind its own first
  attempt finds that attempt's step. Checked against the state first, it
  would be refused as illegal although it was applied.
- **What a retry compares.** The step's payment, kind and amount; the
  record's business time, fee and ledger transaction ID, which is the link
  of ADR-0011; and the posting's entries, through the comparison that
  `post()` already uses. It does not compare `seq`, the statuses or the
  refundable amount, which describe where the command landed rather than
  what it asked for, nor `recorded_at`.
- **The reply.** An applied command returns `Recorded`: the step's `seq`,
  its from and to statuses, and the refundable amount after it. A retry gets
  the same values, read from the stored step, which is what phase 3 will
  replay to clients.
- **Rejections are not remembered.** An illegal transition or an amount
  above its limit writes nothing, and a retry is decided again: a capture
  refused while the payment was `created` can succeed later with the same
  ID.
- **Errors.** Each operation shape has its own enum, holding only what it
  can return (ADR-0015): `CreateError`, `AuthorizeError`, `StepError` (for
  decline, cancel, void, expire and settle), `CaptureError`, `RefundError`
  and `ReadError`. They derive `thiserror::Error`, keep a `sqlx::Error` as
  `#[source]`, and never convert from one with `#[from]`.
- **One pure classifier** maps (stage, SQLSTATE, constraint name) to an
  outcome:
  - a duplicate key (`23505`), whichever unique index PostgreSQL reports,
    means that some ID already exists. The command drops its transaction
    and looks its `TransitionId` up again, under the lock: a stored step
    equal to the command is `AlreadyApplied`, a different one is
    `Mismatch`, and none means that another writer broke a rule, which is
    `Invariant`;
  - any `LG` code, any other class-`23` error, and "out of shared memory"
    (`53200`, ADR-0018) are `Invariant`: a bug, never retried;
  - everything else, such as a lock timeout (`55P03`), a deadlock
    (`40P01`), a cancelled statement (`57014`), a lost connection (class
    `08`) or no SQLSTATE at all, is `NotConfirmed`, and the caller sends the
    identical command again later;
  - at `COMMIT`, only `LG002` to `LG006` prove a rollback (ADR-0014), and
    they are bugs, so `Invariant`; any other `COMMIT` error is
    `NotConfirmed`.
- **Units of work for later phases.** `pub(crate)` variants, named `*_in`,
  run steps 3 to 6 inside a database transaction that their caller has
  begun and locked. Phase 5 then adds an outbox row, and phase 6 settles a
  batch, in the same transaction as the steps (invariant I6).
- **No HTTP in phase 2.** The commands are a library API exercised by tests.
  Endpoints arrive in phase 3 together with idempotency keys, so no client
  meets the gap below before keys exist.

## Consequences

- A retry after any unknown outcome is safe and gets the original answer,
  provided the caller sends the identical command. A business time is part
  of the command, so a retry rebuilt with a fresh time is `Mismatch`.
- Until phase 3, a caller that builds a new command, with new IDs, after an
  unknown outcome can apply a partial refund twice; the refundable amount
  bounds the damage. This is the gap that ADR-0005 leaves to idempotency
  keys.
- A writer that skips the lock and commits first makes a correct, locked
  command fail as `Invariant` (ADR-0018).
- The tests that travel with the commands:
  - retries: `AlreadyApplied` with the same reply; `Mismatch` for each
    compared field; a rejection decided again; a lock timeout followed by
    `AlreadyApplied`; a first attempt left idle and ended by the server;
    eight concurrent copies of one command, applied once;
  - races: a capture against a void; N refunds of 1 against a refundable
    amount of K, of which exactly K apply; a refund against a settle; a
    writer that skips the lock; a batch that queues and commits;
  - crashes: the backend killed around each statement; `LG004` to `LG006`
    proven to roll back; a trigger that corrupts a write that the types
    allowed, so that only a deferred check can reject it.

## References

- J. H. Saltzer, D. P. Reed and D. D. Clark, [End-to-End Arguments in System Design](https://doi.org/10.1145/357401.357402) (ACM Transactions on Computer Systems, 1984).
- Philip A. Bernstein, Meichun Hsu and Bruce Mann, [Implementing Recoverable Requests Using Queues](https://doi.org/10.1145/93597.98721) (SIGMOD, 1990).
- Butler Lampson, [Hints and Principles for Computer System Design](https://arxiv.org/abs/2011.02455) (arXiv, 2021), section 3.6.2.
- Sebastian Burckhardt et al., [Durable Functions: Semantics for Stateful Serverless](https://doi.org/10.1145/3485510) (OOPSLA, 2021).
- Pat Helland, [Idempotence Is Not a Medical Condition](https://doi.org/10.1145/2181796.2187821) (ACM Queue, 2012).
- Brandur Leach, [Implementing Stripe-like Idempotency Keys in Postgres](https://brandur.org/idempotency-keys) (2017).
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests).
- TigerBeetle, [create_transfers](https://docs.tigerbeetle.com/reference/requests/create_transfers/#id_already_failed) (`id_already_failed`).
