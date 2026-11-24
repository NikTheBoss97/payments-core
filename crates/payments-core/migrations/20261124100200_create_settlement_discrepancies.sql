-- Author: Nicola Antonello

-- Discrepancies (ADR-0068, ADR-0069, ADR-0072): one row for each, the subject that it is
-- about in a table of its kind, and a chain of steps, whose first, find, is written with
-- it. An overdue movement's chain is its movement's, so it has no steps of its own. At
-- COMMIT, LG012 refuses a discrepancy without its subject or its first step, and LG011
-- now also an overdue step without its subject. Resolving's tables add the outcomes and
-- the evidence later. Everything here is written only by the system role.

CREATE TABLE settlement_discrepancies (
    id          uuid        NOT NULL,
    kind        text        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT settlement_discrepancies_pkey PRIMARY KEY (id),
    CONSTRAINT settlement_discrepancies_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    -- A closed set of labels, parsed once into a Rust enum (ADR-0009).
    CONSTRAINT settlement_discrepancies_kind_check CHECK (kind IN (
        'unmatched_line', 'payout_difference', 'fee_difference', 'missing_batch',
        'overdue_movement', 'uncaused_posting'
    )),
    -- The target of the subjects' and the steps' foreign keys, which carry the kind.
    CONSTRAINT settlement_discrepancies_kind_key UNIQUE (id, kind)
);

-- A line that settles nothing of ours (ADR-0068), found as its batch is booked: its net
-- is in suspense (ADR-0067).
CREATE TABLE settlement_unmatched_lines (
    discrepancy_id uuid    NOT NULL,
    kind           text    NOT NULL GENERATED ALWAYS AS ('unmatched_line') STORED,
    batch_id       uuid    NOT NULL,
    line_no        integer NOT NULL,
    net            bigint  NOT NULL,
    xact_id        xid8    NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_unmatched_lines_pkey PRIMARY KEY (discrepancy_id),
    CONSTRAINT settlement_unmatched_lines_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, kind) REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_unmatched_lines_line_fkey
        FOREIGN KEY (batch_id, line_no, net) REFERENCES settlement_lines (batch_id, line_no, net),
    CONSTRAINT settlement_unmatched_lines_line_key UNIQUE (batch_id, line_no),
    CONSTRAINT settlement_unmatched_lines_batch_fkey
        FOREIGN KEY (batch_id, xact_id) REFERENCES settlement_batches (id, xact_id)
);

-- A payout that differs from the sum of its batch's nets, found as the batch is booked:
-- the difference is in suspense.
CREATE TABLE settlement_payout_differences (
    discrepancy_id uuid   NOT NULL,
    kind           text   NOT NULL GENERATED ALWAYS AS ('payout_difference') STORED,
    batch_id       uuid   NOT NULL,
    -- The payout less the sum of the nets.
    amount         bigint NOT NULL,
    xact_id        xid8   NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_payout_differences_pkey PRIMARY KEY (discrepancy_id),
    CONSTRAINT settlement_payout_differences_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, kind) REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_payout_differences_batch_key UNIQUE (batch_id),
    CONSTRAINT settlement_payout_differences_batch_fkey
        FOREIGN KEY (batch_id, xact_id) REFERENCES settlement_batches (id, xact_id),
    CONSTRAINT settlement_payout_differences_amount_check
        CHECK (amount BETWEEN -1000000000000000 AND 1000000000000000 AND amount <> 0)
);

-- A fee off the agreed terms on a line that matches (ADR-0068), found at booking or at
-- the line's late match.
CREATE TABLE settlement_fee_differences (
    discrepancy_id uuid    NOT NULL,
    kind           text    NOT NULL GENERATED ALWAYS AS ('fee_difference') STORED,
    batch_id       uuid    NOT NULL,
    line_no        integer NOT NULL,
    -- The fee that the PSP charged, carried from the line, and the one that the terms give.
    charged        bigint  NOT NULL,
    agreed         bigint  NOT NULL,
    CONSTRAINT settlement_fee_differences_pkey PRIMARY KEY (discrepancy_id),
    CONSTRAINT settlement_fee_differences_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, kind) REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_fee_differences_line_fkey
        FOREIGN KEY (batch_id, line_no, charged) REFERENCES settlement_lines (batch_id, line_no, fee),
    CONSTRAINT settlement_fee_differences_line_key UNIQUE (batch_id, line_no),
    CONSTRAINT settlement_fee_differences_agreed_check
        CHECK (agreed BETWEEN 0 AND 1000000000000000),
    CONSTRAINT settlement_fee_differences_differs_check CHECK (charged <> agreed)
);

