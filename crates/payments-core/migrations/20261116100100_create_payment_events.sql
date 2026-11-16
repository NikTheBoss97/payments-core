-- Author: Nicola Antonello

-- Events and their deliveries (ADR-0049, ADR-0052, ADR-0053, ADR-0055). The catch-up
-- writes one event for each fact that it applies, and one delivery for each endpoint
-- of the merchant enabled at that moment, in the fact's own transaction; the relay
-- records a success when an endpoint answers with a 2xx. All are append-only rows of
-- facts, written only by the system role: the merchant's role has no privilege on any.

-- The target of the events' foreign key, which proves that an event's merchant is its
-- payment's (ADR-0054).
ALTER TABLE payments ADD CONSTRAINT payments_id_merchant_key UNIQUE (id, merchant_id);

CREATE TABLE payment_events (
    id          uuid        NOT NULL,
    merchant_id uuid        NOT NULL,
    payment_id  uuid        NOT NULL,
    -- The position of the fact that the event announces (ADR-0049).
    position    integer     NOT NULL,
    -- The ID of the inserting transaction, which the relay searches by (ADR-0052).
    -- Only this default fills it: no role may insert into it. In a savepoint it is
    -- still the top-level transaction's, unlike a row's xmin (ADR-0021).
    xact_id     xid8        NOT NULL DEFAULT pg_current_xact_id(),
    -- The start of the inserting database transaction (ADR-0012), which the payload
    -- carries as its timestamp (ADR-0049).
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT payment_events_pkey PRIMARY KEY (id),
    CONSTRAINT payment_events_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT payment_events_payment_fkey
        FOREIGN KEY (payment_id, merchant_id) REFERENCES payments (id, merchant_id),
    CONSTRAINT payment_events_fact_fkey
        FOREIGN KEY (payment_id, position) REFERENCES payment_facts (payment_id, position),
    -- One event per fact (ADR-0049).
    CONSTRAINT payment_events_fact_key UNIQUE (payment_id, position),
    -- The targets of the deliveries' foreign keys, which carry an event's merchant and
    -- its transaction across.
    CONSTRAINT payment_events_id_merchant_key UNIQUE (id, merchant_id),
    CONSTRAINT payment_events_id_xact_id_key UNIQUE (id, xact_id)
);

-- One per event and endpoint enabled when the event was written (ADR-0053). It
-- commits with its event, so it has no recorded_at of its own (ADR-0012, ADR-0055).
CREATE TABLE webhook_deliveries (
    id          uuid NOT NULL,
    event_id    uuid NOT NULL,
    endpoint_id uuid NOT NULL,
    merchant_id uuid NOT NULL,
    -- The ID of the inserting transaction, which must be its event's (ADR-0052).
    xact_id     xid8 NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT webhook_deliveries_pkey PRIMARY KEY (id),
    CONSTRAINT webhook_deliveries_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT webhook_deliveries_event_endpoint_key UNIQUE (event_id, endpoint_id),
    -- I7 (ADR-0054): the event and the endpoint belong to the delivery's merchant, so
    -- an event reaches only the endpoints of its own merchant.
    CONSTRAINT webhook_deliveries_event_fkey
        FOREIGN KEY (event_id, merchant_id) REFERENCES payment_events (id, merchant_id),
    CONSTRAINT webhook_deliveries_endpoint_fkey
        FOREIGN KEY (endpoint_id, merchant_id) REFERENCES webhook_endpoints (id, merchant_id),
    -- I10 (ADR-0052): a delivery written in any later transaction than its event's has
    -- no event to match, so none can appear below a mark that has passed it.
    CONSTRAINT webhook_deliveries_transaction_fkey
        FOREIGN KEY (event_id, xact_id) REFERENCES payment_events (id, xact_id)
);

-- The relay's search reads the unfinished deliveries from a transaction ID up
-- (ADR-0052).
CREATE INDEX webhook_deliveries_xact_id_idx ON webhook_deliveries (xact_id);

-- One per delivery that an endpoint answered with a 2xx (ADR-0053).
CREATE TABLE webhook_delivery_successes (
    delivery_id uuid        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT webhook_delivery_successes_pkey PRIMARY KEY (delivery_id),
    CONSTRAINT webhook_delivery_successes_delivery_fkey
        FOREIGN KEY (delivery_id) REFERENCES webhook_deliveries (id)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER payment_events_append_only
    BEFORE UPDATE OR DELETE ON payment_events
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_events_no_truncate
    BEFORE TRUNCATE ON payment_events
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_deliveries_append_only
    BEFORE UPDATE OR DELETE ON webhook_deliveries
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_deliveries_no_truncate
    BEFORE TRUNCATE ON webhook_deliveries
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_delivery_successes_append_only
    BEFORE UPDATE OR DELETE ON webhook_delivery_successes
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER webhook_delivery_successes_no_truncate
    BEFORE TRUNCATE ON webhook_delivery_successes
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_events ENABLE ALWAYS TRIGGER payment_events_append_only;
ALTER TABLE payment_events ENABLE ALWAYS TRIGGER payment_events_no_truncate;
ALTER TABLE webhook_deliveries ENABLE ALWAYS TRIGGER webhook_deliveries_append_only;
ALTER TABLE webhook_deliveries ENABLE ALWAYS TRIGGER webhook_deliveries_no_truncate;
ALTER TABLE webhook_delivery_successes
    ENABLE ALWAYS TRIGGER webhook_delivery_successes_append_only;
ALTER TABLE webhook_delivery_successes
    ENABLE ALWAYS TRIGGER webhook_delivery_successes_no_truncate;

-- The system role reads the three tables and inserts every column except xact_id and
-- recorded_at, which only their defaults may fill (ADR-0012, ADR-0052). Never grant
-- INSERT on a whole table: a column-level REVOKE cannot take that back. The merchant's
-- role has no privilege on any, so none is under row-level security (ADR-0025,
-- ADR-0055).
GRANT SELECT ON payment_events, webhook_deliveries, webhook_delivery_successes TO payments_app;
GRANT INSERT (id, merchant_id, payment_id, position) ON payment_events TO payments_app;
GRANT INSERT (id, event_id, endpoint_id, merchant_id) ON webhook_deliveries TO payments_app;
GRANT INSERT (delivery_id) ON webhook_delivery_successes TO payments_app;
