-- Author: Nicola Antonello

-- Raise append-only violations in the form that ADR-0013 sets for every
-- database guard: the message, plus the trigger, table and schema in their
-- own error fields, so that a program can tell which guard fired without
-- parsing the message.
--
-- CREATE OR REPLACE keeps the function's identity, so the triggers that
-- already call it run the new body. It resets every property the command
-- leaves out, except the owner and the privileges, so the pinned
-- search_path (ADR-0004) is stated again.

CREATE OR REPLACE FUNCTION reject_change() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    RAISE SQLSTATE 'LG001' USING
        MESSAGE = format('%s on %s is not allowed: the table is append-only', TG_OP, TG_TABLE_NAME),
        CONSTRAINT = TG_NAME,
        TABLE = TG_TABLE_NAME,
        SCHEMA = TG_TABLE_SCHEMA;
END;
$$;
