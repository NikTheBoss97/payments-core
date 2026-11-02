-- Author: Nicola Antonello

-- Merchants and payments (ADR-0017): append-only rows of facts that never
-- change. A payment's state is not here: it is the newest row of its history.
-- Every CHECK, key and foreign key is named, because tests and the application
-- match on the name.

CREATE TABLE merchants (
    id          uuid        NOT NULL,
    -- The start of the inserting database transaction (ADR-0012).
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT merchants_pkey PRIMARY KEY (id),
    CONSTRAINT merchants_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7)
);

CREATE TABLE payments (
    id          uuid          NOT NULL,
    merchant_id uuid          NOT NULL,
    -- A closed set of labels, parsed once into a Rust enum (ADR-0020).
    psp         text          NOT NULL,
    -- No foreign key to currencies: the payment's accounts are opened first,
    -- and their key checks the currency, while a key here would share-lock the
    -- currency's one row on every new payment.
    currency    currency_code NOT NULL,
    -- The requested amount, up to the cap of a ledger amount (ADR-0003).
    amount      bigint        NOT NULL,
    -- The fee rate in basis points, fixed for the payment's life (ADR-0020).
    fee_bps     integer       NOT NULL,
    recorded_at timestamptz   NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT payments_pkey PRIMARY KEY (id),
    CONSTRAINT payments_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT payments_merchant_fkey FOREIGN KEY (merchant_id) REFERENCES merchants (id),
    CONSTRAINT payments_psp_check CHECK (psp IN ('mock')),
    CONSTRAINT payments_amount_check CHECK (amount BETWEEN 1 AND 1000000000000000),
    CONSTRAINT payments_fee_bps_check CHECK (fee_bps BETWEEN 0 AND 10000),
    -- The target of the authorizations' foreign key, which carries the
    -- requested amount across to check it.
    CONSTRAINT payments_id_amount_key UNIQUE (id, amount)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER merchants_append_only
    BEFORE UPDATE OR DELETE ON merchants
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER merchants_no_truncate
    BEFORE TRUNCATE ON merchants
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payments_append_only
    BEFORE UPDATE OR DELETE ON payments
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payments_no_truncate
    BEFORE TRUNCATE ON payments
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE merchants ENABLE ALWAYS TRIGGER merchants_append_only;
ALTER TABLE merchants ENABLE ALWAYS TRIGGER merchants_no_truncate;
ALTER TABLE payments ENABLE ALWAYS TRIGGER payments_append_only;
ALTER TABLE payments ENABLE ALWAYS TRIGGER payments_no_truncate;

-- The runtime role reads both tables and inserts every column except
-- recorded_at, which only its default may fill (ADR-0012). Never grant INSERT
-- on a whole table: a column-level REVOKE cannot take that back.
GRANT SELECT ON merchants, payments TO payments_app;
GRANT INSERT (id) ON merchants TO payments_app;
GRANT INSERT (id, merchant_id, psp, currency, amount, fee_bps) ON payments TO payments_app;
