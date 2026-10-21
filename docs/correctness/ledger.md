<!-- Author: Nicola Antonello -->

# Correctness: the ledger

The [README](../../README.md#correctness) lists every invariant of the project; this page shows how the ledger's are tested.

Phase 1 guarantees the ledger's three invariants. PostgreSQL enforces I1 and
I2 itself, so they hold even for writes that bypass the Rust code: a buggy
code path, or SQL typed by hand as the service's own database role. Rust types
add a first layer where they can, such as a ledger transaction that cannot be
built unbalanced. I3 holds by construction, because no balance is stored. The
tests attack each layer on its own.
[`docs/DESIGN.md`](../DESIGN.md#invariants) states how each invariant is
enforced, and the [ADRs](../adr/) say why.

| | Invariant | Tested by |
|---|---|---|
| I1 | Every ledger transaction's entries sum to zero, so each currency sums to zero across the ledger. | Property tests of amounts and of the ledger transaction type ([`money.rs`](../../crates/payments-core/src/money.rs), [`ledger.rs`](../../crates/payments-core/src/ledger.rs)). Raw SQL, as the runtime role, that writes unbalanced, incomplete and mixed-currency transactions, or adds entries after a commit ([`ledger_schema.rs`](../../crates/payments-core/tests/ledger_schema.rs)). Triggers that corrupt a posting after the types accepted it, so that only the database's commit-time check can reject it ([`posting.rs`](../../crates/payments-core/tests/posting.rs)). A trial balance after 200 concurrent postings ([`concurrency.rs`](../../crates/payments-core/tests/concurrency.rs)). |
| I2 | The ledger's tables are append-only: no row is ever updated or deleted, and a correction is a new, reversing transaction. | `UPDATE`, `DELETE` and `TRUNCATE` as the runtime role, which lacks the privileges, and as the tables' owner, which only the triggers stop ([`ledger_schema.rs`](../../crates/payments-core/tests/ledger_schema.rs), [`accounts.rs`](../../crates/payments-core/tests/accounts.rs), [`currencies.rs`](../../crates/payments-core/tests/currencies.rs)). |
| I3 | Balances are derived from the entries: none is stored. | After 200 concurrent postings, each balance must equal an in-memory model that adds the lines up in Rust ([`concurrency.rs`](../../crates/payments-core/tests/concurrency.rs)). Reads of a new account, an unknown one and one whose sum overflows ([`balances.rs`](../../crates/payments-core/tests/balances.rs)). A catalog test that pins every column fails on a new balance column ([`catalog.rs`](../../crates/payments-core/tests/catalog.rs)). |

The tests also attack the guards themselves:

- **A decoy table.** A temporary table named `ledger_entries`, which an unqualified name finds before the real one, holds balanced rows while the real entries are unbalanced. The commit-time check still reads the real ones, because its function pins `search_path`.
- **A planted operator.** `<>` operators created in `public` that never report a difference cannot decide the balance check, which compares only types for which `pg_catalog` has an exact operator.
- **Replica mode.** `session_replication_role = replica` turns off ordinary triggers and foreign-key checks; the ledger's triggers are `ENABLE ALWAYS`, so they still fire.
- **What behaviour cannot show.** Catalog tests ([`catalog.rs`](../../crates/payments-core/tests/catalog.rs)) pin the runtime role's exact privileges and session timeouts, each guard trigger with its function's `search_path`, and every column's type and `NOT NULL`.

## Failures the posting path is tested against

`post()` is idempotent by transaction ID. A failed `COMMIT` counts as rolled
back only when it reports `LG002` or `LG003`, the codes of the ledger's own
commit-time check; any other failure is reported as `NotConfirmed`, and
posting the same value again settles it
([ADR-0014](../adr/0014-what-a-failed-commit-proves.md),
[ADR-0015](../adr/0015-posting-errors-and-idempotent-posting.md)). The rule
lives in one function, `db::commit`, which
[`committing.rs`](../../crates/payments-core/tests/committing.rs) tests against real
`COMMIT`s and a property test in [`db.rs`](../../crates/payments-core/src/db.rs)
tests against random SQLSTATEs.

| Failure | Outcome the test expects | Test |
|---|---|---|
| Eight copies of one posting race each other | One `Applied` and seven `AlreadyApplied`; the balance counts the posting once | [`concurrency.rs`](../../crates/payments-core/tests/concurrency.rs) |
| The same ID arrives with other entries | `Mismatch`; the stored posting is unchanged | [`posting.rs`](../../crates/payments-core/tests/posting.rs) |
| The session ends during `COMMIT`, which rolls back | `NotConfirmed`; posting again returns `Applied` | [`posting.rs`](../../crates/payments-core/tests/posting.rs) |
| A `COMMIT` commits, then returns an error | `NotConfirmed`; posting again returns `AlreadyApplied` | [`commit_race.rs`](../../crates/payments-core/tests/commit_race.rs) |
| A retry waits behind a first attempt that has not committed | `NotConfirmed` when the retry's lock wait times out; `AlreadyApplied` once the first attempt commits | [`posting.rs`](../../crates/payments-core/tests/posting.rs) |
| The first attempt is left idle inside its transaction | The server ends it before the retry gives up (a 30 s idle timeout against a 35 s lock timeout in production), and the retry applies | [`posting.rs`](../../crates/payments-core/tests/posting.rs) |
