-- Author: Nicola Antonello

-- An early outcome reversed (ADR-0069, ADR-0072): when the evidence of a timing difference
-- arrives after an operator's claim or write-off, the job reverses the outcome with its
-- mirror posting, in the step that settles the movement or closes the discrepancy, and
-- books as usual. The record here names the step that carries it and the outcome that it
-- reverses, and holds the posting. At COMMIT, LG010 refuses a reversal whose posting is not
-- exactly its outcome's mirrored, and LG011 a step after a posted outcome without its
-- reversal. Everything here is written only by the system role.

-- A movement claimed or written off settles later, on time or late, and a line claimed or
-- written off closes on its evidence (ADR-0069).
ALTER TABLE settlement_movement_steps
    DROP CONSTRAINT settlement_movement_steps_edge_check,
    ADD CONSTRAINT settlement_movement_steps_edge_check CHECK (
        (kind = 'open') = (seq = 0)
        AND (from_state, kind, to_state) IN (
            ('open', 'open', 'open'),
            ('open', 'settle', 'settled'),
            ('open', 'settle_late', 'settled'),
            ('open', 'overdue', 'overdue'),
            ('overdue', 'settle', 'settled'),
            ('overdue', 'settle_late', 'settled'),
            ('overdue', 'claim', 'claimed'),
            ('overdue', 'write_off', 'written_off'),
            ('claimed', 'settle', 'settled'),
            ('claimed', 'settle_late', 'settled'),
            ('written_off', 'settle', 'settled'),
            ('written_off', 'settle_late', 'settled')
        )
    );
ALTER TABLE settlement_discrepancy_steps
    DROP CONSTRAINT settlement_discrepancy_steps_edge_check,
    ADD CONSTRAINT settlement_discrepancy_steps_edge_check CHECK (
        (kind = 'find') = (seq = 0)
        AND (discrepancy_kind, from_state, kind, to_state) IN (
            ('unmatched_line', 'open', 'find', 'open'),
            ('unmatched_line', 'open', 'claim', 'claimed'),
            ('unmatched_line', 'open', 'write_off', 'written_off'),
            ('unmatched_line', 'open', 'evidence', 'closed'),
            ('unmatched_line', 'claimed', 'evidence', 'closed'),
            ('unmatched_line', 'written_off', 'evidence', 'closed'),
            ('payout_difference', 'open', 'find', 'open'),
            ('payout_difference', 'open', 'claim', 'claimed'),
            ('payout_difference', 'open', 'write_off', 'written_off'),
            ('fee_difference', 'open', 'find', 'open'),
            ('fee_difference', 'open', 'claim', 'claimed'),
            ('fee_difference', 'open', 'accept', 'accepted'),
            ('missing_batch', 'open', 'find', 'open'),
            ('missing_batch', 'open', 'accept', 'accepted'),
            ('uncaused_posting', 'open', 'find', 'open'),
            ('uncaused_posting', 'open', 'accept', 'accepted')
        )
    );

-- The targets of the reversals' keys, which tie a reversal to an outcome of its own subject.
ALTER TABLE settlement_movement_outcomes
    ADD CONSTRAINT settlement_movement_outcomes_reversal_key
    UNIQUE (step_id, payment_id, position);
ALTER TABLE settlement_discrepancy_outcomes
    ADD CONSTRAINT settlement_discrepancy_outcomes_reversal_key UNIQUE (step_id, discrepancy_id);

