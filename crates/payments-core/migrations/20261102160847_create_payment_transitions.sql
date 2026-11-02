-- Author: Nicola Antonello

-- A payment's history (ADR-0017): one append-only row per step. The row with
-- the highest seq is the payment's state, and no other copy of it exists.
-- Constraints alone keep the lifecycle (ADR-0016) and the refund limit, for any
-- statement the runtime role can run, at any isolation level.

CREATE TABLE payment_transitions (
    payment_id      uuid        NOT NULL,
    -- 0 for the first step, then one more than prev_seq.
    seq             integer     NOT NULL,
    prev_seq        integer     NOT NULL,
    -- The command's TransitionId: unique across all payments, so a retry finds
    -- its step by this alone (ADR-0019).
    id              uuid        NOT NULL,
    kind            text        NOT NULL,
    from_status     text        NOT NULL,
    to_status       text        NOT NULL,
    amount          bigint      NOT NULL,
    -- What can still be refunded before and after this step. Written once and
    -- fixed by its predecessor through the constraints below, so it cannot
    -- drift: not a balance in ADR-0002's sense.
    prev_refundable bigint      NOT NULL,
    refundable      bigint      NOT NULL,
    recorded_at     timestamptz NOT NULL DEFAULT transaction_timestamp(),
    -- Only one row can follow any state: a second row after the same
    -- predecessor has the same (payment_id, seq).
    CONSTRAINT payment_transitions_pkey PRIMARY KEY (payment_id, seq),
    CONSTRAINT payment_transitions_id_key UNIQUE (id),
    CONSTRAINT payment_transitions_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT payment_transitions_payment_fkey FOREIGN KEY (payment_id) REFERENCES payments (id),
    -- Every row starts from its predecessor's status and refundable amount. The
    -- first row points to itself, so every column stays NOT NULL.
    CONSTRAINT payment_transitions_state_key UNIQUE (payment_id, seq, to_status, refundable),
    CONSTRAINT payment_transitions_prev_fkey
        FOREIGN KEY (payment_id, prev_seq, from_status, prev_refundable)
        REFERENCES payment_transitions (payment_id, seq, to_status, refundable),
    -- The target of the records' foreign keys, which tie a record to a step of
    -- the right kind with the same amount.
    CONSTRAINT payment_transitions_step_key UNIQUE (id, payment_id, kind, amount),
    CONSTRAINT payment_transitions_seq_check
        CHECK ((seq = 0 AND prev_seq = 0) OR (seq >= 1 AND prev_seq = seq - 1)),
    -- The moves of ADR-0016. PostgreSQL runs a table's CHECKs in the order of
    -- their names, so an illegal move always fails here, with this one pair of
    -- code and name, before any other rule.
    CONSTRAINT payment_transitions_edge_check CHECK (
        (kind = 'create') = (seq = 0)
        AND (from_status, kind, to_status) IN (
            ('created', 'create', 'created'),
            ('created', 'authorize', 'authorized'),
            ('created', 'decline', 'failed'),
            ('created', 'cancel', 'voided'),
            ('authorized', 'capture', 'captured'),
            ('authorized', 'void', 'voided'),
            ('authorized', 'expire', 'voided'),
            ('captured', 'settle', 'settled'),
            ('captured', 'refund', 'captured'),
            ('captured', 'refund', 'refunded'),
            ('settled', 'refund', 'settled'),
            ('settled', 'refund', 'refunded')
        )
    ),
    -- Each kind's amount rule. The limits that need other rows, such as an
    -- authorization's limit of the requested amount, are on the records.
    CONSTRAINT payment_transitions_money_check CHECK (
        CASE kind
            WHEN 'create' THEN
                amount = 0 AND prev_refundable = 0 AND refundable = 0
            WHEN 'authorize' THEN
                amount >= 1 AND refundable = prev_refundable
            WHEN 'capture' THEN
                amount >= 1 AND prev_refundable = 0 AND refundable = amount
            WHEN 'refund' THEN
                amount >= 1
                AND amount <= prev_refundable
                AND refundable = prev_refundable - amount
                AND (to_status = 'refunded') = (refundable = 0)
            ELSE
                amount = 0 AND refundable = prev_refundable
        END
    )
);

-- A payment with no first step has no state at all (LG005, ADR-0013). The
-- comparisons below have exact matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_payment_genesis() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM payment_transitions WHERE payment_id = NEW.id AND seq = 0
    ) THEN
        RAISE SQLSTATE 'LG005' USING
            MESSAGE = format('payment %s has no first step', NEW.id),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT, after the first step's own INSERT.
CREATE CONSTRAINT TRIGGER payments_commit_check
    AFTER INSERT ON payments
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_payment_genesis();

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER payment_transitions_append_only
    BEFORE UPDATE OR DELETE ON payment_transitions
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_transitions_no_truncate
    BEFORE TRUNCATE ON payment_transitions
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payments ENABLE ALWAYS TRIGGER payments_commit_check;
ALTER TABLE payment_transitions ENABLE ALWAYS TRIGGER payment_transitions_append_only;
ALTER TABLE payment_transitions ENABLE ALWAYS TRIGGER payment_transitions_no_truncate;

-- The runtime role reads the history and inserts every column except
-- recorded_at (ADR-0012). It cannot lock a row: every locking clause of SELECT
-- needs UPDATE on the table (ADR-0018).
GRANT SELECT ON payment_transitions TO payments_app;
GRANT INSERT (
    payment_id, seq, prev_seq, id, kind, from_status, to_status, amount, prev_refundable, refundable
) ON payment_transitions TO payments_app;
