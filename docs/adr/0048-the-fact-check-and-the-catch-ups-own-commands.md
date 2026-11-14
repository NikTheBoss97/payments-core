<!-- Author: Nicola Antonello -->

# ADR-0048: The fact check and the catch-up's own commands

- Status: accepted
- Date: 2026-11-14

## Context

ADR-0034 guards I8 four ways, and since the merchant role's guards (ADR-0047's third pull
request) two are left. `LG007` is a check at `COMMIT` that refuses a step of the PSP's
without its fact. The Rust guard is a type that only the catch-up can build, without
which no fact is applied. ADR-0047 left two questions to the pull request that lands them:
how `LG007` meets the other checks at `COMMIT`, and what becomes of the code that only a
retry of a step of the PSP's reached.

A first draft of `LG007`, run against the whole suite, failed 39 of 781 tests. Three are
the catalog's pins. The other 36, in eight files, commit a step of the PSP's that no fact
is tied to. Most write it through the helpers that build a payment at each status by
hand, and the rest write it themselves. Some of the 36 attack another check, which the
draft reached first. PostgreSQL 18.6 ran the deferred checks in the order that their rows
were written, and it fires the triggers of one row "in alphabetical order by name". So a
fixture's earlier step without its fact was reported before the step without its record
that the test was about.

Since ADR-0046 no route runs a step command of the PSP's, and since ADR-0047's second pull
request no test does. The catch-up applies each fact through the half of the command that
runs in its transaction: `authorize_in`, `capture_in`, `refund_in` or `plain_in`. Any
module of the crate can call those.

## Options

How `LG007` meets the other checks:

| Option | Why not |
|---|---|
| **A trigger of its own, which sorts after the step's other check, and fixtures that write each step's fact** | Chosen |
| Tests that expect `LG007` wherever their fixture lacks a fact | They would stop reaching the rule that they attack, against ADR-0013's one rule at a time |
| The fact checked in the step's existing trigger | One trigger would raise two codes, and ADR-0034 names a trigger of its own |

The Rust guard:

| Option | Why not |
|---|---|
| **A token with a private field, which only the catch-up's module makes and the four halves take** | Chosen |
| The commands' types moved into the catch-up's module | More code moves for the same effect |
| The halves left as they are, `pub(crate)` | Any module of the crate could apply a fact, which ADR-0034's first guard rules out |

The code that only a retry of a step of the PSP's reached:

| Option | Why not |
|---|---|
| **Removed** | Chosen |
| Kept, for a command that may post one day | A path that no caller and no test reaches |

## Decision

- **`LG007`.** A constraint trigger, `payment_transitions_fact_check`, `DEFERRABLE
  INITIALLY DEFERRED` and `ENABLE ALWAYS`, runs `check_payment_step_fact()`, whose
  `search_path` is pinned (ADR-0004). At `COMMIT` it refuses a step that no row of
  `payment_transition_facts` ties to a fact, unless the step is a payment's first or a
  cancel, and it raises as ADR-0013 prescribes. It names the two kinds that need no fact
  rather than the seven that do, as I8 does, so a kind added later needs a fact until a
  decision says otherwise. Its name sorts after `payment_transitions_commit_check`, so a
  step that lacks both its record and its fact is reported as `LG004`.
- **What `COMMIT` proves.** `LG007` joins the codes that prove a rollback (ADR-0014).
- **The fixtures.** The helpers that write a step of the PSP's by hand write its fact and
  the link with it, so that each test still breaks one rule (ADR-0013). A fixture's
  history starts with the payment's first step and puts no cancel before a step of the
  PSP's, so a step's sequence number is its fact's position. Tests that write such a step
  themselves do the same where they commit it.
- **The Rust guard.** The catch-up's module defines `PspWord`, whose one field is private,
  so only that module makes one. `authorize_in`, `capture_in`, `refund_in` and `plain_in`
  take one. A `compile_fail` doctest shows that code outside the crate cannot make one, as
  `Transition`'s does (ADR-0016), and Rust's privacy keeps the crate's other modules out
  in the same way.
- **What goes.** The seven public step commands of the PSP's, `authorize`, `capture`,
  `refund`, `decline`, `void`, `expire` and `settle`, go with their three error types.
  `cancel` stays. A retry no longer compares a stored posting, since only `cancel` and
  `create` are retried and neither posts. The final refusals that only the PSP's commands
  stored under an idempotency key go too. The commands' types stay, private to the crate,
  as the catch-up's input.

## Consequences

- A step of the PSP's commits only with its fact, whoever writes it: the catch-up through
  the Rust guard, raw SQL through `LG007`.
- What `LG007` still does not stop is ADR-0034's: code running as `payments_app` that
  writes a step and a made-up fact together, which the next catch-up finds and stops.
- I8's status in the README and in DESIGN.md, and the PSP's correctness page, come with the
  pull request that closes phase 4.

## References

- PostgreSQL, [CREATE TRIGGER](https://www.postgresql.org/docs/18/sql-createtrigger.html)
  (PostgreSQL 18 docs), section Description: the order in which several triggers on one
  event fire.