-- The reversal of a movement's outcome, carried by the step that settles the movement.
CREATE TABLE settlement_movement_reversals (
    step_id               uuid    NOT NULL,
    payment_id            uuid    NOT NULL,
    position              integer NOT NULL,
    -- settle or settle_late: on time, in a later batch, or late.
    step_kind             text    NOT NULL,
    outcome_step_id       uuid    NOT NULL,
    ledger_transaction_id uuid    NOT NULL,
    CONSTRAINT settlement_movement_reversals_pkey PRIMARY KEY (step_id),
    CONSTRAINT settlement_movement_reversals_step_fkey
        FOREIGN KEY (step_id, payment_id, position, step_kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_movement_reversals_step_kind_check
        CHECK (step_kind IN ('settle', 'settle_late')),
    -- An outcome of the same movement, reversed once.
    CONSTRAINT settlement_movement_reversals_outcome_fkey
        FOREIGN KEY (outcome_step_id, payment_id, position)
        REFERENCES settlement_movement_outcomes (step_id, payment_id, position),
    CONSTRAINT settlement_movement_reversals_outcome_key UNIQUE (outcome_step_id),
    CONSTRAINT settlement_movement_reversals_ledger_transaction_id_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_movement_reversals_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- The reversal of a discrepancy's outcome that posted, carried by its evidence step.
CREATE TABLE settlement_discrepancy_reversals (
    step_id               uuid NOT NULL,
    discrepancy_id        uuid NOT NULL,
    step_kind             text NOT NULL GENERATED ALWAYS AS ('evidence') STORED,
    outcome_step_id       uuid NOT NULL,
    ledger_transaction_id uuid NOT NULL,
    CONSTRAINT settlement_discrepancy_reversals_pkey PRIMARY KEY (step_id),
    CONSTRAINT settlement_discrepancy_reversals_step_fkey
        FOREIGN KEY (step_id, discrepancy_id, step_kind)
        REFERENCES settlement_discrepancy_steps (id, discrepancy_id, kind),
    -- An outcome of the same discrepancy, reversed once, and one that posted.
    CONSTRAINT settlement_discrepancy_reversals_outcome_fkey
        FOREIGN KEY (outcome_step_id, discrepancy_id)
        REFERENCES settlement_discrepancy_outcomes (step_id, discrepancy_id),
    CONSTRAINT settlement_discrepancy_reversals_posting_fkey
        FOREIGN KEY (outcome_step_id)
        REFERENCES settlement_discrepancy_outcome_postings (step_id),
    CONSTRAINT settlement_discrepancy_reversals_outcome_key UNIQUE (outcome_step_id),
    CONSTRAINT settlement_discrepancy_reversals_ledger_transaction_id_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_discrepancy_reversals_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- A reversal's posting is exactly its outcome's mirrored (LG010, ADR-0069): each entry of
-- the outcome's posting, on the same account, negated, and no other. The posting was
-- written by this transaction, in the outcome's currency. The comparisons have exact
-- matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_reversal() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    reversed          uuid;
    reversed_currency text;
    written_here      boolean;
    posting_currency  text;
    differing         bigint;
BEGIN
    IF TG_TABLE_NAME = 'settlement_movement_reversals' THEN
        SELECT o.ledger_transaction_id INTO reversed
          FROM settlement_movement_outcomes o
         WHERE o.step_id = NEW.outcome_step_id;
    ELSE
        SELECT o.ledger_transaction_id INTO reversed
          FROM settlement_discrepancy_outcome_postings o
         WHERE o.step_id = NEW.outcome_step_id;
    END IF;
    SELECT t.currency::text INTO reversed_currency
      FROM ledger_transactions t
     WHERE t.id = reversed;
    -- A row written inside a savepoint carries the savepoint's own transaction ID, so a
    -- posting written there fails this test, as LG006's does (ADR-0021).
    SELECT t.xmin = pg_current_xact_id()::xid, t.currency::text
      INTO written_here, posting_currency
      FROM ledger_transactions t
     WHERE t.id = NEW.ledger_transaction_id;

    -- Set operations compare with each type's default equality, not through the search
    -- path. EXCEPT ALL in both directions finds a missing entry, an extra one, a changed
    -- amount and a wrong account.
    WITH expected (account_id, amount) AS (
        SELECT e.account_id, -e.amount
          FROM ledger_entries e
         WHERE e.transaction_id = reversed
    ), actual (account_id, amount) AS (
        SELECT e.account_id, e.amount
          FROM ledger_entries e
         WHERE e.transaction_id = NEW.ledger_transaction_id
    )
    SELECT count(*)
      INTO differing
      FROM (
          (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual)
          UNION ALL
          (SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
      ) AS difference;

    IF NOT written_here OR posting_currency <> reversed_currency OR differing <> 0::bigint THEN
        RAISE SQLSTATE 'LG010' USING
            MESSAGE = format(
                'the posting of the reversal of outcome %s is not its mirror',
                NEW.outcome_step_id
            ),
            DETAIL = format(
                'Posting %s against %s: written by this transaction: %s, currency %s against '
                '%s, %s entries differ.',
                NEW.ledger_transaction_id, reversed, written_here, posting_currency,
                reversed_currency, differing
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs them at COMMIT, after the posting's entries.
CREATE CONSTRAINT TRIGGER settlement_movement_reversals_commit_check
    AFTER INSERT ON settlement_movement_reversals
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_reversal();
CREATE CONSTRAINT TRIGGER settlement_discrepancy_reversals_commit_check
    AFTER INSERT ON settlement_discrepancy_reversals
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_reversal();

-- A movement's step that needs a record has one (LG011, ADR-0069): a settle step its match,
-- a settle_late step its late match, an overdue step its subject, an outcome's step its
-- outcome, and now a step that settles after an outcome its reversal as well. The
-- comparisons have exact matches in pg_catalog (ADR-0004).
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
    ELSIF NEW.kind = 'settle_late' THEN
        has_record := EXISTS (
                          SELECT 1 FROM settlement_late_capture_matches WHERE step_id = NEW.id
                      )
                      OR EXISTS (
                          SELECT 1 FROM settlement_late_refund_matches WHERE step_id = NEW.id
                      );
    ELSIF NEW.kind = 'overdue' THEN
        has_record := EXISTS (SELECT 1 FROM settlement_overdue_movements WHERE step_id = NEW.id);
    ELSIF NEW.kind IN ('claim', 'write_off') THEN
        has_record := EXISTS (SELECT 1 FROM settlement_movement_outcomes WHERE step_id = NEW.id);
    ELSE
        RETURN NULL;
    END IF;
    IF NEW.from_state IN ('claimed', 'written_off') THEN
        has_record := has_record
                      AND EXISTS (
                          SELECT 1 FROM settlement_movement_reversals WHERE step_id = NEW.id
                      );
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

-- A discrepancy's step that needs a record has one (LG011, ADR-0069): an outcome's step its
-- outcome, a line's evidence its late match, and now evidence after an outcome that posted
-- its reversal as well. The comparisons have exact matches in pg_catalog (ADR-0004).
CREATE OR REPLACE FUNCTION check_settlement_discrepancy_step_record() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_record boolean;
BEGIN
    IF NEW.kind IN ('claim', 'write_off', 'accept') THEN
        has_record := EXISTS (
            SELECT 1 FROM settlement_discrepancy_outcomes WHERE step_id = NEW.id
        );
    ELSIF NEW.kind = 'evidence' THEN
        has_record := EXISTS (
                          SELECT 1 FROM settlement_late_capture_matches
                           WHERE evidence_step_id = NEW.id
                      )
                      OR EXISTS (
                          SELECT 1 FROM settlement_late_refund_matches
                           WHERE evidence_step_id = NEW.id
                      );
        -- The outcome before it, which posted, is reversed.
        IF EXISTS (
               SELECT 1
                 FROM settlement_discrepancy_steps s
                 JOIN settlement_discrepancy_outcome_postings p ON p.step_id = s.id
                WHERE s.discrepancy_id = NEW.discrepancy_id AND s.seq = NEW.prev_seq
           ) THEN
            has_record := has_record
                          AND EXISTS (
                              SELECT 1 FROM settlement_discrepancy_reversals
                               WHERE step_id = NEW.id
                          );
        END IF;
    ELSE
        RETURN NULL;
    END IF;
    IF NOT has_record THEN
        RAISE SQLSTATE 'LG011' USING
            MESSAGE = format('discrepancy step %s of kind %s has no record', NEW.id, NEW.kind),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER settlement_movement_reversals_append_only
    BEFORE UPDATE OR DELETE ON settlement_movement_reversals
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_movement_reversals_no_truncate
    BEFORE TRUNCATE ON settlement_movement_reversals
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_reversals_append_only
    BEFORE UPDATE OR DELETE ON settlement_discrepancy_reversals
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_reversals_no_truncate
    BEFORE TRUNCATE ON settlement_discrepancy_reversals
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_movement_reversals
    ENABLE ALWAYS TRIGGER settlement_movement_reversals_commit_check;
ALTER TABLE settlement_movement_reversals
    ENABLE ALWAYS TRIGGER settlement_movement_reversals_append_only;
ALTER TABLE settlement_movement_reversals
    ENABLE ALWAYS TRIGGER settlement_movement_reversals_no_truncate;
ALTER TABLE settlement_discrepancy_reversals
    ENABLE ALWAYS TRIGGER settlement_discrepancy_reversals_commit_check;
ALTER TABLE settlement_discrepancy_reversals
    ENABLE ALWAYS TRIGGER settlement_discrepancy_reversals_append_only;
ALTER TABLE settlement_discrepancy_reversals
    ENABLE ALWAYS TRIGGER settlement_discrepancy_reversals_no_truncate;

-- The system role reads them and inserts every column except the generated one
-- (ADR-0012). The merchant's role has no privilege on either, so neither is under row-level
-- security.
GRANT SELECT ON settlement_movement_reversals, settlement_discrepancy_reversals
    TO payments_app;
GRANT INSERT (step_id, payment_id, position, step_kind, outcome_step_id, ledger_transaction_id)
    ON settlement_movement_reversals TO payments_app;
GRANT INSERT (step_id, discrepancy_id, outcome_step_id, ledger_transaction_id)
    ON settlement_discrepancy_reversals TO payments_app;
