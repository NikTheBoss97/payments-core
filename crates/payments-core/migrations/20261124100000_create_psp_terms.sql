-- Author: Nicola Antonello

-- The accounts that reconciliation posts to (ADR-0067, ADR-0069), seeded by the
-- migration that first lets a posting reach them (ADR-0020).
INSERT INTO account_kinds (kind, normal_side) VALUES
    -- The payouts that the PSP reports sending, which no bank statement confirms.
    ('psp_payouts', 'debit'),
    -- The PSP's fees on the platform's own movements: an expense.
    ('psp_fees', 'debit'),
    -- Money that the PSP reported and that nothing of ours explains yet.
    ('settlement_suspense', 'credit'),
    -- What the PSP owes outside settlement, negative when we owe it.
    ('psp_claims', 'debit'),
    -- What reconciliation wrote off: an expense, negative for a gain.
    ('settlement_writeoffs', 'debit');

-- The terms agreed with a PSP in one currency, from a business time on (ADR-0068): the
-- fee on a capture, and the delay within which a movement must settle. Only migrations
-- write them; a route for the operator to add terms is a later need.
CREATE TABLE psp_terms (
    psp           text          NOT NULL,
    currency      currency_code NOT NULL,
    -- From when the terms hold, a business time (ADR-0059).
    starts_at     timestamptz   NOT NULL,
    -- The fee on a capture of C is (C * fee_bps + 5000) div 10000 + fee_fixed, half up
    -- (ADR-0066).
    fee_bps       integer       NOT NULL,
    fee_fixed     bigint        NOT NULL,
    -- The seconds within which a movement must settle, or be overdue (ADR-0068).
    settle_within bigint        NOT NULL,
    recorded_at   timestamptz   NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT psp_terms_pkey PRIMARY KEY (psp, currency, starts_at),
    CONSTRAINT psp_terms_psp_check CHECK (psp IN ('mock')),
    -- Terms are written rarely, so the key's lock on the currency's row costs nothing.
    CONSTRAINT psp_terms_currency_fkey FOREIGN KEY (currency) REFERENCES currencies (code),
    CONSTRAINT psp_terms_starts_at_check
        CHECK (starts_at BETWEEN '0001-01-01 00:00:00+00 BC' AND '9999-12-31 23:59:59.999999+00'),
    CONSTRAINT psp_terms_fee_bps_check CHECK (fee_bps BETWEEN 0 AND 10000),
    CONSTRAINT psp_terms_fee_fixed_check CHECK (fee_fixed BETWEEN 0 AND 1000000000000000),
    -- At most 366 days.
    CONSTRAINT psp_terms_settle_within_check CHECK (settle_within BETWEEN 1 AND 31622400)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER psp_terms_append_only
    BEFORE UPDATE OR DELETE ON psp_terms
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER psp_terms_no_truncate
    BEFORE TRUNCATE ON psp_terms
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE psp_terms ENABLE ALWAYS TRIGGER psp_terms_append_only;
ALTER TABLE psp_terms ENABLE ALWAYS TRIGGER psp_terms_no_truncate;

-- The mock's own terms, in every currency, from RFC 3339's year 0000 (ADR-0072): 140
-- basis points and 25 minor units on each capture, settling within two days.
INSERT INTO psp_terms (psp, currency, starts_at, fee_bps, fee_fixed, settle_within)
SELECT 'mock', code, '0001-01-01 00:00:00+00 BC', 140, 25, 172800
  FROM currencies;

-- The system role reads the terms; only migrations write them.
GRANT SELECT ON psp_terms TO payments_app;
