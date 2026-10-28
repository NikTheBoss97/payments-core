<!-- Author: Nicola Antonello -->

# ADR-0013: SQLSTATE codes for database guards

- Status: accepted; partly superseded by ADR-0014
- Date: 2026-10-17

## Context

The ledger's triggers (ADR-0004) reject bad writes with errors that have two
readers: a person reads the message, a program reads the code. A SQLSTATE
has five characters. The first two are its class, such as `23` (integrity
constraint violation), `40` (transaction rollback) or `42` (syntax error or
access rule violation), so a program that does not know a code can still
act on its class. The SQL standard keeps the classes that begin with `0` to
`4` or `A` to `H` for itself and leaves the rest to implementations, and
PostgreSQL 18 defines no class beginning with `L`.

`LG001` already marks an append-only violation (ADR-0004). PostgreSQL's own
constraints use one code per kind of constraint, such as `23514` for every
`CHECK`, and name the constraint in a separate error field.

## Options

| Option | Why not |
|---|---|
| One code for every rule of the commit-time check | A test can tell the rules apart only by matching message text. A setup slip also lets a test pass for the wrong reason: two unbalanced entries under an `entry_count` of 3 report "incomplete", not "unbalanced" |
| **One code per rule, in class `LG`** | Chosen |
| `23514` (`check_violation`), with the rule named in the constraint field | A ledger bug would look like an ordinary rejected input, and the names would refer to no row of `pg_constraint` |
| `40002`, which SQL-92 gives to a constraint that fails at `COMMIT` | Retry logic looks at class `40`, and these errors must never be retried |

## Decision

- Each database rule has its own code in class `LG`. Codes are assigned by
  rule, never reused, and never end in `000`: such codes are category codes,
  which a handler can only trap by trapping the whole category.
- The registry:
  - `LG001`: a change to an append-only table (ADR-0004);
  - `LG002`: an incomplete ledger transaction, with fewer entries than its
    `entry_count`, an empty header included;
  - `LG003`: an unbalanced ledger transaction.
- The commit-time check tests completeness first, then balance, and raises
  the first failure. The order is documented behaviour.
- Triggers raise with
  `RAISE SQLSTATE 'LG002' USING MESSAGE = ..., DETAIL = ..., CONSTRAINT = TG_NAME, TABLE = TG_TABLE_NAME, SCHEMA = TG_TABLE_SCHEMA`.
  In this form a malformed code fails the migration, at `CREATE FUNCTION`;
  written as `USING ERRCODE = '...'`, it would surface only when the guard
  fires. Setting the three name fields also fills sqlx's `constraint()` and
  `table()`.
- A message names the rule and the ledger transaction's ID. DETAIL gives the
  numbers: the declared and found entries, or the sum and its currency.
- The commit-time trigger is named `ledger_transactions_commit_check`.
  `ledger_transactions_check` is the name PostgreSQL would generate for an
  unnamed `CHECK` on the same table.
- The application classifies database errors as follows:
  - any `LG` code is a bug: never retried, answered with a 5xx, and logged
    with its SQLSTATE, constraint and DETAIL as fields, because sqlx's
    `Display` leaves the code out;
  - class-`23` errors are keyed by the pair (code, constraint name);
  - a SQLSTATE returned for `COMMIT` means the transaction rolled back, and no
    SQLSTATE means the outcome is unknown (ADR-0005).
- Tests assert literal codes rather than constants imported from the code
  under test.

## Consequences

- Each test asserts one constant, and messages can be reworded freely.
- Logs, metrics and alerts get a stable key for each rule, while the
  application can still treat the whole class as one domain error.
- A future PostgreSQL release could define class `LG`. PostgreSQL 18 does
  not, nor does the development branch of the next release.
- ADR-0004's "dedicated error code" now means one code per rule.

## Amendments

- 2026-10-20: ADR-0014 supersedes the last classification bullet. An error
  returned for `COMMIT` proves a rollback only when it carries one of the
  ledger's deferred-check codes (`LG002`, `LG003`): a cancel or terminate
  signal held during the commit can arrive as `57014` or `57P01` after the
  transaction committed. Any other `COMMIT` error leaves the outcome
  unknown.
- 2026-10-28: the registry gains three codes for the payment tables
  (ADR-0017), each raised by a deferred check at `COMMIT`:
  - `LG004`: a payment step of kind `authorize`, `capture` or `refund`
    without its record;
  - `LG005`: a payment without its genesis step;
  - `LG006`: a capture or refund record whose posting is not exactly the
    record's lines, or was not written by the same database transaction.

  The checks are separate triggers, so when several rules fail at once, the
  first trigger to run is the one reported; tests break one rule at a time.

## References

- PostgreSQL 18 manual, [PostgreSQL Error Codes](https://www.postgresql.org/docs/18/errcodes-appendix.html).
- PostgreSQL 18 manual, [Errors and Messages](https://www.postgresql.org/docs/18/plpgsql-errors-and-messages.html) (PL/pgSQL `RAISE`).
- PostgreSQL 18 manual, [Error and Notice Message Fields](https://www.postgresql.org/docs/18/protocol-error-fields.html).
- Google, [AIP-193: Errors](https://google.aip.dev/193) (programs must not parse error messages).
- TigerBeetle, [create_transfers](https://docs.tigerbeetle.com/reference/requests/create_transfers/) (one result code per rule, in precedence order).
