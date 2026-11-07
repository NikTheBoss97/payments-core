<!-- Author: Nicola Antonello -->

# ADR-0025: Tenant isolation

- Status: accepted; partly superseded by ADR-0034
- Date: 2026-11-03

## Context

A merchant must reach only its own payments (invariant I7). OWASP ranks getting this
wrong first among API risks: an endpoint trusts an object's ID from the client, and
one forgotten check exposes another customer's data. This project enforces each
invariant twice, in Rust and in PostgreSQL, and its owner chose to hold isolation to
that rule.

Row-level security reaches into what phase 2 built, so its behaviour was measured on
PostgreSQL 18.6 before this was written. The results are stated where they decide
something.

## Options

How isolation is enforced:

| Option | Why not |
|---|---|
| **One check in Rust, and row-level security** | Chosen |
| One check in Rust alone | A bug in that check, or a path that skips it, reaches another merchant's rows with nothing behind it |
| A schema or database per merchant | The ledger is one set of books that must sum to zero across every merchant, and each migration would run once per merchant |
| A database role per merchant | Roles are cluster-wide (ADR-0008), and a pooled connection belongs to one role |

How a transaction names its merchant:

| Option | Why not |
|---|---|
| **`set_config(..., true)` in each transaction** | Chosen |
| A session-level setting | A pooled connection keeps it, so the next request can inherit another merchant |
| A privileged login that runs `SET LOCAL ROLE` per transaction | A path that forgets runs with every privilege: it fails open |

How the system paths reach every merchant's rows:

| Option | Why not |
|---|---|
| **A second login role with `BYPASSRLS`, whose pool merchant routes never hold** | Chosen |
| One role, and a setting that means "all merchants" | Any bug that sets it opens everything, on the same pool as merchant requests |
| Policies of `USING (true)` for the second role | The same effect, and one more policy per table to keep |

How a table without a merchant column is filtered:

| Option | Why not |
|---|---|
| **A subquery on `payments`, itself under its policy** | Chosen |
| A `merchant_id` copied onto each table, under a composite foreign key | Cheaper per row, but it changes four tables, every insert and every phase-2 test that writes raw SQL, to save a cost measured below |

## Decision

- **Two login roles.** `payments_tenant` is new, and row-level security applies to
  it. Merchant routes hold only its pool, so a path that forgets to name a merchant
  sees nothing and can write nothing: it fails closed. `payments_app` gains
  `BYPASSRLS` and becomes the system role: operator routes, looking up an API key,
  and later the webhooks, the outbox relay and reconciliation. Neither belongs to the
  other, and both get the session timeouts of ADR-0005 and ADR-0018.
- **What each can touch.** `payments_tenant` has the privileges of ADR-0012 and
  ADR-0017, except that it cannot insert into `merchants`, and has none at all on
  `api_keys`, `api_key_revocations` and `operator_idempotency_records`. `payments_app`
  has none on `idempotency_records`, which only a merchant's request writes.
- **Which tables.** `merchants`, `payments`, `payment_transitions`,
  `payment_authorizations`, `payment_captures`, `payment_refunds` and
  `idempotency_records`. Not the ledger's tables, `accounts`, `currencies` or
  `account_kinds`: one posting touches a merchant's account and the platform's, the
  ledger knows nothing of causes (ADR-0011), and no route reads them.
- **The policies** are named `<table>_tenant_isolation` and apply to `payments_tenant`.
  A table with a merchant compares it with
  `NULLIF(current_setting('app.merchant', true), '')::uuid`. A step or record instead
  tests `EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id)`, which is itself
  filtered by the policy on `payments`. The expression is written out in each policy
  rather than in a function, because a policy is stored parsed and so has no
  `search_path` to attack (ADR-0004).
- **Why the `NULLIF`.** Measured: the setting is NULL in a fresh session, but `''` once
  a transaction has set it and ended. `''::uuid` raises `22P02`, which would turn every
  later statement on that pooled connection into an error.
- **Naming the merchant.** Every transaction of a merchant's request that touches those
  tables runs `SELECT set_config('app.merchant', $1, true)` first. In a command that is
  the second statement: the lock of ADR-0018 stays first. Reads get a transaction too.
  Never at session level. Measured: when the transaction ends, the setting goes with
  it, and a later statement on that connection sees no rows.
- **In the library,** a `Tenant` holds a pool and a merchant, and is the only way to a
  merchant's transaction.
