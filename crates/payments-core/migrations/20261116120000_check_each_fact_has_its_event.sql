-- Author: Nicola Antonello

-- I6 (ADR-0054): a fact of the PSP's commits with the event that announces it
-- (ADR-0049). The catch-up writes each event in the fact's transaction (ADR-0057), and a
-- fact without one is refused at COMMIT with LG008 (ADR-0013), as a step of the PSP's
-- without its fact is with LG007 (ADR-0048). The comparisons below have exact matches in
-- pg_catalog (ADR-0004).
CREATE FUNCTION check_payment_fact_event() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM payment_events e
                    WHERE e.payment_id = NEW.payment_id AND e.position = NEW.position) THEN
        RAISE SQLSTATE 'LG008' USING
            MESSAGE = format('fact %s of payment %s has no event', NEW.position, NEW.payment_id),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT: the event's foreign key needs the fact to exist
-- first.
CREATE CONSTRAINT TRIGGER payment_facts_event_check
    AFTER INSERT ON payment_facts
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_payment_fact_event();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_facts ENABLE ALWAYS TRIGGER payment_facts_event_check;