-- A batch that the listing skips, or whose file cannot be read (ADR-0066, ADR-0070).
CREATE TABLE settlement_missing_batches (
    discrepancy_id uuid          NOT NULL,
    kind           text          NOT NULL GENERATED ALWAYS AS ('missing_batch') STORED,
    psp            text          NOT NULL,
    currency       currency_code NOT NULL,
    number         bigint        NOT NULL,
    cause          text          NOT NULL,
    CONSTRAINT settlement_missing_batches_pkey PRIMARY KEY (discrepancy_id),
    CONSTRAINT settlement_missing_batches_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, kind) REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_missing_batches_psp_check CHECK (psp IN ('mock')),
    CONSTRAINT settlement_missing_batches_currency_fkey
        FOREIGN KEY (currency) REFERENCES currencies (code),
    CONSTRAINT settlement_missing_batches_number_check CHECK (number >= 1),
    CONSTRAINT settlement_missing_batches_cause_check
        CHECK (cause IN ('not_listed', 'unreadable')),
    CONSTRAINT settlement_missing_batches_number_key UNIQUE (psp, currency, number)
);

-- A movement found overdue (ADR-0068, ADR-0070): the record of its overdue step, whose
-- chain is this discrepancy's (ADR-0069).
CREATE TABLE settlement_overdue_movements (
    discrepancy_id uuid        NOT NULL,
    kind           text        NOT NULL GENERATED ALWAYS AS ('overdue_movement') STORED,
    payment_id     uuid        NOT NULL,
    position       integer     NOT NULL,
    step_id        uuid        NOT NULL,
    step_kind      text        NOT NULL GENERATED ALWAYS AS ('overdue') STORED,
    -- The time as of which the movement was judged overdue (ADR-0070): a business time.
    as_of          timestamptz NOT NULL,
    CONSTRAINT settlement_overdue_movements_pkey PRIMARY KEY (discrepancy_id),
    CONSTRAINT settlement_overdue_movements_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, kind) REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_overdue_movements_step_fkey
        FOREIGN KEY (step_id, payment_id, position, step_kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_overdue_movements_movement_key UNIQUE (payment_id, position),
    CONSTRAINT settlement_overdue_movements_step_id_key UNIQUE (step_id),
    CONSTRAINT settlement_overdue_movements_as_of_check
        CHECK (as_of BETWEEN '0001-01-01 00:00:00+00 BC' AND '9999-12-31 23:59:59.999999+00')
);

-- A ledger transaction that no record causes (ADR-0021, ADR-0070). This row records it
-- and is not its cause.
CREATE TABLE settlement_uncaused_postings (
    discrepancy_id        uuid NOT NULL,
    kind                  text NOT NULL GENERATED ALWAYS AS ('uncaused_posting') STORED,
    ledger_transaction_id uuid NOT NULL,
    CONSTRAINT settlement_uncaused_postings_pkey PRIMARY KEY (discrepancy_id),
    CONSTRAINT settlement_uncaused_postings_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, kind) REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_uncaused_postings_ledger_transaction_id_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_uncaused_postings_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- A discrepancy's chain (ADR-0069), as a movement's: the first step, find, written with
-- the discrepancy, points to itself, and the edge check carries the discrepancy's kind.
CREATE TABLE settlement_discrepancy_steps (
    discrepancy_id   uuid        NOT NULL,
    seq              integer     NOT NULL,
    prev_seq         integer     NOT NULL,
    id               uuid        NOT NULL,
    kind             text        NOT NULL,
    from_state       text        NOT NULL,
    to_state         text        NOT NULL,
    discrepancy_kind text        NOT NULL,
    recorded_at      timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT settlement_discrepancy_steps_pkey PRIMARY KEY (discrepancy_id, seq),
    CONSTRAINT settlement_discrepancy_steps_id_key UNIQUE (id),
    CONSTRAINT settlement_discrepancy_steps_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT settlement_discrepancy_steps_discrepancy_fkey
        FOREIGN KEY (discrepancy_id, discrepancy_kind)
        REFERENCES settlement_discrepancies (id, kind),
    CONSTRAINT settlement_discrepancy_steps_state_key UNIQUE (discrepancy_id, seq, to_state),
    CONSTRAINT settlement_discrepancy_steps_prev_fkey
        FOREIGN KEY (discrepancy_id, prev_seq, from_state)
        REFERENCES settlement_discrepancy_steps (discrepancy_id, seq, to_state),
    -- The target of the records' foreign keys.
    CONSTRAINT settlement_discrepancy_steps_step_key UNIQUE (id, discrepancy_id, kind),
    CONSTRAINT settlement_discrepancy_steps_seq_check
        CHECK ((seq = 0 AND prev_seq = 0) OR (seq >= 1 AND prev_seq = seq - 1)),
    -- The first steps; resolving's tables add the outcomes and the evidence under this
    -- name. An overdue movement has no steps here: its chain is its movement's.
    CONSTRAINT settlement_discrepancy_steps_edge_check CHECK (
        (kind = 'find') = (seq = 0)
        AND (discrepancy_kind, from_state, kind, to_state) IN (
            ('unmatched_line', 'open', 'find', 'open'),
            ('payout_difference', 'open', 'find', 'open'),
            ('fee_difference', 'open', 'find', 'open'),
            ('missing_batch', 'open', 'find', 'open'),
            ('uncaused_posting', 'open', 'find', 'open')
        )
    )
);

