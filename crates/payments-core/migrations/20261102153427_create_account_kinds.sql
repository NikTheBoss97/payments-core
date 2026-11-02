-- Author: Nicola Antonello

-- The chart of accounts (ADR-0020): every account's kind and normal side must
-- be a row of this table. It holds exactly the kinds that phase 2 posts to; a
-- later phase adds a kind in a migration of its own when it first posts to it.

CREATE TABLE account_kinds (
    kind        text NOT NULL,
    -- The side on which this kind's balances increase (ADR-0003).
    normal_side text NOT NULL,
    -- One row per kind, so each kind has one normal side.
    CONSTRAINT account_kinds_pkey PRIMARY KEY (kind),
    -- The same rules as the accounts' CHECKs, so a bad seed fails here rather
    -- than leaving a row that no account could ever use.
    CONSTRAINT account_kinds_kind_check CHECK (kind ~ '^[a-z][a-z0-9_]{0,62}$'),
    CONSTRAINT account_kinds_normal_side_check CHECK (normal_side IN ('debit', 'credit')),
    -- The target of the accounts' foreign key.
    CONSTRAINT account_kinds_kind_normal_side_key UNIQUE (kind, normal_side)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER account_kinds_append_only
    BEFORE UPDATE OR DELETE ON account_kinds
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER account_kinds_no_truncate
    BEFORE TRUNCATE ON account_kinds
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE account_kinds ENABLE ALWAYS TRIGGER account_kinds_append_only;
ALTER TABLE account_kinds ENABLE ALWAYS TRIGGER account_kinds_no_truncate;

INSERT INTO account_kinds (kind, normal_side) VALUES
    -- What the PSP owes the platform.
    ('psp_receivable', 'debit'),
    -- What the platform owes a merchant.
    ('merchant_payable', 'credit'),
    -- The platform's fees.
    ('fee_revenue', 'credit');

-- Checked only when an account is created, which is rare: entries reference
-- accounts, not kinds, so no posting touches the chart.
ALTER TABLE accounts
    ADD CONSTRAINT accounts_kind_normal_side_fkey
    FOREIGN KEY (kind, normal_side) REFERENCES account_kinds (kind, normal_side);

-- The runtime role only reads the chart; only migrations write it.
GRANT SELECT ON account_kinds TO payments_app;
