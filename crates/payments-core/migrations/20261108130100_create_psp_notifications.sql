-- Author: Nicola Antonello

-- The PSP's notifications (ADR-0030, ADR-0031, ADR-0032, ADR-0037). A verified one
-- is recorded once, by its PSP and its event ID, and kept forever, so a replay is
-- always a duplicate. It is thin: it names a payment, which may be one that the
-- service does not know, so it references nothing, and it decides nothing.
-- Catching up reads the PSP's history, which does.

CREATE TABLE psp_notifications (
    psp         text        NOT NULL,
    event_id    text        NOT NULL,
    payment_id  uuid        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT psp_notifications_pkey PRIMARY KEY (psp, event_id),
    -- The labels of payments.psp (ADR-0020).
    CONSTRAINT psp_notifications_psp_check CHECK (psp IN ('mock')),
    -- One shape, with no '.', so that the signed string msg_id.timestamp.payload
    -- reads one way only (ADR-0030).
    CONSTRAINT psp_notifications_event_id_check CHECK (event_id ~ '^[0-9A-Za-z_-]{1,255}$')
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER psp_notifications_append_only
    BEFORE UPDATE OR DELETE ON psp_notifications
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER psp_notifications_no_truncate
    BEFORE TRUNCATE ON psp_notifications
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE psp_notifications ENABLE ALWAYS TRIGGER psp_notifications_append_only;
ALTER TABLE psp_notifications ENABLE ALWAYS TRIGGER psp_notifications_no_truncate;

-- The webhook route runs on the system role (ADR-0030), which reads the
-- notifications and records them, every column except recorded_at (ADR-0012). A
-- merchant has nothing to do with them.
GRANT SELECT ON psp_notifications TO payments_app;
GRANT INSERT (psp, event_id, payment_id) ON psp_notifications TO payments_app;
