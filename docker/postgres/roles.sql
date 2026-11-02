-- Author: Nicola Antonello

-- Cluster-wide roles for local development and CI (ADR-0008). A role is
-- shared by every database in the server, so it is created here, once, and
-- never in a per-database migration. The password is for local use only.

-- Runtime role used by the application (ADR-0004). It is not a superuser,
-- owns no tables and belongs to no other role, so it cannot disable the
-- ledger's triggers. Migrations grant it SELECT and INSERT.
CREATE ROLE payments_app LOGIN PASSWORD 'payments_app'
    NOSUPERUSER NOCREATEDB NOCREATEROLE;

-- A session left idle inside an open transaction keeps its locks until the
-- server notices. After a lost COMMIT, this bounds the wait to 30 seconds
-- (ADR-0005).
ALTER ROLE payments_app SET idle_in_transaction_session_timeout = '30s';

-- A retried posting waits for a first attempt's rows (ADR-0005). The limit
-- applies to each lock wait and is longer than the idle timeout above, so a
-- retry outlasts a first attempt left idle after a lost COMMIT, then gets a
-- definite answer.
ALTER ROLE payments_app SET lock_timeout = '35s';

-- A session-level advisory lock outlives its transaction. The library never
-- takes one, but hand-written SQL could leak it, and no other timeout ends a
-- session idle outside a transaction. sqlx's pool keeps an idle connection for at
-- most about 20 minutes, so this ends only sessions the pool no longer manages
-- (ADR-0018).
ALTER ROLE payments_app SET idle_session_timeout = '30min';
