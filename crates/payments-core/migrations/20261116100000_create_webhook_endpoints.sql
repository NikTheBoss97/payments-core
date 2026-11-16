-- Author: Nicola Antonello

-- A merchant's webhook endpoints and their disablements (ADR-0050, ADR-0055):
-- append-only rows of facts. The operator registers an endpoint and may disable it,
-- and the relay disables one that answers 410 or keeps failing (ADR-0053); both run
-- as the system role. An endpoint's secret is never stored: the relay derives it from
-- the master key and the endpoint's ID (ADR-0051).

CREATE TABLE webhook_endpoints (
    id          uuid        NOT NULL,
    merchant_id uuid        NOT NULL,
    -- Absolute and http, without user information, which would store a credential,
    -- and without a fragment (ADR-0050): http://, an authority that is not empty and
    -- holds no '/', '?', '#' or '@', then no '#', whitespace or control character.
    -- The server parses it first; this keeps a credential out of the table even under
    -- raw SQL. The build that adds TLS changes it with its parser.
    url         text        NOT NULL,
    -- The start of the inserting database transaction (ADR-0012).
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT webhook_endpoints_pkey PRIMARY KEY (id),
    CONSTRAINT webhook_endpoints_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT webhook_endpoints_merchant_fkey FOREIGN KEY (merchant_id) REFERENCES merchants (id),
    CONSTRAINT webhook_endpoints_url_check
        CHECK (url ~ '^http://[^/?#@[:space:][:cntrl:]]+([/?][^#[:space:][:cntrl:]]*)?$'),
    -- The target of the deliveries' foreign key, which proves that an endpoint belongs
    -- to its event's merchant (ADR-0054).
    CONSTRAINT webhook_endpoints_id_merchant_key UNIQUE (id, merchant_id)
);

-- An endpoint that is disabled stays disabled (ADR-0050): by the operator, on a 410,
-- or once one of its deliveries has gone three days without a success (ADR-0053).
CREATE TABLE webhook_endpoint_disablements (
    endpoint_id uuid        NOT NULL,
    -- A closed set of labels, parsed once into a Rust enum (ADR-0009).
    reason      text        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT webhook_endpoint_disablements_pkey PRIMARY KEY (endpoint_id),
    CONSTRAINT webhook_endpoint_disablements_endpoint_fkey
        FOREIGN KEY (endpoint_id) REFERENCES webhook_endpoints (id),
    CONSTRAINT webhook_endpoint_disablements_reason_check
        CHECK (reason IN ('operator', 'gone', 'failing'))
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER webhook_endpoints_append_only
    BEFORE UPDATE OR DELETE ON webhook_endpoints
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_endpoints_no_truncate
    BEFORE TRUNCATE ON webhook_endpoints
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_endpoint_disablements_append_only
    BEFORE UPDATE OR DELETE ON webhook_endpoint_disablements
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_endpoint_disablements_no_truncate
    BEFORE TRUNCATE ON webhook_endpoint_disablements
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE webhook_endpoints ENABLE ALWAYS TRIGGER webhook_endpoints_append_only;
ALTER TABLE webhook_endpoints ENABLE ALWAYS TRIGGER webhook_endpoints_no_truncate;
ALTER TABLE webhook_endpoint_disablements
    ENABLE ALWAYS TRIGGER webhook_endpoint_disablements_append_only;
ALTER TABLE webhook_endpoint_disablements
    ENABLE ALWAYS TRIGGER webhook_endpoint_disablements_no_truncate;

-- The system role reads both tables and inserts every column except recorded_at,
-- which only its default may fill (ADR-0012). Never grant INSERT on a whole table: a
-- column-level REVOKE cannot take that back. The merchant's role has no privilege on
-- either, so neither is under row-level security (ADR-0025, ADR-0055).
GRANT SELECT ON webhook_endpoints, webhook_endpoint_disablements TO payments_app;
GRANT INSERT (id, merchant_id, url) ON webhook_endpoints TO payments_app;
GRANT INSERT (endpoint_id, reason) ON webhook_endpoint_disablements TO payments_app;
