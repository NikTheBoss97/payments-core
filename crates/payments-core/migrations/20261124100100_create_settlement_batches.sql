-- Author: Nicola Antonello

-- Settlement batches, their lines and the lines' matches, and the chains of the movements
-- that they settle (ADR-0066 to ADR-0069, ADR-0072). Booking writes a batch, its lines, their
-- matches, the steps of the movements that they settle and the batch's posting in one
-- transaction. At COMMIT, LG009 checks the posting, and LG011 that each step that needs a
-- record has one. Everything here is written only by the system role.

-- The targets of the matches' foreign keys, each unique already through a narrower key.
ALTER TABLE payment_facts
    ADD CONSTRAINT payment_facts_line_key
    UNIQUE (psp, psp_fact_id, payment_id, position, kind, amount);
ALTER TABLE payment_request_decisions
    ADD CONSTRAINT payment_request_decisions_line_key UNIQUE (request_id, payment_id, position);
ALTER TABLE payments ADD CONSTRAINT payments_id_currency_key UNIQUE (id, currency);

-- One settlement run of a PSP in one currency (ADR-0066).
CREATE TABLE settlement_batches (
    id          uuid          NOT NULL,
    psp         text          NOT NULL,
    currency    currency_code NOT NULL,
    -- The batch's place in its PSP's sequence for its currency, from 1.
    number      bigint        NOT NULL,
    -- When the PSP settled it, from the file: a business time (ADR-0037, ADR-0059).
    settled_at  timestamptz   NOT NULL,
    -- The payout's line, as the PSP reports it, which may differ from the sum of the nets.
    payout      bigint        NOT NULL,
    -- The ID of the inserting transaction, which every row of the batch repeats, so that
    -- none joins it later and escapes LG009 (ADR-0072). Only this default fills it.
    xact_id     xid8          NOT NULL DEFAULT pg_current_xact_id(),
    recorded_at timestamptz   NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT settlement_batches_pkey PRIMARY KEY (id),
    CONSTRAINT settlement_batches_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT settlement_batches_psp_check CHECK (psp IN ('mock')),
    -- A batch is written about once a day per currency, so the key's lock on the
    -- currency's row costs nothing.
    CONSTRAINT settlement_batches_currency_fkey FOREIGN KEY (currency) REFERENCES currencies (code),
    CONSTRAINT settlement_batches_number_check CHECK (number >= 1),
    CONSTRAINT settlement_batches_settled_at_check
        CHECK (settled_at BETWEEN '0001-01-01 00:00:00+00 BC' AND '9999-12-31 23:59:59.999999+00'),
    CONSTRAINT settlement_batches_payout_check
        CHECK (payout BETWEEN -1000000000000000 AND 1000000000000000),
    -- Booked once (ADR-0070).
    CONSTRAINT settlement_batches_number_key UNIQUE (psp, currency, number),
    -- The targets of its rows' foreign keys, which carry its transaction, its PSP and its
    -- currency across.
    CONSTRAINT settlement_batches_id_xact_id_key UNIQUE (id, xact_id),
    CONSTRAINT settlement_batches_id_psp_key UNIQUE (id, psp),
    CONSTRAINT settlement_batches_id_currency_key UNIQUE (id, currency)
);

