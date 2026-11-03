-- Author: Nicola Antonello

-- A merchant's API keys and their revocations (ADR-0024): append-only rows of
-- facts. A secret is never stored, only its SHA-256 digest, and revoking a key
-- appends a row. Only the system role touches these tables, and it looks a key up
-- before any merchant is known (ADR-0025).

CREATE TABLE api_keys (
    id          uuid        NOT NULL,
    merchant_id uuid        NOT NULL,
    -- The SHA-256 digest of the secret: 32 bytes, unique, and looked up directly.
    digest      bytea       NOT NULL,
    -- The start of the inserting database transaction (ADR-0012).
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT api_keys_pkey PRIMARY KEY (id),
    CONSTRAINT api_keys_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT api_keys_merchant_fkey FOREIGN KEY (merchant_id) REFERENCES merchants (id),
    CONSTRAINT api_keys_digest_key UNIQUE (digest),
    CONSTRAINT api_keys_digest_check CHECK (octet_length(digest) = 32)
);

CREATE TABLE api_key_revocations (
    api_key_id  uuid        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT api_key_revocations_pkey PRIMARY KEY (api_key_id),
    CONSTRAINT api_key_revocations_api_key_fkey
        FOREIGN KEY (api_key_id) REFERENCES api_keys (id)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER api_keys_append_only
    BEFORE UPDATE OR DELETE ON api_keys
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER api_keys_no_truncate
    BEFORE TRUNCATE ON api_keys
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER api_key_revocations_append_only
    BEFORE UPDATE OR DELETE ON api_key_revocations
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER api_key_revocations_no_truncate
    BEFORE TRUNCATE ON api_key_revocations
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE api_keys ENABLE ALWAYS TRIGGER api_keys_append_only;
ALTER TABLE api_keys ENABLE ALWAYS TRIGGER api_keys_no_truncate;
ALTER TABLE api_key_revocations ENABLE ALWAYS TRIGGER api_key_revocations_append_only;
ALTER TABLE api_key_revocations ENABLE ALWAYS TRIGGER api_key_revocations_no_truncate;

-- The system role reads both tables and inserts every column except recorded_at,
-- which only its default may fill (ADR-0012). Never grant INSERT on a whole table:
-- a column-level REVOKE cannot take that back.
GRANT SELECT ON api_keys, api_key_revocations TO payments_app;
GRANT INSERT (id, merchant_id, digest) ON api_keys TO payments_app;
GRANT INSERT (api_key_id) ON api_key_revocations TO payments_app;
