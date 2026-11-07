<!-- Author: Nicola Antonello -->

# ADR-0034: Only the PSP's word moves a payment

- Status: accepted
- Date: 2026-11-07
- Supersedes: in ADR-0025's Decision, "`payments_tenant` has the privileges of ADR-0012
  and ADR-0017": the merchant's role loses `INSERT` on the ledger's transactions and
  entries and on the three record tables, and may insert only `create` and `cancel`
  steps.

## Context

In phase 3 the merchant's role could write any step, any record and any posting,
because the merchant stood in for the PSP (ADR-0023). Phase 4 takes that role away:
facts come from the PSP's history (ADR-0031), and requests reach the PSP under a stable
ID (ADR-0028). Two guarantees follow that I1 to I7 do not state.

I5 says that a payment never makes an invalid transition, whoever writes the step. It
says nothing of who may write one, or in what order: a settlement and a full refund
applied in the wrong order are legal under I5 and still wrong (ADR-0031). I4 covers the
idempotency keys that the service receives; nothing yet covers the requests that it
sends.

## Options

| Option | Why not |
|---|---|
| **I8 and I9, with I8 guarded in Rust, by privileges, by the chain of facts, and by a deferred check, `LG007`** | Chosen |
| The same without `LG007` | Privileges guard only the merchant's role. `payments_app`, which runs the catch-up, the sweep and the operator's routes, could still write a step of the PSP's without its fact |
| No new invariant: widen the text of I4 and I5 | Two new failures would hide behind old rows of the README's table |

## Decision

- **I8:** only the PSP's word moves a payment. Apart from its creation and a
  cancellation, every step comes from a fact of the PSP's history, applied once and in
  the PSP's order, so what the service holds of that history is always a prefix of it.
- **I9:** a request sent to the PSP takes effect there at most once, even after the PSP
  has forgotten its idempotency key.
- **I8's guards:**
  1. **Rust.** Only the catch-up applies facts (ADR-0031), through a type with private
     fields that only it can build, as `Transition` is for steps (ADR-0016).
  2. **Least privilege.** Saltzer and Schroeder: "Every program and every user of the
     system should operate using the least set of privileges necessary to complete the
     job." The merchant's role keeps `INSERT` on `payments`, `accounts`,
     `payment_transitions`, `idempotency_records` and the table of requests
     (ADR-0028), and loses it on `ledger_transactions`, `ledger_entries`,
     `payment_authorizations`, `payment_captures` and `payment_refunds`. A restrictive
     policy, `payment_transitions_tenant_steps`, lets it insert only steps of kind
     `create` and `cancel`. PostgreSQL combines restrictive policies with `AND`, so "all
     restrictive policies must be passed for each record", on top of the permissive
     `payment_transitions_tenant_isolation`. The policy's name departs from ADR-0025's
     pattern on purpose, because it guards kinds, not tenants. A merchant's credential
     can then write no ledger line at all.
  3. **The chain of facts** (ADR-0031): a primary key and a predecessor foreign key on
     positions, and a check at every catch-up that what we hold is the PSP's first
     facts.
  4. **`LG007`.** A constraint trigger, `payment_transitions_fact_check`, `DEFERRABLE
     INITIALLY DEFERRED` and `ENABLE ALWAYS`, with its `search_path` pinned (ADR-0004),
     refuses at `COMMIT` a step of kind `authorize`, `decline`, `capture`, `void`,
     `expire`, `settle` or `refund` that no fact is tied to. It raises as ADR-0013
     prescribes, and tests assert the pair (`LG007`, `payment_transitions_fact_check`).
- **I9's guards:**
  1. **Rust.** Only the sender sends, and it looks a request up before every resend
     (ADR-0028).
  2. **The database.** At most one decision per request, under a unique key (ADR-0031).
  3. **A test,** for the PSP's side, which no guard of ours can see. The fake acts, loses
     its reply and forgets the key; the sender restarts; the fake has acted once. A
     control turns the lookup off and sees the fake act twice, which shows that the test
     can fail.
- **Tests of I8:** ADR-0031's property test; each guard broken alone, to show that the
  others still hold: the merchant's role inserting a `capture` step, refused by the
  policy, or a ledger line, refused with `42501`; `payments_app` inserting a step of the
  PSP's without its fact, refused with `LG007` at `COMMIT`; a fact out of position,
  refused by the foreign key; a catalog test that pins the merchant role's privileges and
  the new policy; and the backend killed around each statement of the catch-up, under
  ADR-0014's rule.
- **ADR-0013's registry** gains `LG007`.

## Consequences

- A migration revokes the merchant role's grants and adds the policy. Grants stay by
  column, as ADR-0012 requires.
- Phase 3's routes that wrote steps now record requests (ADR-0028), so their tests of
  steps move to the catch-up, on `payments_app`.
- What `LG007` does not stop: code running as `payments_app` that writes a step and a
  made-up fact together. The Rust guard covers that path, and the next catch-up finds a
  fact that the PSP's history does not hold, and stops the payment.
- DESIGN.md and the README gain I8 and I9, and `docs/correctness/` a page for the PSP,
  when the code lands.

## References

- J. H. Saltzer and M. D. Schroeder, [The Protection of Information in Computer Systems](https://doi.org/10.1109/PROC.1975.9939) (Proceedings of the IEEE, 1975), section I.A.3.
- NIST, [Least privilege](https://csrc.nist.gov/glossary/term/least_privilege) (CSRC glossary).
- PostgreSQL, [CREATE POLICY](https://www.postgresql.org/docs/18/sql-createpolicy.html) (PostgreSQL 18 docs).
