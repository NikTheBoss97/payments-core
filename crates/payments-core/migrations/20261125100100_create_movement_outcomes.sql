-- Author: Nicola Antonello

-- An operator's outcome on an overdue movement (ADR-0069, ADR-0072): a claim on the PSP or
-- a write-off, a step of the movement's chain, which is its discrepancy's, with its record
-- here, the reason given for it and its posting. At COMMIT, LG010 refuses an outcome whose
-- posting is not exactly its lines, and LG011 an outcome's step without its record.
-- Everything here is written only by the system role.

-- An overdue movement can now be claimed or written off (ADR-0069). Settling after an
-- outcome comes with the reversals.
ALTER TABLE settlement_movement_steps
    DROP CONSTRAINT settlement_movement_steps_edge_check,
    ADD CONSTRAINT settlement_movement_steps_edge_check CHECK (
        (kind = 'open') = (seq = 0)
        AND (from_state, kind, to_state) IN (
            ('open', 'open', 'open'),
            ('open', 'settle', 'settled'),
            ('open', 'overdue', 'overdue'),
            ('overdue', 'settle', 'settled'),
            ('overdue', 'claim', 'claimed'),
            ('overdue', 'write_off', 'written_off')
        )
    );

-- The record of an outcome's step, which commits with it, and its posting.
CREATE TABLE settlement_movement_outcomes (
    step_id               uuid    NOT NULL,
    payment_id            uuid    NOT NULL,
    position              integer NOT NULL,
    kind                  text    NOT NULL,
    -- Why the operator chose it, shown in the discrepancy's history (ADR-0069).
    reason                text    NOT NULL,
    ledger_transaction_id uuid    NOT NULL,
    CONSTRAINT settlement_movement_outcomes_pkey PRIMARY KEY (step_id),
    CONSTRAINT settlement_movement_outcomes_step_fkey
        FOREIGN KEY (step_id, payment_id, position, kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_movement_outcomes_kind_check CHECK (kind IN ('claim', 'write_off')),
    -- Characters, not bytes, as the route counts them.
    CONSTRAINT settlement_movement_outcomes_reason_check
        CHECK (char_length(reason) BETWEEN 1 AND 500),
    CONSTRAINT settlement_movement_outcomes_ledger_transaction_id_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_movement_outcomes_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- An outcome's posting is exactly its lines (LG010, ADR-0069): the movement's amount moves
-- off psp_receivable to psp_claims (the PSP's) or settlement_writeoffs (the platform's). A
-- capture left its amount there, owed by the PSP; a refund left its amount owed to it, so
-- a refund's moves negative. The posting was written by this transaction, in the payment's
-- currency. This repeats ADR-0069's rule on purpose, as LG006 repeats a capture's
-- (ADR-0021). The comparisons have exact matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_movement_outcome() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    moved            bigint;
    psp_label        text;
    payment_currency text;
    to_kind          text;
    to_party         text;
    written_here     boolean;
    posting_currency text;
    differing        bigint;
BEGIN
    SELECT CASE s.fact_kind WHEN 'capture' THEN s.amount ELSE -s.amount END
      INTO moved
      FROM settlement_movement_steps s
     WHERE s.id = NEW.step_id;
    -- The currencies are compared as text: pg_catalog has no operator for the
    -- currency_code domain (ADR-0004).
    SELECT p.psp, p.currency::text
      INTO psp_label, payment_currency
      FROM payments p
     WHERE p.id = NEW.payment_id;
    IF NEW.kind = 'claim' THEN
        to_kind := 'psp_claims';
        to_party := psp_label;
    ELSE
        to_kind := 'settlement_writeoffs';
        to_party := 'platform';
    END IF;

    -- A row written inside a savepoint carries the savepoint's own transaction ID, so a
    -- posting written there fails this test, as LG006's does (ADR-0021).
    SELECT t.xmin = pg_current_xact_id()::xid, t.currency::text
      INTO written_here, posting_currency
      FROM ledger_transactions t
     WHERE t.id = NEW.ledger_transaction_id;

    -- Set operations compare with each type's default equality, not through the search
    -- path. EXCEPT ALL in both directions finds a missing entry, an extra one, a changed
    -- amount and a wrong account.
    WITH expected (kind, party, amount) AS (
        VALUES ('psp_receivable', psp_label, -moved),
               (to_kind, to_party, moved)
    ), actual (kind, party, amount) AS (
        SELECT a.kind, a.party, e.amount
          FROM ledger_entries e
          JOIN accounts a ON a.id = e.account_id
         WHERE e.transaction_id = NEW.ledger_transaction_id
    )
    SELECT count(*)
      INTO differing
      FROM (
          (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual)
          UNION ALL
          (SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
      ) AS difference;

    IF NOT written_here OR posting_currency <> payment_currency OR differing <> 0::bigint THEN
        RAISE SQLSTATE 'LG010' USING
            MESSAGE = format('the posting of outcome %s is not exactly its lines', NEW.step_id),
            DETAIL = format(
                'Posting %s: written by this transaction: %s, currency %s against %s, '
                '%s entries differ.',
                NEW.ledger_transaction_id, written_here, posting_currency, payment_currency,
                differing
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT, after the posting's entries.
CREATE CONSTRAINT TRIGGER settlement_movement_outcomes_commit_check
    AFTER INSERT ON settlement_movement_outcomes
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_movement_outcome();

-- A movement's step that needs a record has one (LG011, ADR-0069): a settle step its match,
-- an overdue step its subject, and now an outcome's step its outcome. The comparisons have
-- exact matches in pg_catalog (ADR-0004).
CREATE OR REPLACE FUNCTION check_settlement_movement_step_record() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_record boolean;
BEGIN
    IF NEW.kind = 'settle' THEN
        has_record := EXISTS (SELECT 1 FROM settlement_capture_matches WHERE step_id = NEW.id)
                      OR EXISTS (SELECT 1 FROM settlement_refund_matches WHERE step_id = NEW.id);
    ELSIF NEW.kind = 'overdue' THEN
        has_record := EXISTS (SELECT 1 FROM settlement_overdue_movements WHERE step_id = NEW.id);
    ELSIF NEW.kind IN ('claim', 'write_off') THEN
        has_record := EXISTS (SELECT 1 FROM settlement_movement_outcomes WHERE step_id = NEW.id);
    ELSE
        RETURN NULL;
    END IF;
    IF NOT has_record THEN
        RAISE SQLSTATE 'LG011' USING
            MESSAGE = format('movement step %s of kind %s has no record', NEW.id, NEW.kind),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER settlement_movement_outcomes_append_only
    BEFORE UPDATE OR DELETE ON settlement_movement_outcomes
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_movement_outcomes_no_truncate
    BEFORE TRUNCATE ON settlement_movement_outcomes
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_movement_outcomes
    ENABLE ALWAYS TRIGGER settlement_movement_outcomes_commit_check;
ALTER TABLE settlement_movement_outcomes
    ENABLE ALWAYS TRIGGER settlement_movement_outcomes_append_only;
ALTER TABLE settlement_movement_outcomes
    ENABLE ALWAYS TRIGGER settlement_movement_outcomes_no_truncate;

-- The system role reads it and inserts every column (ADR-0012). The merchant's role has no
-- privilege on it, so it is not under row-level security.
GRANT SELECT ON settlement_movement_outcomes TO payments_app;
GRANT INSERT (step_id, payment_id, position, kind, reason, ledger_transaction_id)
    ON settlement_movement_outcomes TO payments_app;
