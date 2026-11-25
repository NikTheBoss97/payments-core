-- Author: Nicola Antonello

-- A missing batch that arrived (ADR-0069, ADR-0070): once the batch is booked, the job
-- closes its discrepancy with an evidence step, whether or not an operator accepted it
-- first, and the record here names the batch. An acceptance posts nothing, so nothing is
-- reversed. At COMMIT, LG011 refuses the evidence step without it. Everything here is
-- written only by the system role.

-- A missing batch closes on its evidence, from open or after its acceptance (ADR-0069).
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
            ('missing_batch', 'open', 'evidence', 'closed'),
            ('missing_batch', 'accepted', 'evidence', 'closed'),
            ('uncaused_posting', 'open', 'find', 'open'),
            ('uncaused_posting', 'open', 'accept', 'accepted')
        )
    );

-- The target of an arrival's key, which ties it to the batch that its discrepancy missed.
ALTER TABLE settlement_missing_batches
    ADD CONSTRAINT settlement_missing_batches_arrival_key
    UNIQUE (discrepancy_id, psp, currency, number);

-- The record of a missing batch's evidence step: the batch that arrived.
CREATE TABLE settlement_batch_arrivals (
    step_id        uuid          NOT NULL,
    discrepancy_id uuid          NOT NULL,
    step_kind      text          NOT NULL GENERATED ALWAYS AS ('evidence') STORED,
    psp            text          NOT NULL,
    currency       currency_code NOT NULL,
    number         bigint        NOT NULL,
    CONSTRAINT settlement_batch_arrivals_pkey PRIMARY KEY (step_id),
    CONSTRAINT settlement_batch_arrivals_step_fkey
        FOREIGN KEY (step_id, discrepancy_id, step_kind)
        REFERENCES settlement_discrepancy_steps (id, discrepancy_id, kind),
    -- The batch that the discrepancy missed, and that is now booked.
    CONSTRAINT settlement_batch_arrivals_missing_fkey
        FOREIGN KEY (discrepancy_id, psp, currency, number)
        REFERENCES settlement_missing_batches (discrepancy_id, psp, currency, number),
    CONSTRAINT settlement_batch_arrivals_batch_fkey
        FOREIGN KEY (psp, currency, number)
        REFERENCES settlement_batches (psp, currency, number)
);

-- A discrepancy's step that needs a record has one (LG011, ADR-0069): an outcome's step its
-- outcome; evidence for a line its late match, and its reversal after an outcome that
-- posted; and now evidence for a missing batch its arrival. The comparisons have exact
-- matches in pg_catalog (ADR-0004).
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
    ELSIF NEW.kind = 'evidence' AND NEW.discrepancy_kind = 'missing_batch' THEN
        has_record := EXISTS (
            SELECT 1 FROM settlement_batch_arrivals WHERE step_id = NEW.id
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
CREATE TRIGGER settlement_batch_arrivals_append_only
    BEFORE UPDATE OR DELETE ON settlement_batch_arrivals
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_batch_arrivals_no_truncate
    BEFORE TRUNCATE ON settlement_batch_arrivals
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_batch_arrivals
    ENABLE ALWAYS TRIGGER settlement_batch_arrivals_append_only;
ALTER TABLE settlement_batch_arrivals
    ENABLE ALWAYS TRIGGER settlement_batch_arrivals_no_truncate;

-- The system role reads it and inserts every column except the generated one (ADR-0012).
-- The merchant's role has no privilege on it, so it is not under row-level security.
GRANT SELECT ON settlement_batch_arrivals TO payments_app;
GRANT INSERT (step_id, discrepancy_id, psp, currency, number)
    ON settlement_batch_arrivals TO payments_app;
