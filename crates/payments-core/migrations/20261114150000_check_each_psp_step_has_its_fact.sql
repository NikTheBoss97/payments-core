-- Author: Nicola Antonello

-- Only the PSP's word moves a payment (ADR-0034): every step but a payment's first and a
-- cancel comes from a fact of the PSP's history, tied to it in payment_transition_facts
-- (LG007, ADR-0013). The two kinds that need no fact are named rather than the seven that
-- do, so a kind added later needs a fact until a decision says otherwise (ADR-0048). The
-- comparisons below have exact matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_payment_step_fact() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF NEW.kind = 'create' OR NEW.kind = 'cancel' THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM payment_transition_facts WHERE transition_id = NEW.id) THEN
        RAISE SQLSTATE 'LG007' USING
            MESSAGE = format('payment step %s of kind %s has no fact', NEW.id, NEW.kind),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT: the link's foreign key needs the step to exist
-- first. Its name sorts after payment_transitions_commit_check, which PostgreSQL fires
-- first on the same row, so a step that lacks both its record and its fact is reported as
-- LG004 (ADR-0048).
CREATE CONSTRAINT TRIGGER payment_transitions_fact_check
    AFTER INSERT ON payment_transitions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_payment_step_fact();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_transitions ENABLE ALWAYS TRIGGER payment_transitions_fact_check;