-- A batch's posting, when an entry remains (ADR-0067). It commits with its batch.
CREATE TABLE settlement_batch_postings (
    batch_id              uuid NOT NULL,
    ledger_transaction_id uuid NOT NULL,
    xact_id               xid8 NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_batch_postings_pkey PRIMARY KEY (batch_id),
    -- Its batch, written by the same transaction.
    CONSTRAINT settlement_batch_postings_batch_fkey
        FOREIGN KEY (batch_id, xact_id) REFERENCES settlement_batches (id, xact_id),
    CONSTRAINT settlement_batch_postings_ledger_transaction_id_key UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_batch_postings_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- A line of a batch's file, but the payout's, which the batch holds (ADR-0066). It
-- commits with its batch.
CREATE TABLE settlement_lines (
    batch_id uuid    NOT NULL,
    -- The line's number in the file, the header being line 1, so that an operator finds
    -- it there.
    line_no  integer NOT NULL,
    kind     text    NOT NULL,
    gross    bigint  NOT NULL,
    fee      bigint  NOT NULL,
    net      bigint  NOT NULL,
    xact_id  xid8    NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_lines_pkey PRIMARY KEY (batch_id, line_no),
    CONSTRAINT settlement_lines_batch_fkey
        FOREIGN KEY (batch_id, xact_id) REFERENCES settlement_batches (id, xact_id),
    -- PostgreSQL runs a table's CHECKs in the order of their names, so the bounds are
    -- checked before the net's rule computes with the amounts, and a kind outside the
    -- set fails its own check.
    CONSTRAINT settlement_lines_amounts_check CHECK (
        gross BETWEEN -1000000000000000 AND 1000000000000000
        AND fee BETWEEN -1000000000000000 AND 1000000000000000
        AND net BETWEEN -1000000000000000 AND 1000000000000000
    ),
    CONSTRAINT settlement_lines_gross_check CHECK (
        CASE kind
            WHEN 'capture' THEN gross >= 1
            WHEN 'refund' THEN gross <= -1
            ELSE true
        END
    ),
    CONSTRAINT settlement_lines_kind_check CHECK (kind IN ('capture', 'refund')),
    CONSTRAINT settlement_lines_line_no_check CHECK (line_no >= 2),
    CONSTRAINT settlement_lines_net_check CHECK (net = gross - fee),
    -- The targets of the foreign keys that carry a line's kind and gross to its match, its
    -- fee to a fee's difference and its net to the discrepancy of a line that settles
    -- nothing of ours.
    CONSTRAINT settlement_lines_kind_gross_key UNIQUE (batch_id, line_no, kind, gross),
    CONSTRAINT settlement_lines_fee_key UNIQUE (batch_id, line_no, fee),
    CONSTRAINT settlement_lines_net_key UNIQUE (batch_id, line_no, net)
);

-- A movement's chain (ADR-0069): a capture or refund that the catch-up applied, named by
-- its fact, settles once, by a batch's line or late, or goes overdue first. Like a
-- payment's history (ADR-0017), the newest step is the state, each step's key to its
-- predecessor carries the predecessor's state, and the first step, open, points to
-- itself. The first writer that moves a movement writes its first step.
CREATE TABLE settlement_movement_steps (
    payment_id  uuid        NOT NULL,
    position    integer     NOT NULL,
    -- 0 for the first step, then one more than prev_seq.
    seq         integer     NOT NULL,
    prev_seq    integer     NOT NULL,
    -- The step's ID, the target of its record's key.
    id          uuid        NOT NULL,
    kind        text        NOT NULL,
    from_state  text        NOT NULL,
    to_state    text        NOT NULL,
    -- The movement's fact's kind and amount, carried across by the fact's key.
    fact_kind   text        NOT NULL,
    amount      bigint      NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    -- Only one row can follow any state.
    CONSTRAINT settlement_movement_steps_pkey PRIMARY KEY (payment_id, position, seq),
    CONSTRAINT settlement_movement_steps_id_key UNIQUE (id),
    CONSTRAINT settlement_movement_steps_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT settlement_movement_steps_fact_fkey
        FOREIGN KEY (payment_id, position, fact_kind, amount)
        REFERENCES payment_facts (payment_id, position, kind, amount),
    CONSTRAINT settlement_movement_steps_fact_kind_check
        CHECK (fact_kind IN ('capture', 'refund')),
    CONSTRAINT settlement_movement_steps_state_key UNIQUE (payment_id, position, seq, to_state),
    CONSTRAINT settlement_movement_steps_prev_fkey
        FOREIGN KEY (payment_id, position, prev_seq, from_state)
        REFERENCES settlement_movement_steps (payment_id, position, seq, to_state),
    -- The target of the records' foreign keys, which tie a record to a step of the right
    -- kind of the right movement.
    CONSTRAINT settlement_movement_steps_step_key UNIQUE (id, payment_id, position, kind),
    CONSTRAINT settlement_movement_steps_seq_check
        CHECK ((seq = 0 AND prev_seq = 0) OR (seq >= 1 AND prev_seq = seq - 1)),
    -- The moves that booking needs; the next migrations add the others under this name.
    -- An illegal move fails here first, since the CHECKs run in the order of their names.
    CONSTRAINT settlement_movement_steps_edge_check CHECK (
        (kind = 'open') = (seq = 0)
        AND (from_state, kind, to_state) IN (
            ('open', 'open', 'open'),
            ('open', 'settle', 'settled')
        )
    )
);

-- A capture line that settles a capture of ours (ADR-0068): each rule is a foreign key of
-- its own, so that a test can tell which one refused. It commits with its batch.
CREATE TABLE settlement_capture_matches (
    batch_id         uuid          NOT NULL,
    line_no          integer       NOT NULL,
    kind             text          NOT NULL GENERATED ALWAYS AS ('capture') STORED,
    gross            bigint        NOT NULL,
    psp              text          NOT NULL,
    currency         currency_code NOT NULL,
    payment_id       uuid          NOT NULL,
    position         integer       NOT NULL,
    psp_fact_id      text          NOT NULL,
    request_id       uuid          NOT NULL,
    -- The position of the payment's settled fact (ADR-0068).
    settled_position integer       NOT NULL,
    settled_kind     text          NOT NULL GENERATED ALWAYS AS ('settle') STORED,
    -- The movement's settle step, which this match is the record of (ADR-0069).
    step_id          uuid          NOT NULL,
    step_kind        text          NOT NULL GENERATED ALWAYS AS ('settle') STORED,
    xact_id          xid8          NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_capture_matches_pkey PRIMARY KEY (batch_id, line_no),
    CONSTRAINT settlement_capture_matches_line_fkey
        FOREIGN KEY (batch_id, line_no, kind, gross)
        REFERENCES settlement_lines (batch_id, line_no, kind, gross),
    CONSTRAINT settlement_capture_matches_batch_fkey
        FOREIGN KEY (batch_id, xact_id) REFERENCES settlement_batches (id, xact_id),
    CONSTRAINT settlement_capture_matches_psp_fkey
        FOREIGN KEY (batch_id, psp) REFERENCES settlement_batches (id, psp),
    CONSTRAINT settlement_capture_matches_currency_fkey
        FOREIGN KEY (batch_id, currency) REFERENCES settlement_batches (id, currency),
    CONSTRAINT settlement_capture_matches_fact_fkey
        FOREIGN KEY (psp, psp_fact_id, payment_id, position, kind, gross)
        REFERENCES payment_facts (psp, psp_fact_id, payment_id, position, kind, amount),
    -- A fact settles once.
    CONSTRAINT settlement_capture_matches_fact_key UNIQUE (psp, psp_fact_id),
    CONSTRAINT settlement_capture_matches_settled_fkey
        FOREIGN KEY (payment_id, settled_position, settled_kind)
        REFERENCES payment_facts (payment_id, position, kind),
    -- A settled fact backs one line.
    CONSTRAINT settlement_capture_matches_settled_key UNIQUE (payment_id, settled_position),
    CONSTRAINT settlement_capture_matches_decision_fkey
        FOREIGN KEY (request_id, payment_id, position)
        REFERENCES payment_request_decisions (request_id, payment_id, position),
    CONSTRAINT settlement_capture_matches_payment_currency_fkey
        FOREIGN KEY (payment_id, currency) REFERENCES payments (id, currency),
    CONSTRAINT settlement_capture_matches_step_fkey
        FOREIGN KEY (step_id, payment_id, position, step_kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_capture_matches_step_id_key UNIQUE (step_id)
);

-- A refund line that settles a refund of ours, as a capture's match without a settled
-- fact. Its gross is negative, and the fact's amount positive, so a generated column
-- carries the gross negated to the fact's key. It commits with its batch.
CREATE TABLE settlement_refund_matches (
    batch_id    uuid          NOT NULL,
    line_no     integer       NOT NULL,
    kind        text          NOT NULL GENERATED ALWAYS AS ('refund') STORED,
    gross       bigint        NOT NULL,
    amount      bigint        NOT NULL GENERATED ALWAYS AS (-gross) STORED,
    psp         text          NOT NULL,
    currency    currency_code NOT NULL,
    payment_id  uuid          NOT NULL,
    position    integer       NOT NULL,
    psp_fact_id text          NOT NULL,
    request_id  uuid          NOT NULL,
    step_id     uuid          NOT NULL,
    step_kind   text          NOT NULL GENERATED ALWAYS AS ('settle') STORED,
    xact_id     xid8          NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_refund_matches_pkey PRIMARY KEY (batch_id, line_no),
    CONSTRAINT settlement_refund_matches_line_fkey
        FOREIGN KEY (batch_id, line_no, kind, gross)
        REFERENCES settlement_lines (batch_id, line_no, kind, gross),
    CONSTRAINT settlement_refund_matches_batch_fkey
        FOREIGN KEY (batch_id, xact_id) REFERENCES settlement_batches (id, xact_id),
    CONSTRAINT settlement_refund_matches_psp_fkey
        FOREIGN KEY (batch_id, psp) REFERENCES settlement_batches (id, psp),
    CONSTRAINT settlement_refund_matches_currency_fkey
        FOREIGN KEY (batch_id, currency) REFERENCES settlement_batches (id, currency),
    CONSTRAINT settlement_refund_matches_fact_fkey
        FOREIGN KEY (psp, psp_fact_id, payment_id, position, kind, amount)
        REFERENCES payment_facts (psp, psp_fact_id, payment_id, position, kind, amount),
    CONSTRAINT settlement_refund_matches_fact_key UNIQUE (psp, psp_fact_id),
    CONSTRAINT settlement_refund_matches_decision_fkey
        FOREIGN KEY (request_id, payment_id, position)
        REFERENCES payment_request_decisions (request_id, payment_id, position),
    CONSTRAINT settlement_refund_matches_payment_currency_fkey
        FOREIGN KEY (payment_id, currency) REFERENCES payments (id, currency),
    CONSTRAINT settlement_refund_matches_step_fkey
        FOREIGN KEY (step_id, payment_id, position, step_kind)
        REFERENCES settlement_movement_steps (id, payment_id, position, kind),
    CONSTRAINT settlement_refund_matches_step_id_key UNIQUE (step_id)
);

-- A batch's posting is exactly the entries that its lines and payout give (LG009,
-- ADR-0067): psp_payouts debited with the payout, psp_fees with the matched lines' fees,
-- psp_receivable credited with their gross, and settlement_suspense with the nets of the
-- lines that settle nothing of ours plus the payout less all the nets, an entry of zero
-- left out. A batch whose entries are all zero has no posting. The posting's header was
-- written by this transaction, in the batch's currency. This repeats ADR-0067's rule on
-- purpose, as LG006 repeats a capture's (ADR-0021). Sums are numeric, and every
-- comparison names its types, so each has an exact operator in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_batch_posting() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    fees             numeric;
    matched_gross    numeric;
    unmatched_nets   numeric;
    all_nets         numeric;
    suspense         numeric;
    posting          uuid;
    written_here     boolean;
    posting_currency text;
    remaining        bigint;
    differing        bigint;
BEGIN
    -- A line counts as matched when a match names it (ADR-0072).
    SELECT COALESCE(sum(l.fee) FILTER (WHERE m.matched), 0::numeric),
           COALESCE(sum(l.gross) FILTER (WHERE m.matched), 0::numeric),
           COALESCE(sum(l.net) FILTER (WHERE NOT m.matched), 0::numeric),
           COALESCE(sum(l.net), 0::numeric)
      INTO fees, matched_gross, unmatched_nets, all_nets
      FROM settlement_lines l
     CROSS JOIN LATERAL (
           SELECT EXISTS (
                      SELECT 1 FROM settlement_capture_matches c
                       WHERE c.batch_id = l.batch_id AND c.line_no = l.line_no
                  )
                  OR EXISTS (
                      SELECT 1 FROM settlement_refund_matches r
                       WHERE r.batch_id = l.batch_id AND r.line_no = l.line_no
                  ) AS matched
           ) m
     WHERE l.batch_id = NEW.id;
    suspense := -(unmatched_nets + NEW.payout::numeric - all_nets);

    -- With no link, posting stays NULL, and so do the two that depend on it.
    SELECT p.ledger_transaction_id INTO posting
      FROM settlement_batch_postings p
     WHERE p.batch_id = NEW.id;
    -- A row written inside a savepoint carries the savepoint's own transaction ID, so a
    -- posting written there fails this test, as LG006's does (ADR-0021).
    SELECT t.xmin = pg_current_xact_id()::xid, t.currency::text
      INTO written_here, posting_currency
      FROM ledger_transactions t
     WHERE t.id = posting;

    -- Set operations compare with each type's default equality, not through the search
    -- path. EXCEPT ALL in both directions finds a missing entry, an extra one, a changed
    -- amount and a wrong account.
    WITH expected (kind, party, amount) AS (
        SELECT line.kind, line.party, line.amount
          FROM (VALUES
                    ('psp_payouts', NEW.psp, NEW.payout::numeric),
                    ('psp_fees', NEW.psp, fees),
                    ('psp_receivable', NEW.psp, -matched_gross),
                    ('settlement_suspense', NEW.psp, suspense)
               ) AS line (kind, party, amount)
         WHERE line.amount <> 0::numeric
    ), actual (kind, party, amount) AS (
        SELECT a.kind, a.party, e.amount::numeric
          FROM ledger_entries e
          JOIN accounts a ON a.id = e.account_id
         WHERE e.transaction_id = posting
    )
    SELECT (SELECT count(*) FROM expected),
           (SELECT count(*)
              FROM (
                  (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual)
                  UNION ALL
                  (SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
              ) AS difference)
      INTO remaining, differing;

    IF (remaining = 0::bigint) <> (posting IS NULL)
       OR (posting IS NOT NULL
           AND (NOT written_here
                OR posting_currency <> NEW.currency::text
                OR differing <> 0::bigint)) THEN
        RAISE SQLSTATE 'LG009' USING
            MESSAGE = format('the posting of settlement batch %s is not exactly its lines', NEW.id),
            DETAIL = format(
                'Posting %s: written by this transaction: %s, currency %s against %s, '
                '%s entries expected, %s differ.',
                posting, written_here, posting_currency, NEW.currency, remaining, differing
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT, after every line, match and link of the batch.
CREATE CONSTRAINT TRIGGER settlement_batches_commit_check
    AFTER INSERT ON settlement_batches
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_batch_posting();

-- A movement's step that needs a record has one (LG011, ADR-0069): a settle step its
-- match. The next migrations add the other kinds. The comparisons have exact matches in
-- pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_movement_step_record() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_record boolean;
BEGIN
    IF NEW.kind = 'settle' THEN
        has_record := EXISTS (SELECT 1 FROM settlement_capture_matches WHERE step_id = NEW.id)
                      OR EXISTS (SELECT 1 FROM settlement_refund_matches WHERE step_id = NEW.id);
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

-- INITIALLY DEFERRED runs it at COMMIT: the record's foreign key needs the step to exist
-- first, so the step cannot be checked when it is inserted.
CREATE CONSTRAINT TRIGGER settlement_movement_steps_record_check
    AFTER INSERT ON settlement_movement_steps
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_movement_step_record();

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER settlement_batches_append_only
    BEFORE UPDATE OR DELETE ON settlement_batches
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_batches_no_truncate
    BEFORE TRUNCATE ON settlement_batches
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_batch_postings_append_only
    BEFORE UPDATE OR DELETE ON settlement_batch_postings
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_batch_postings_no_truncate
    BEFORE TRUNCATE ON settlement_batch_postings
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_lines_append_only
    BEFORE UPDATE OR DELETE ON settlement_lines
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_lines_no_truncate
    BEFORE TRUNCATE ON settlement_lines
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_movement_steps_append_only
    BEFORE UPDATE OR DELETE ON settlement_movement_steps
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_movement_steps_no_truncate
    BEFORE TRUNCATE ON settlement_movement_steps
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_capture_matches_append_only
    BEFORE UPDATE OR DELETE ON settlement_capture_matches
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_capture_matches_no_truncate
    BEFORE TRUNCATE ON settlement_capture_matches
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_refund_matches_append_only
    BEFORE UPDATE OR DELETE ON settlement_refund_matches
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_refund_matches_no_truncate
    BEFORE TRUNCATE ON settlement_refund_matches
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_batches ENABLE ALWAYS TRIGGER settlement_batches_commit_check;
ALTER TABLE settlement_batches ENABLE ALWAYS TRIGGER settlement_batches_append_only;
ALTER TABLE settlement_batches ENABLE ALWAYS TRIGGER settlement_batches_no_truncate;
ALTER TABLE settlement_batch_postings ENABLE ALWAYS TRIGGER settlement_batch_postings_append_only;
ALTER TABLE settlement_batch_postings ENABLE ALWAYS TRIGGER settlement_batch_postings_no_truncate;
ALTER TABLE settlement_lines ENABLE ALWAYS TRIGGER settlement_lines_append_only;
ALTER TABLE settlement_lines ENABLE ALWAYS TRIGGER settlement_lines_no_truncate;
ALTER TABLE settlement_movement_steps ENABLE ALWAYS TRIGGER settlement_movement_steps_record_check;
ALTER TABLE settlement_movement_steps ENABLE ALWAYS TRIGGER settlement_movement_steps_append_only;
ALTER TABLE settlement_movement_steps ENABLE ALWAYS TRIGGER settlement_movement_steps_no_truncate;
ALTER TABLE settlement_capture_matches ENABLE ALWAYS TRIGGER settlement_capture_matches_append_only;
ALTER TABLE settlement_capture_matches ENABLE ALWAYS TRIGGER settlement_capture_matches_no_truncate;
ALTER TABLE settlement_refund_matches ENABLE ALWAYS TRIGGER settlement_refund_matches_append_only;
ALTER TABLE settlement_refund_matches ENABLE ALWAYS TRIGGER settlement_refund_matches_no_truncate;

-- The system role reads them all and inserts every column except xact_id and
-- recorded_at, which only their defaults fill, and the generated ones (ADR-0012,
-- ADR-0055). Never grant INSERT on a whole table: a column-level REVOKE cannot take that
-- back. The merchant's role has no privilege on any, so none is under row-level security.
GRANT SELECT ON settlement_batches, settlement_batch_postings, settlement_lines,
    settlement_movement_steps, settlement_capture_matches, settlement_refund_matches
    TO payments_app;
GRANT INSERT (id, psp, currency, number, settled_at, payout)
    ON settlement_batches TO payments_app;
GRANT INSERT (batch_id, ledger_transaction_id) ON settlement_batch_postings TO payments_app;
GRANT INSERT (batch_id, line_no, kind, gross, fee, net) ON settlement_lines TO payments_app;
GRANT INSERT (
    payment_id, position, seq, prev_seq, id, kind, from_state, to_state, fact_kind, amount
) ON settlement_movement_steps TO payments_app;
GRANT INSERT (
    batch_id, line_no, gross, psp, currency, payment_id, position, psp_fact_id, request_id,
    settled_position, step_id
) ON settlement_capture_matches TO payments_app;
GRANT INSERT (
    batch_id, line_no, gross, psp, currency, payment_id, position, psp_fact_id, request_id,
    step_id
) ON settlement_refund_matches TO payments_app;