- **In HTTP,** one extractor gives every `/payments/{id}` route its ID. It reads the
  payment as the merchant and compares the payment's merchant with the credential's,
  answering 404 for both a stranger's payment and none, so an ID's existence does not
  leak. A payment's merchant never changes, so the check cannot go stale, and the
  race the manual warns of for sub-selects in a policy cannot occur either.
- **What a refusal means.** A refused write raises `42501`, with no constraint name.
  It is permanent, so class `42` joins what the classifier of ADR-0019 reads as
  `Invariant`. Until now that code fell through to `NotConfirmed`, which told a caller
  to retry something that can never succeed.
- **No `FORCE ROW LEVEL SECURITY`.** Only migrations run as the owner, and that role
  is outside the failure model.

## Consequences

- Measured with 200,000 payments and 1,000,000 steps: reading one payment's rows went
  from 0.054 ms to 0.083 ms, one primary-key lookup per row, and inserting 20,000 rows
  from 255 ms to 283 ms, about 11%. A query that forgets to filter by payment pays
  for its whole merchant: 303 ms for 500,000 rows. Phase 8 measures these properly.
- A read on a merchant's behalf costs four round trips instead of one.
- **What this does not stop.** The tenant role may set the setting itself: measured, a
  session that names another merchant sees that merchant's rows. So this guards
  against a forgotten or wrong merchant, not against SQL written on purpose to cross
  over. That is a smaller promise than I2's, and the failure model states it.
- Also measured as refused for the tenant role: `SET ROLE`, `SET SESSION AUTHORIZATION`,
  `ALTER TABLE ... DISABLE ROW LEVEL SECURITY`, and `ALTER ROLE ... BYPASSRLS`. `SET
  row_security = off` raises an error rather than bypassing anything.
- The tenant role can read the ledger's tables and every account, because the commands
  and their checks need them. No route exposes them.
- A key collision reveals that a row exists, and the manual warns of this covert
  channel. Measured: `23505` is raised for a row the session cannot see. Here IDs are
  made by the server, and an idempotency key's primary key includes its merchant, so
  nothing a merchant chooses can collide with another merchant's row.
- Foreign-key checks bypass row security, as the manual says, so they still work
  across the policies.
- Deferred checks run as the tenant role and see the transaction's rows through the
  policies. Measured: at `COMMIT` the setting is still in place. Their tests must run
  as that role too.
- sqlx's cached statements are safe: measured under `force_generic_plan`, one prepared
  statement gave each merchant its own rows on one session.
- Two pools, so more connections to budget.
- Phase-2 tests stay on `payments_app`, unchanged, and test the guards as before. New
  tests run the commands as the tenant role, try every route across merchants, and
  break each guard alone to show that the other still holds. Catalog tests pin each
  table's `relrowsecurity`, every policy's text and each role's `rolbypassrls`.
- `roles.sql` changes, so a local volume needs `docker compose down -v` (ADR-0008).

## Amendments

- 2026-11-07: ADR-0034 supersedes "`payments_tenant` has the privileges of ADR-0012 and
  ADR-0017". In phase 4 a merchant's command records a request to the PSP instead of a
  step (ADR-0028), so the role loses `INSERT` on `ledger_transactions`,
  `ledger_entries`, `payment_authorizations`, `payment_captures` and `payment_refunds`,
  and a restrictive policy, `payment_transitions_tenant_steps`, lets it insert only
  `create` and `cancel` steps. The webhooks and the sweep run on `payments_app`, as this
  record planned.

## References

- PostgreSQL, [5.9. Row Security Policies](https://www.postgresql.org/docs/18/ddl-rowsecurity.html) (PostgreSQL 18 docs).
- OWASP, [API1:2023 Broken Object Level Authorization](https://api-security.owasp.org/editions/2023/en/0xa1-broken-object-level-authorization) (OWASP API Security Top 10, 2023).
- AWS, [Row-level security recommendations](https://docs.aws.amazon.com/prescriptive-guidance/latest/saas-multitenant-managed-postgresql/rls.html) (AWS Prescriptive Guidance).
- Wen Zhang, Eric Sheng, Michael Chang, Aurojit Panda, Mooly Sagiv and Scott Shenker, [Blockaid: Data Access Policy Enforcement for Web Applications](https://www.usenix.org/conference/osdi22/presentation/zhang) (OSDI, 2022).
- Shariq Rizvi, Alberto Mendelzon, S. Sudarshan and Prasan Roy, [Extending Query Rewriting Techniques for Fine-Grained Access Control](https://doi.org/10.1145/1007568.1007631) (SIGMOD, 2004).
