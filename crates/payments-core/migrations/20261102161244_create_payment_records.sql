-- Author: Nicola Antonello

-- The records of the steps that carry data (ADR-0017): limits that need other
-- rows, business times (ADR-0022), and the link to the ledger (ADR-0011).
--
-- A record's primary key is its step's ID, and a foreign key on
-- (transition_id, payment_id, kind, amount) ties it to a step of the right kind
-- with the same amount. kind is generated, holding the table's one kind, so no
-- caller writes it. Records carry no recorded_at: they commit with their step,
-- whose recorded_at is the same instant (ADR-0012).

CREATE TABLE payment_authorizations (
    transition_id  uuid        NOT NULL,
    payment_id     uuid        NOT NULL,
    kind           text        NOT NULL GENERATED ALWAYS AS ('authorize') STORED,
    amount         bigint      NOT NULL,
    -- The requested amount: the foreign key below proves the copy true, so the
    -- CHECK can compare with it.
    payment_amount bigint      NOT NULL,
    -- The deadline the PSP reported, never enforced by a clock here (ADR-0016).
    expires_at     timestamptz NOT NULL,
    CONSTRAINT payment_authorizations_pkey PRIMARY KEY (transition_id),
    CONSTRAINT payment_authorizations_step_fkey
        FOREIGN KEY (transition_id, payment_id, kind, amount)
        REFERENCES payment_transitions (id, payment_id, kind, amount),
    CONSTRAINT payment_authorizations_payment_fkey
        FOREIGN KEY (payment_id, payment_amount) REFERENCES payments (id, amount),
    CONSTRAINT payment_authorizations_amount_check CHECK (amount <= payment_amount),
    -- The target of the captures' foreign key, which carries the authorized
    -- amount across to check it.
    CONSTRAINT payment_authorizations_payment_id_amount_key UNIQUE (payment_id, amount)
);

CREATE TABLE payment_captures (
    transition_id         uuid        NOT NULL,
    payment_id            uuid        NOT NULL,
    kind                  text        NOT NULL GENERATED ALWAYS AS ('capture') STORED,
    amount                bigint      NOT NULL,
    -- Proved true by the foreign key below, like the requested amount above.
    authorized_amount     bigint      NOT NULL,
    -- The platform's fee, computed once in Rust (ADR-0020).
    fee                   bigint      NOT NULL,
    captured_at           timestamptz NOT NULL,
    ledger_transaction_id uuid        NOT NULL,
    CONSTRAINT payment_captures_pkey PRIMARY KEY (transition_id),
    CONSTRAINT payment_captures_step_fkey
        FOREIGN KEY (transition_id, payment_id, kind, amount)
        REFERENCES payment_transitions (id, payment_id, kind, amount),
    CONSTRAINT payment_captures_authorization_fkey
        FOREIGN KEY (payment_id, authorized_amount)
        REFERENCES payment_authorizations (payment_id, amount),
    CONSTRAINT payment_captures_amount_check CHECK (amount <= authorized_amount),
    CONSTRAINT payment_captures_fee_check CHECK (fee BETWEEN 0 AND amount),
    -- No key makes one capture per payment permanent, so that a later phase can
    -- add recapture without superseding this one (ADR-0016).
    CONSTRAINT payment_captures_ledger_transaction_id_key UNIQUE (ledger_transaction_id),
    CONSTRAINT payment_captures_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

CREATE TABLE payment_refunds (
    transition_id         uuid        NOT NULL,
    payment_id            uuid        NOT NULL,
    kind                  text        NOT NULL GENERATED ALWAYS AS ('refund') STORED,
    amount                bigint      NOT NULL,
    refunded_at           timestamptz NOT NULL,
    ledger_transaction_id uuid        NOT NULL,
    CONSTRAINT payment_refunds_pkey PRIMARY KEY (transition_id),
    CONSTRAINT payment_refunds_step_fkey
        FOREIGN KEY (transition_id, payment_id, kind, amount)
        REFERENCES payment_transitions (id, payment_id, kind, amount),
    CONSTRAINT payment_refunds_ledger_transaction_id_key UNIQUE (ledger_transaction_id),
    CONSTRAINT payment_refunds_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- A step of a kind that carries data has its record (LG004, ADR-0013). The
-- comparisons below have exact matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_payment_step_record() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_record boolean;
BEGIN
    IF NEW.kind = 'authorize' THEN
        has_record := EXISTS (SELECT 1 FROM payment_authorizations WHERE transition_id = NEW.id);
    ELSIF NEW.kind = 'capture' THEN
        has_record := EXISTS (SELECT 1 FROM payment_captures WHERE transition_id = NEW.id);
    ELSIF NEW.kind = 'refund' THEN
        has_record := EXISTS (SELECT 1 FROM payment_refunds WHERE transition_id = NEW.id);
    ELSE
        RETURN NULL;
    END IF;
    IF NOT has_record THEN
        RAISE SQLSTATE 'LG004' USING
            MESSAGE = format('payment step %s of kind %s has no record', NEW.id, NEW.kind),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT: the record's foreign key needs the step
-- to exist first, so the step cannot be checked when it is inserted.
CREATE CONSTRAINT TRIGGER payment_transitions_commit_check
    AFTER INSERT ON payment_transitions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_payment_step_record();

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER payment_authorizations_append_only
    BEFORE UPDATE OR DELETE ON payment_authorizations
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_authorizations_no_truncate
    BEFORE TRUNCATE ON payment_authorizations
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_captures_append_only
    BEFORE UPDATE OR DELETE ON payment_captures
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_captures_no_truncate
    BEFORE TRUNCATE ON payment_captures
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_refunds_append_only
    BEFORE UPDATE OR DELETE ON payment_refunds
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_refunds_no_truncate
    BEFORE TRUNCATE ON payment_refunds
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_transitions ENABLE ALWAYS TRIGGER payment_transitions_commit_check;
ALTER TABLE payment_authorizations ENABLE ALWAYS TRIGGER payment_authorizations_append_only;
ALTER TABLE payment_authorizations ENABLE ALWAYS TRIGGER payment_authorizations_no_truncate;
ALTER TABLE payment_captures ENABLE ALWAYS TRIGGER payment_captures_append_only;
ALTER TABLE payment_captures ENABLE ALWAYS TRIGGER payment_captures_no_truncate;
ALTER TABLE payment_refunds ENABLE ALWAYS TRIGGER payment_refunds_append_only;
ALTER TABLE payment_refunds ENABLE ALWAYS TRIGGER payment_refunds_no_truncate;

-- The runtime role reads the records and inserts every column except the
-- generated kind.
GRANT SELECT ON payment_authorizations, payment_captures, payment_refunds TO payments_app;
GRANT INSERT (transition_id, payment_id, amount, payment_amount, expires_at)
    ON payment_authorizations TO payments_app;
GRANT INSERT (
    transition_id, payment_id, amount, authorized_amount, fee, captured_at, ledger_transaction_id
) ON payment_captures TO payments_app;
GRANT INSERT (transition_id, payment_id, amount, refunded_at, ledger_transaction_id)
    ON payment_refunds TO payments_app;
