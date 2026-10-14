-- Author: Nicola Antonello

-- Currencies reference data (ADR-0003) and the append-only guard that every
-- ledger table uses (ADR-0004).

-- An ISO 4217 alphabetic code: three uppercase letters. A domain, so every
-- currency column applies the same rule. NOT NULL belongs on the columns,
-- because a domain's CHECK passes when the value is NULL.
CREATE DOMAIN currency_code AS text
    CHECK (VALUE ~ '^[A-Z]{3}$');

CREATE TABLE currencies (
    code     currency_code NOT NULL PRIMARY KEY,
    -- Decimals of the currency's minor unit in ISO 4217 (JPY 0, EUR 2, KWD 3).
    decimals smallint      NOT NULL CHECK (decimals BETWEEN 0 AND 4)
);

-- Rejects every change to a stored row. Mistakes are corrected by inserting
-- new rows, never by editing old ones. LG001 is this project's SQLSTATE for
-- the violation, so callers can tell it apart from other errors.
CREATE FUNCTION reject_change() RETURNS trigger
    LANGUAGE plpgsql
    -- Look up nothing in the session's temporary schema first (ADR-0004).
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on % is not allowed: the table is append-only', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'LG001';
END;
$$;

-- Row-level for UPDATE and DELETE; TRUNCATE has only a statement-level form.
CREATE TRIGGER currencies_append_only
    BEFORE UPDATE OR DELETE ON currencies
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER currencies_no_truncate
    BEFORE TRUNCATE ON currencies
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE currencies ENABLE ALWAYS TRIGGER currencies_append_only;
ALTER TABLE currencies ENABLE ALWAYS TRIGGER currencies_no_truncate;

-- ISK has no decimals in ISO 4217 although some PSPs send it with two, which
-- the mock PSP will exercise (ADR-0003).
INSERT INTO currencies (code, decimals) VALUES
    ('EUR', 2),
    ('GBP', 2),
    ('USD', 2),
    ('JPY', 0),
    ('ISK', 0),
    ('KWD', 3);

-- The runtime role only reads currencies; only migrations write them.
GRANT SELECT ON currencies TO payments_app;
