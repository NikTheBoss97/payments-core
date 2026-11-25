-- Author: Nicola Antonello

-- An operator's outcome on a discrepancy whose chain is its own (ADR-0069, ADR-0072): a
-- claim on the PSP, a write-off or an acceptance, each a step of the discrepancy's chain
-- with its record here and the reason given for it. A claim and a write-off move the
-- discrepancy's amount, and a row links each to its posting. At COMMIT, LG010 refuses an
-- outcome whose posting is not exactly its lines, and LG011 a step of the chain without its
-- record. Everything here is written only by the system role.

-- The outcomes that each kind allows (ADR-0069). The evidence steps come with the late
-- matches and the arrivals of missing batches.
ALTER TABLE settlement_discrepancy_steps
    DROP CONSTRAINT settlement_discrepancy_steps_edge_check,
    ADD CONSTRAINT settlement_discrepancy_steps_edge_check CHECK (
        (kind = 'find') = (seq = 0)
        AND (discrepancy_kind, from_state, kind, to_state) IN (
            ('unmatched_line', 'open', 'find', 'open'),
            ('unmatched_line', 'open', 'claim', 'claimed'),
            ('unmatched_line', 'open', 'write_off', 'written_off'),
            ('payout_difference', 'open', 'find', 'open'),
            ('payout_difference', 'open', 'claim', 'claimed'),
            ('payout_difference', 'open', 'write_off', 'written_off'),
            ('fee_difference', 'open', 'find', 'open'),
            ('fee_difference', 'open', 'claim', 'claimed'),
            ('fee_difference', 'open', 'accept', 'accepted'),
            ('missing_batch', 'open', 'find', 'open'),
            ('missing_batch', 'open', 'accept', 'accepted'),
            ('uncaused_posting', 'open', 'find', 'open'),
            ('uncaused_posting', 'open', 'accept', 'accepted')
        )
    );

-- The record of an outcome's step, which commits with it.
CREATE TABLE settlement_discrepancy_outcomes (
    step_id        uuid NOT NULL,
    discrepancy_id uuid NOT NULL,
    kind           text NOT NULL,
    -- Why the operator chose it, shown in the discrepancy's history (ADR-0069).
    reason         text NOT NULL,
    -- The ID of the inserting transaction, which the posting's link repeats, so that no
    -- link joins the outcome later and escapes LG010 (ADR-0072). Only this default fills it.
    xact_id        xid8 NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_discrepancy_outcomes_pkey PRIMARY KEY (step_id),
    CONSTRAINT settlement_discrepancy_outcomes_step_fkey
        FOREIGN KEY (step_id, discrepancy_id, kind)
        REFERENCES settlement_discrepancy_steps (id, discrepancy_id, kind),
    CONSTRAINT settlement_discrepancy_outcomes_kind_check
        CHECK (kind IN ('claim', 'write_off', 'accept')),
    -- Characters, not bytes, as the route counts them.
    CONSTRAINT settlement_discrepancy_outcomes_reason_check
        CHECK (char_length(reason) BETWEEN 1 AND 500),
    -- The target of the posting link's key, which carries the kind and the transaction.
    CONSTRAINT settlement_discrepancy_outcomes_link_key UNIQUE (step_id, kind, xact_id)
);

-- A claim's or a write-off's posting. It commits with its outcome.
CREATE TABLE settlement_discrepancy_outcome_postings (
    step_id               uuid NOT NULL,
    kind                  text NOT NULL,
    ledger_transaction_id uuid NOT NULL,
    xact_id               xid8 NOT NULL DEFAULT pg_current_xact_id(),
    CONSTRAINT settlement_discrepancy_outcome_postings_pkey PRIMARY KEY (step_id),
    -- Its outcome, of the same kind, written by the same transaction.
    CONSTRAINT settlement_discrepancy_outcome_postings_outcome_fkey
        FOREIGN KEY (step_id, kind, xact_id)
        REFERENCES settlement_discrepancy_outcomes (step_id, kind, xact_id),
    CONSTRAINT settlement_discrepancy_outcome_postings_kind_check
        CHECK (kind IN ('claim', 'write_off')),
    CONSTRAINT settlement_discrepancy_outcome_postings_ledger_transaction_key
        UNIQUE (ledger_transaction_id),
    CONSTRAINT settlement_discrepancy_outcome_postings_ledger_transaction_fkey
        FOREIGN KEY (ledger_transaction_id) REFERENCES ledger_transactions (id)
);

-- An outcome's posting is exactly its lines (LG010, ADR-0069): a claim or a write-off moves
-- the discrepancy's amount out of the account where booking left it, settlement_suspense
-- for a line's net or a payout's difference and psp_fees for a fee's difference, to
-- psp_claims (the PSP's) or settlement_writeoffs (the platform's). An acceptance moves
-- nothing. The link exists exactly when an entry remains, as a batch's does (LG009), and
-- the posting was written by this transaction, in the batch's currency. This repeats
-- ADR-0069's rule on purpose, as LG006 repeats a capture's (ADR-0021). The comparisons
-- have exact matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_discrepancy_outcome() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    discrepancy_kind text;
    moved            bigint := 0;
    from_kind        text;
    psp_label        text;
    batch_currency   text;
    to_kind          text;
    to_party         text;
    posting          uuid;
    written_here     boolean;
    posting_currency text;
    remaining        bigint;
    differing        bigint;