-- A movement can now go overdue, and settle after that (ADR-0069).
ALTER TABLE settlement_movement_steps
    DROP CONSTRAINT settlement_movement_steps_edge_check,
    ADD CONSTRAINT settlement_movement_steps_edge_check CHECK (
        (kind = 'open') = (seq = 0)
        AND (from_state, kind, to_state) IN (
            ('open', 'open', 'open'),
            ('open', 'settle', 'settled'),
            ('open', 'overdue', 'overdue'),
            ('overdue', 'settle', 'settled')
        )
    );

-- A movement's step that needs a record has one (LG011, ADR-0069): a settle step its
-- match, and now an overdue step its subject. The comparisons have exact matches in
-- pg_catalog (ADR-0004).
CREATE OR REPLACE FUNCTION check_settlement_movement_step_record() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_record boolean;
BEGIN
    IF NEW.kind = 'settle' THEN
        has_record := EXISTS (SELECT 1 FROM settlement_capture_matches WHERE step_id = NEW.id)
                      OR EXISTS (SELECT 1 FROM settlement_refund_matches WHERE step_id = NEW.id);
    ELSIF NEW.kind = 'overdue' THEN
        has_record := EXISTS (SELECT 1 FROM settlement_overdue_movements WHERE step_id = NEW.id);
    ELSE
        RETURN NULL;
    END IF;
    IF NOT has_record THEN
        RAISE SQLSTATE 'LG011' USING
            MESSAGE = format('movement step %s of kind %s has no record', NEW.id, NEW.kind),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- A discrepancy has its subject, and, unless its chain is its movement's, its first step
-- (LG012, ADR-0072), as a payment has its first step (LG005). The comparisons have exact
-- matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_discrepancy() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_subject    boolean;
    has_first_step boolean;
BEGIN
    has_subject := CASE NEW.kind
        WHEN 'unmatched_line' THEN
            EXISTS (SELECT 1 FROM settlement_unmatched_lines WHERE discrepancy_id = NEW.id)
        WHEN 'payout_difference' THEN
            EXISTS (SELECT 1 FROM settlement_payout_differences WHERE discrepancy_id = NEW.id)
        WHEN 'fee_difference' THEN
            EXISTS (SELECT 1 FROM settlement_fee_differences WHERE discrepancy_id = NEW.id)
        WHEN 'missing_batch' THEN
            EXISTS (SELECT 1 FROM settlement_missing_batches WHERE discrepancy_id = NEW.id)
        WHEN 'overdue_movement' THEN
            EXISTS (SELECT 1 FROM settlement_overdue_movements WHERE discrepancy_id = NEW.id)
        WHEN 'uncaused_posting' THEN
            EXISTS (SELECT 1 FROM settlement_uncaused_postings WHERE discrepancy_id = NEW.id)
        ELSE false
    END;
    has_first_step := NEW.kind = 'overdue_movement'
        OR EXISTS (
            SELECT 1 FROM settlement_discrepancy_steps WHERE discrepancy_id = NEW.id AND seq = 0
        );
    IF NOT has_subject OR NOT has_first_step THEN
        RAISE SQLSTATE 'LG012' USING
            MESSAGE = format('discrepancy %s of kind %s is not whole', NEW.id, NEW.kind),
            DETAIL = format('Subject: %s, first step: %s.', has_subject, has_first_step),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT: the subject's and the first step's foreign keys
