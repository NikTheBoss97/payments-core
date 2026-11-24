-- Author: Nicola Antonello

-- A fee's difference is on a line that matches (LG013, ADR-0068, ADR-0072). Only a line
-- that settles a movement of ours has its fee booked and checked against the terms, so a
-- difference on any other line would let a claim move money out of psp_fees that never
-- went in. At booking, the line's match is written in the same transaction; resolving's
-- tables add the late matches to this check. The comparisons have exact matches in
-- pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_fee_difference() RETURNS trigger
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

-- INITIALLY DEFERRED runs it at COMMIT, whichever of the difference and the match is
-- written first.
CREATE CONSTRAINT TRIGGER settlement_fee_differences_commit_check
    AFTER INSERT ON settlement_fee_differences
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_fee_difference();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_fee_differences
    ENABLE ALWAYS TRIGGER settlement_fee_differences_commit_check;
