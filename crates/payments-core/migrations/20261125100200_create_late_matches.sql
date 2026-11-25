-- Author: Nicola Antonello

-- A line that settles late (ADR-0069, ADR-0072): a line that settled nothing of ours when
-- its batch was booked, because the PSP's history lacked its fact, settles its movement once
-- the fact arrives. The job writes a step in both chains, the movement's settle_late and the
-- discrepancy's evidence, and the record here, which holds the foreign keys of a match and
-- its posting. At COMMIT, LG010 refuses a late match whose posting is not exactly its lines,
-- LG011 either step without it, and LG013 now counts it as the match that a fee's difference
-- needs. Everything here is written only by the system role.

-- A movement settles late from its start or once overdue, and a line that settled nothing
-- of ours closes on its evidence (ADR-0069). Evidence after an outcome comes with the
-- reversals.
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
            ('overdue', 'write_off', 'written_off')
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

-- The target of a late match's key, which ties it to the discrepancy of its line.
ALTER TABLE settlement_unmatched_lines
    ADD CONSTRAINT settlement_unmatched_lines_late_match_key
    UNIQUE (discrepancy_id, batch_id, line_no);

-- A capture line that settles a capture of ours late, as a batch's capture match would
-- (ADR-0068), each rule a foreign key of its own.
CREATE TABLE settlement_late_capture_matches (
    discrepancy_id        uuid          NOT NULL,
    batch_id              uuid          NOT NULL,
    line_no               integer       NOT NULL,
    kind                  text          NOT NULL GENERATED ALWAYS AS ('capture') STORED,
    gross                 bigint        NOT NULL,
    psp                   text          NOT NULL,
    currency              currency_code NOT NULL,
    payment_id            uuid          NOT NULL,
    position              integer       NOT NULL,
    psp_fact_id           text          NOT NULL,
    request_id            uuid          NOT NULL,
    -- The position of the payment's settled fact (ADR-0068).
    settled_position      integer       NOT NULL,
    settled_kind          text          NOT NULL GENERATED ALWAYS AS ('settle') STORED,
    -- The movement's settle_late step and the discrepancy's evidence step, which this late
    -- match is the record of (ADR-0069).
    step_id               uuid          NOT NULL,
    step_kind             text          NOT NULL GENERATED ALWAYS AS ('settle_late') STORED,
    evidence_step_id      uuid          NOT NULL,
    evidence_kind         text          NOT NULL GENERATED ALWAYS AS ('evidence') STORED,
    ledger_transaction_id uuid          NOT NULL,
    CONSTRAINT settlement_late_capture_matches_pkey PRIMARY KEY (batch_id, line_no),
    CONSTRAINT settlement_late_capture_matches_unmatched_line_fkey
        FOREIGN KEY (discrepancy_id, batch_id, line_no)
        REFERENCES settlement_unmatched_lines (discrepancy_id, batch_id, line_no),
    CONSTRAINT settlement_late_capture_matches_line_fkey
        FOREIGN KEY (batch_id, line_no, kind, gross)
        REFERENCES settlement_lines (batch_id, line_no, kind, gross),
    CONSTRAINT settlement_late_capture_matches_psp_fkey
        FOREIGN KEY (batch_id, psp) REFERENCES settlement_batches (id, psp),
    CONSTRAINT settlement_late_capture_matches_currency_fkey
        FOREIGN KEY (batch_id, currency) REFERENCES settlement_batches (id, currency),
    CONSTRAINT settlement_late_capture_matches_fact_fkey
        FOREIGN KEY (psp, psp_fact_id, payment_id, position, kind, gross)
        REFERENCES payment_facts (psp, psp_fact_id, payment_id, position, kind, amount),
    CONSTRAINT settlement_late_capture_matches_fact_key UNIQUE (psp, psp_fact_id),
    CONSTRAINT settlement_late_capture_matches_settled_fkey
        FOREIGN KEY (payment_id, settled_position, settled_kind)
        REFERENCES payment_facts (payment_id, position, kind),
    CONSTRAINT settlement_late_capture_matches_settled_key
        UNIQUE (payment_id, settled_position),
    CONSTRAINT settlement_late_capture_matches_decision_fkey
        FOREIGN KEY (request_id, payment_id, position)
        REFERENCES payment_request_decisions (request_id, payment_id, position),
    CONSTRAINT settlement_late_capture_matches_payment_currency_fkey
        FOREIGN KEY (payment_id, currency) REFERENCES payments (id, currency),
    CONSTRAINT settlement_late_capture_matches_step_fkey
        FOREIGN KEY (step_id, payment_id, position, step_kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_late_capture_matches_step_id_key UNIQUE (step_id),
    CONSTRAINT settlement_late_capture_matches_evidence_fkey
        FOREIGN KEY (evidence_step_id, discrepancy_id, evidence_kind)
        REFERENCES settlement_discrepancy_steps (id, discrepancy_id, kind),
    CONSTRAINT settlement_late_capture_matches_evidence_key UNIQUE (evidence_step_id),
    CONSTRAINT settlement_late_capture_matches_ledger_transaction_id_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_late_capture_matches_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- A refund line that settles a refund of ours late, as a capture's late match without a
-- settled fact. Its gross is negative, and the fact's amount positive, so a generated column
-- carries the gross negated to the fact's key.
CREATE TABLE settlement_late_refund_matches (
    discrepancy_id        uuid          NOT NULL,
    batch_id              uuid          NOT NULL,
    line_no               integer       NOT NULL,
    kind                  text          NOT NULL GENERATED ALWAYS AS ('refund') STORED,
    gross                 bigint        NOT NULL,
    amount                bigint        NOT NULL GENERATED ALWAYS AS (-gross) STORED,
    psp                   text          NOT NULL,
    currency              currency_code NOT NULL,
    payment_id            uuid          NOT NULL,
    position              integer       NOT NULL,
    psp_fact_id           text          NOT NULL,
    request_id            uuid          NOT NULL,
    step_id               uuid          NOT NULL,
    step_kind             text          NOT NULL GENERATED ALWAYS AS ('settle_late') STORED,
    evidence_step_id      uuid          NOT NULL,
    evidence_kind         text          NOT NULL GENERATED ALWAYS AS ('evidence') STORED,
    ledger_transaction_id uuid          NOT NULL,
    CONSTRAINT settlement_late_refund_matches_pkey PRIMARY KEY (batch_id, line_no),
    CONSTRAINT settlement_late_refund_matches_unmatched_line_fkey
        FOREIGN KEY (discrepancy_id, batch_id, line_no)
        REFERENCES settlement_unmatched_lines (discrepancy_id, batch_id, line_no),
    CONSTRAINT settlement_late_refund_matches_line_fkey
        FOREIGN KEY (batch_id, line_no, kind, gross)
        REFERENCES settlement_lines (batch_id, line_no, kind, gross),
    CONSTRAINT settlement_late_refund_matches_psp_fkey
        FOREIGN KEY (batch_id, psp) REFERENCES settlement_batches (id, psp),
    CONSTRAINT settlement_late_refund_matches_currency_fkey
        FOREIGN KEY (batch_id, currency) REFERENCES settlement_batches (id, currency),
    CONSTRAINT settlement_late_refund_matches_fact_fkey
        FOREIGN KEY (psp, psp_fact_id, payment_id, position, kind, amount)
        REFERENCES payment_facts (psp, psp_fact_id, payment_id, position, kind, amount),
    CONSTRAINT settlement_late_refund_matches_fact_key UNIQUE (psp, psp_fact_id),
    CONSTRAINT settlement_late_refund_matches_decision_fkey
        FOREIGN KEY (request_id, payment_id, position)
        REFERENCES payment_request_decisions (request_id, payment_id, position),
    CONSTRAINT settlement_late_refund_matches_payment_currency_fkey
        FOREIGN KEY (payment_id, currency) REFERENCES payments (id, currency),
    CONSTRAINT settlement_late_refund_matches_step_fkey
        FOREIGN KEY (step_id, payment_id, position, step_kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_late_refund_matches_step_id_key UNIQUE (step_id),
    CONSTRAINT settlement_late_refund_matches_evidence_fkey
        FOREIGN KEY (evidence_step_id, discrepancy_id, evidence_kind)
        REFERENCES settlement_discrepancy_steps (id, discrepancy_id, kind),
    CONSTRAINT settlement_late_refund_matches_evidence_key UNIQUE (evidence_step_id),
    CONSTRAINT settlement_late_refund_matches_ledger_transaction_id_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_late_refund_matches_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- A late match's posting is exactly its line's (LG010, ADR-0069): the line's net out of
-- settlement_suspense, where booking left it, its fee to psp_fees and its gross off
-- psp_receivable, an entry of zero left out. The posting was written by this transaction,
-- in the batch's currency. This repeats ADR-0069's rule on purpose, as LG006 repeats a
-- capture's (ADR-0021). The comparisons have exact matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_late_match() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    line_gross       bigint;
    line_fee         bigint;
    line_net         bigint;
    psp_label        text;
    batch_currency   text;
    written_here     boolean;
    posting_currency text;
    differing        bigint;
BEGIN
    SELECT l.gross, l.fee, l.net, b.psp, b.currency::text
      INTO line_gross, line_fee, line_net, psp_label, batch_currency
      FROM settlement_lines l
      JOIN settlement_batches b ON b.id = l.batch_id
     WHERE l.batch_id = NEW.batch_id AND l.line_no = NEW.line_no;

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
        SELECT line.kind, line.party, line.amount
          FROM (VALUES
                    ('settlement_suspense', psp_label, line_net),
                    ('psp_fees', psp_label, line_fee),
                    ('psp_receivable', psp_label, -line_gross)
               ) AS line (kind, party, amount)
         WHERE line.amount <> 0::bigint
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

    IF NOT written_here OR posting_currency <> batch_currency OR differing <> 0::bigint THEN
        RAISE SQLSTATE 'LG010' USING
            MESSAGE = format(
                'the posting of the late match of line %s of batch %s is not exactly its lines',
                NEW.line_no, NEW.batch_id
            ),
            DETAIL = format(
                'Posting %s: written by this transaction: %s, currency %s against %s, '
                '%s entries differ.',
                NEW.ledger_transaction_id, written_here, posting_currency, batch_currency,
                differing
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs them at COMMIT, after the posting's entries.
CREATE CONSTRAINT TRIGGER settlement_late_capture_matches_commit_check
    AFTER INSERT ON settlement_late_capture_matches
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_late_match();
CREATE CONSTRAINT TRIGGER settlement_late_refund_matches_commit_check
    AFTER INSERT ON settlement_late_refund_matches
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_late_match();

-- A movement's step that needs a record has one (LG011, ADR-0069): a settle step its match,
-- now a settle_late step its late match, an overdue step its subject and an outcome's step
-- its outcome. The comparisons have exact matches in pg_catalog (ADR-0004).
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
-- outcome, and now a line's evidence its late match. The comparisons have exact matches in
-- pg_catalog (ADR-0004).
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

-- A fee's difference is on a line that matches (LG013, ADR-0068), on time or late. The
-- comparisons have exact matches in pg_catalog (ADR-0004).
CREATE OR REPLACE FUNCTION check_settlement_fee_difference() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
           SELECT 1 FROM settlement_capture_matches
            WHERE batch_id = NEW.batch_id AND line_no = NEW.line_no
       )
       AND NOT EXISTS (
           SELECT 1 FROM settlement_refund_matches
            WHERE batch_id = NEW.batch_id AND line_no = NEW.line_no
       )
       AND NOT EXISTS (
           SELECT 1 FROM settlement_late_capture_matches
            WHERE batch_id = NEW.batch_id AND line_no = NEW.line_no
       )
       AND NOT EXISTS (
           SELECT 1 FROM settlement_late_refund_matches
            WHERE batch_id = NEW.batch_id AND line_no = NEW.line_no
       ) THEN
        RAISE SQLSTATE 'LG013' USING
            MESSAGE = format(
                'fee difference %s is on line %s of batch %s, which matches nothing',
                NEW.discrepancy_id, NEW.line_no, NEW.batch_id
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER settlement_late_capture_matches_append_only
    BEFORE UPDATE OR DELETE ON settlement_late_capture_matches
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_late_capture_matches_no_truncate
    BEFORE TRUNCATE ON settlement_late_capture_matches
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_late_refund_matches_append_only
    BEFORE UPDATE OR DELETE ON settlement_late_refund_matches
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_late_refund_matches_no_truncate
    BEFORE TRUNCATE ON settlement_late_refund_matches
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_late_capture_matches
    ENABLE ALWAYS TRIGGER settlement_late_capture_matches_commit_check;
ALTER TABLE settlement_late_capture_matches
    ENABLE ALWAYS TRIGGER settlement_late_capture_matches_append_only;
ALTER TABLE settlement_late_capture_matches
    ENABLE ALWAYS TRIGGER settlement_late_capture_matches_no_truncate;
ALTER TABLE settlement_late_refund_matches
    ENABLE ALWAYS TRIGGER settlement_late_refund_matches_commit_check;
ALTER TABLE settlement_late_refund_matches
    ENABLE ALWAYS TRIGGER settlement_late_refund_matches_append_only;
ALTER TABLE settlement_late_refund_matches
    ENABLE ALWAYS TRIGGER settlement_late_refund_matches_no_truncate;

-- The system role reads them and inserts every column except the generated ones (ADR-0012).
-- The merchant's role has no privilege on either, so neither is under row-level security.
GRANT SELECT ON settlement_late_capture_matches, settlement_late_refund_matches
    TO payments_app;
GRANT INSERT (
    discrepancy_id, batch_id, line_no, gross, psp, currency, payment_id, position, psp_fact_id,
    request_id, settled_position, step_id, evidence_step_id, ledger_transaction_id
) ON settlement_late_capture_matches TO payments_app;
GRANT INSERT (
    discrepancy_id, batch_id, line_no, gross, psp, currency, payment_id, position, psp_fact_id,
    request_id, step_id, evidence_step_id, ledger_transaction_id
) ON settlement_late_refund_matches TO payments_app;