-- need the discrepancy to exist first.
CREATE CONSTRAINT TRIGGER settlement_discrepancies_commit_check
    AFTER INSERT ON settlement_discrepancies
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_discrepancy();

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER settlement_discrepancies_append_only
    BEFORE UPDATE OR DELETE ON settlement_discrepancies
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancies_no_truncate
    BEFORE TRUNCATE ON settlement_discrepancies
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_unmatched_lines_append_only
    BEFORE UPDATE OR DELETE ON settlement_unmatched_lines
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_unmatched_lines_no_truncate
    BEFORE TRUNCATE ON settlement_unmatched_lines
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_payout_differences_append_only
    BEFORE UPDATE OR DELETE ON settlement_payout_differences
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_payout_differences_no_truncate
    BEFORE TRUNCATE ON settlement_payout_differences
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_fee_differences_append_only
    BEFORE UPDATE OR DELETE ON settlement_fee_differences
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_fee_differences_no_truncate
    BEFORE TRUNCATE ON settlement_fee_differences
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_missing_batches_append_only
    BEFORE UPDATE OR DELETE ON settlement_missing_batches
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_missing_batches_no_truncate
    BEFORE TRUNCATE ON settlement_missing_batches
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_overdue_movements_append_only
    BEFORE UPDATE OR DELETE ON settlement_overdue_movements
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_overdue_movements_no_truncate
    BEFORE TRUNCATE ON settlement_overdue_movements
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_uncaused_postings_append_only
    BEFORE UPDATE OR DELETE ON settlement_uncaused_postings
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_uncaused_postings_no_truncate
    BEFORE TRUNCATE ON settlement_uncaused_postings
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_steps_append_only
    BEFORE UPDATE OR DELETE ON settlement_discrepancy_steps
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_steps_no_truncate
    BEFORE TRUNCATE ON settlement_discrepancy_steps
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_discrepancies ENABLE ALWAYS TRIGGER settlement_discrepancies_commit_check;
ALTER TABLE settlement_discrepancies ENABLE ALWAYS TRIGGER settlement_discrepancies_append_only;
ALTER TABLE settlement_discrepancies ENABLE ALWAYS TRIGGER settlement_discrepancies_no_truncate;
ALTER TABLE settlement_unmatched_lines
    ENABLE ALWAYS TRIGGER settlement_unmatched_lines_append_only;
ALTER TABLE settlement_unmatched_lines
    ENABLE ALWAYS TRIGGER settlement_unmatched_lines_no_truncate;
ALTER TABLE settlement_payout_differences
    ENABLE ALWAYS TRIGGER settlement_payout_differences_append_only;
ALTER TABLE settlement_payout_differences
    ENABLE ALWAYS TRIGGER settlement_payout_differences_no_truncate;
ALTER TABLE settlement_fee_differences
    ENABLE ALWAYS TRIGGER settlement_fee_differences_append_only;
ALTER TABLE settlement_fee_differences
    ENABLE ALWAYS TRIGGER settlement_fee_differences_no_truncate;
ALTER TABLE settlement_missing_batches
    ENABLE ALWAYS TRIGGER settlement_missing_batches_append_only;
ALTER TABLE settlement_missing_batches
    ENABLE ALWAYS TRIGGER settlement_missing_batches_no_truncate;
ALTER TABLE settlement_overdue_movements
    ENABLE ALWAYS TRIGGER settlement_overdue_movements_append_only;
ALTER TABLE settlement_overdue_movements
    ENABLE ALWAYS TRIGGER settlement_overdue_movements_no_truncate;
ALTER TABLE settlement_uncaused_postings
    ENABLE ALWAYS TRIGGER settlement_uncaused_postings_append_only;
ALTER TABLE settlement_uncaused_postings
    ENABLE ALWAYS TRIGGER settlement_uncaused_postings_no_truncate;
ALTER TABLE settlement_discrepancy_steps
    ENABLE ALWAYS TRIGGER settlement_discrepancy_steps_append_only;
ALTER TABLE settlement_discrepancy_steps
    ENABLE ALWAYS TRIGGER settlement_discrepancy_steps_no_truncate;

-- The system role reads them all and inserts every column except xact_id and
-- recorded_at, which only their defaults fill, and the generated ones (ADR-0012,
-- ADR-0055). The merchant's role has no privilege on any, so none is under row-level
-- security.
GRANT SELECT ON settlement_discrepancies, settlement_unmatched_lines,
    settlement_payout_differences, settlement_fee_differences, settlement_missing_batches,
    settlement_overdue_movements, settlement_uncaused_postings, settlement_discrepancy_steps
    TO payments_app;
GRANT INSERT (id, kind) ON settlement_discrepancies TO payments_app;
GRANT INSERT (discrepancy_id, batch_id, line_no, net)
    ON settlement_unmatched_lines TO payments_app;
GRANT INSERT (discrepancy_id, batch_id, amount) ON settlement_payout_differences TO payments_app;
GRANT INSERT (discrepancy_id, batch_id, line_no, charged, agreed)
    ON settlement_fee_differences TO payments_app;
GRANT INSERT (discrepancy_id, psp, currency, number, cause)
    ON settlement_missing_batches TO payments_app;
GRANT INSERT (discrepancy_id, payment_id, position, step_id, as_of)
    ON settlement_overdue_movements TO payments_app;
GRANT INSERT (discrepancy_id, ledger_transaction_id)
    ON settlement_uncaused_postings TO payments_app;
GRANT INSERT (
    discrepancy_id, seq, prev_seq, id, kind, from_state, to_state, discrepancy_kind
) ON settlement_discrepancy_steps TO payments_app;
