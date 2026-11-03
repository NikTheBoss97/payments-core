-- Author: Nicola Antonello

-- The final answers to requests that carry an idempotency key (ADR-0026). A row
-- commits in the same transaction as its effect and is never changed or removed,
-- so no row means that no attempt left an effect, and a row means that its answer
-- is final. A merchant's keys and the operator's are apart, because an operator is
-- not a merchant.

CREATE TABLE idempotency_records (
    merchant_id uuid        NOT NULL,
    -- Visible ASCII without '"' or '\', so its Structured Field string needs no
    -- escapes, and at most 255 characters.
    key         text        NOT NULL,
    -- The SHA-256 of the request's method, path and raw body, which tells a retry
    -- from a key reused for another request.
    fingerprint bytea       NOT NULL,
    -- Only a final answer is stored: no 5xx, which would mean an unknown outcome.
    status      smallint    NOT NULL,
    -- Text rather than jsonb, so that a replay returns the same bytes.
    body        text        NOT NULL,
    -- The start of the inserting database transaction (ADR-0012).
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT idempotency_records_pkey PRIMARY KEY (merchant_id, key),
    CONSTRAINT idempotency_records_merchant_fkey
        FOREIGN KEY (merchant_id) REFERENCES merchants (id),
    CONSTRAINT idempotency_records_key_check CHECK (key ~ '^[!#-\[\]-~]{1,255}$'),
    CONSTRAINT idempotency_records_fingerprint_check CHECK (octet_length(fingerprint) = 32),
    CONSTRAINT idempotency_records_status_check CHECK (status BETWEEN 200 AND 499),
    CONSTRAINT idempotency_records_body_check CHECK (body IS JSON)
);

CREATE TABLE operator_idempotency_records (
    key         text        NOT NULL,
    fingerprint bytea       NOT NULL,
    status      smallint    NOT NULL,
    body        text        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT operator_idempotency_records_pkey PRIMARY KEY (key),
    CONSTRAINT operator_idempotency_records_key_check CHECK (key ~ '^[!#-\[\]-~]{1,255}$'),
    CONSTRAINT operator_idempotency_records_fingerprint_check
        CHECK (octet_length(fingerprint) = 32),
    CONSTRAINT operator_idempotency_records_status_check CHECK (status BETWEEN 200 AND 499),
    CONSTRAINT operator_idempotency_records_body_check CHECK (body IS JSON)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER idempotency_records_append_only
    BEFORE UPDATE OR DELETE ON idempotency_records
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER idempotency_records_no_truncate
    BEFORE TRUNCATE ON idempotency_records
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER operator_idempotency_records_append_only
    BEFORE UPDATE OR DELETE ON operator_idempotency_records
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER operator_idempotency_records_no_truncate
    BEFORE TRUNCATE ON operator_idempotency_records
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE idempotency_records ENABLE ALWAYS TRIGGER idempotency_records_append_only;
ALTER TABLE idempotency_records ENABLE ALWAYS TRIGGER idempotency_records_no_truncate;
ALTER TABLE operator_idempotency_records
    ENABLE ALWAYS TRIGGER operator_idempotency_records_append_only;
ALTER TABLE operator_idempotency_records
    ENABLE ALWAYS TRIGGER operator_idempotency_records_no_truncate;

-- Each role touches only its own table, and every column except recorded_at, which
-- only its default may fill (ADR-0012, ADR-0025).
GRANT SELECT ON idempotency_records TO payments_tenant;
GRANT INSERT (merchant_id, key, fingerprint, status, body)
    ON idempotency_records TO payments_tenant;
GRANT SELECT ON operator_idempotency_records TO payments_app;
GRANT INSERT (key, fingerprint, status, body)
    ON operator_idempotency_records TO payments_app;

-- A merchant reaches only its own answers (ADR-0025), written out as in the other
-- policies. With no WITH CHECK, USING also judges new rows.
ALTER TABLE idempotency_records ENABLE ROW LEVEL SECURITY;
CREATE POLICY idempotency_records_tenant_isolation ON idempotency_records TO payments_tenant
    USING (merchant_id = NULLIF(current_setting('app.merchant', true), '')::uuid);