BEGIN
    SELECT d.kind INTO discrepancy_kind
      FROM settlement_discrepancies d
     WHERE d.id = NEW.discrepancy_id;

    -- What the outcome moves, as the subject holds it, and from where.
    IF NEW.kind = 'accept' THEN
        moved := 0;
    ELSIF discrepancy_kind = 'unmatched_line' THEN
        SELECT u.net, b.psp, b.currency::text
          INTO moved, psp_label, batch_currency
          FROM settlement_unmatched_lines u
          JOIN settlement_batches b ON b.id = u.batch_id
         WHERE u.discrepancy_id = NEW.discrepancy_id;
        from_kind := 'settlement_suspense';
    ELSIF discrepancy_kind = 'payout_difference' THEN
        SELECT p.amount, b.psp, b.currency::text
          INTO moved, psp_label, batch_currency
          FROM settlement_payout_differences p
          JOIN settlement_batches b ON b.id = p.batch_id
         WHERE p.discrepancy_id = NEW.discrepancy_id;
        from_kind := 'settlement_suspense';
    ELSIF discrepancy_kind = 'fee_difference' THEN
        -- The fee was booked as charged, so a claim takes back what it charged over the
        -- terms.
        SELECT f.agreed - f.charged, b.psp, b.currency::text
          INTO moved, psp_label, batch_currency
          FROM settlement_fee_differences f
          JOIN settlement_batches b ON b.id = f.batch_id
         WHERE f.discrepancy_id = NEW.discrepancy_id;
        from_kind := 'psp_fees';
    END IF;
    IF NEW.kind = 'claim' THEN
        to_kind := 'psp_claims';
        to_party := psp_label;
    ELSE
        to_kind := 'settlement_writeoffs';
        to_party := 'platform';
    END IF;

    -- With no link, posting stays NULL, and so do the two that depend on it.
    SELECT o.ledger_transaction_id INTO posting
      FROM settlement_discrepancy_outcome_postings o
     WHERE o.step_id = NEW.step_id;
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
                    (from_kind, psp_label, moved),
                    (to_kind, to_party, -moved)
               ) AS line (kind, party, amount)
         WHERE line.amount <> 0::bigint
    ), actual (kind, party, amount) AS (
        SELECT a.kind, a.party, e.amount
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
                OR posting_currency <> batch_currency
                OR differing <> 0::bigint)) THEN
        RAISE SQLSTATE 'LG010' USING
            MESSAGE = format('the posting of outcome %s is not exactly its lines', NEW.step_id),
            DETAIL = format(
                'Posting %s: written by this transaction: %s, currency %s against %s, '
                '%s entries expected, %s differ.',
                posting, written_here, posting_currency, batch_currency, remaining, differing
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT, after the outcome's link and posting.
CREATE CONSTRAINT TRIGGER settlement_discrepancy_outcomes_commit_check
    AFTER INSERT ON settlement_discrepancy_outcomes
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_discrepancy_outcome();

-- A discrepancy's step that needs a record has one (LG011, ADR-0069), as a movement's does:
-- an outcome's step its outcome. The evidence steps come later. The comparisons have exact
-- matches in pg_catalog (ADR-0004).
CREATE FUNCTION check_settlement_discrepancy_step_record() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    has_record boolean;
BEGIN
    IF NEW.kind IN ('claim', 'write_off', 'accept') THEN
        has_record := EXISTS (
            SELECT 1 FROM settlement_discrepancy_outcomes WHERE step_id = NEW.id
        );
    ELSE
        RETURN NULL;
    END IF;
    IF NOT has_record THEN
        RAISE SQLSTATE 'LG011' USING
            MESSAGE = format('discrepancy step %s of kind %s has no record', NEW.id, NEW.kind),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs it at COMMIT: the record's foreign key needs the step to exist
-- first, so the step cannot be checked when it is inserted.
CREATE CONSTRAINT TRIGGER settlement_discrepancy_steps_record_check
    AFTER INSERT ON settlement_discrepancy_steps
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_settlement_discrepancy_step_record();

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER settlement_discrepancy_outcomes_append_only
    BEFORE UPDATE OR DELETE ON settlement_discrepancy_outcomes
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_outcomes_no_truncate
    BEFORE TRUNCATE ON settlement_discrepancy_outcomes
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_outcome_postings_append_only
    BEFORE UPDATE OR DELETE ON settlement_discrepancy_outcome_postings
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER settlement_discrepancy_outcome_postings_no_truncate
    BEFORE TRUNCATE ON settlement_discrepancy_outcome_postings
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE settlement_discrepancy_steps
    ENABLE ALWAYS TRIGGER settlement_discrepancy_steps_record_check;
ALTER TABLE settlement_discrepancy_outcomes
    ENABLE ALWAYS TRIGGER settlement_discrepancy_outcomes_commit_check;
ALTER TABLE settlement_discrepancy_outcomes
    ENABLE ALWAYS TRIGGER settlement_discrepancy_outcomes_append_only;
ALTER TABLE settlement_discrepancy_outcomes
    ENABLE ALWAYS TRIGGER settlement_discrepancy_outcomes_no_truncate;
ALTER TABLE settlement_discrepancy_outcome_postings
    ENABLE ALWAYS TRIGGER settlement_discrepancy_outcome_postings_append_only;
ALTER TABLE settlement_discrepancy_outcome_postings
    ENABLE ALWAYS TRIGGER settlement_discrepancy_outcome_postings_no_truncate;

-- The system role reads them and inserts every column except xact_id, which only its
-- default fills (ADR-0012, ADR-0055). The merchant's role has no privilege on either, so
-- neither is under row-level security.
GRANT SELECT ON settlement_discrepancy_outcomes, settlement_discrepancy_outcome_postings
    TO payments_app;
GRANT INSERT (step_id, discrepancy_id, kind, reason)
    ON settlement_discrepancy_outcomes TO payments_app;
GRANT INSERT (step_id, kind, ledger_transaction_id)
    ON settlement_discrepancy_outcome_postings TO payments_app;
