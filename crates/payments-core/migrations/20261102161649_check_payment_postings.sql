-- Author: Nicola Antonello

-- A capture or refund record's posting is exactly the record's lines, written
-- by the same database transaction (LG006, ADR-0013, ADR-0021). This repeats
-- the posting rule of ADR-0020 on purpose, as the ledger's commit-time check
-- repeats its own rules (ADR-0004).
--
-- It closes two attacks: one posting linked as one payment's capture and as
-- another's refund, and a record linked to an older posting that nothing
-- caused. The first cannot match both shapes, because a capture debits the PSP's
-- receivable and a refund credits it. The second fails the xmin test.
CREATE FUNCTION check_payment_posting() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    merchant         text;
    psp_label        text;
    payment_currency text;
    posting_currency text;
    written_here     boolean;
    receivable_line  bigint;
    payable_line     bigint;
    fee_line         bigint;
    differing        bigint;
BEGIN
    IF TG_TABLE_NAME = 'payment_captures' THEN
        receivable_line := NEW.amount;
        payable_line := NEW.fee - NEW.amount;
        fee_line := -NEW.fee;
    ELSE
        receivable_line := -NEW.amount;
        payable_line := NEW.amount;
        fee_line := 0;
    END IF;

    -- The currencies are compared as text: pg_catalog has no operator for the
    -- currency_code domain, so one in public could otherwise take its place
    -- (ADR-0004). The other comparisons have exact matches in pg_catalog.
    SELECT p.merchant_id::text, p.psp, p.currency::text
      INTO merchant, psp_label, payment_currency
      FROM payments p
     WHERE p.id = NEW.payment_id;

    -- A row written inside a savepoint carries the savepoint's own transaction
    -- ID, so a posting written there fails this test (ADR-0017).
    SELECT t.xmin = pg_current_xact_id()::xid, t.currency::text
      INTO written_here, posting_currency
      FROM ledger_transactions t
     WHERE t.id = NEW.ledger_transaction_id;

    -- Set operations compare with each type's default equality, not through
    -- the search path. EXCEPT ALL in both directions finds a missing line, an
    -- extra one, a changed amount and a wrong account.
    WITH expected (kind, party, amount) AS (
        SELECT line.kind, line.party, line.amount
          FROM (VALUES
                    ('psp_receivable', psp_label, receivable_line),
                    ('merchant_payable', merchant, payable_line),
                    ('fee_revenue', 'platform', fee_line)
               ) AS line (kind, party, amount)
         WHERE line.amount <> 0::bigint
    ), actual (kind, party, amount) AS (
        SELECT a.kind, a.party, e.amount
          FROM ledger_entries e
          JOIN accounts a ON a.id = e.account_id
         WHERE e.transaction_id = NEW.ledger_transaction_id
    )
    SELECT count(*)
      INTO differing
      FROM (
          (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual)
          UNION ALL
          (SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
      ) AS difference;

    IF NOT written_here OR posting_currency <> payment_currency OR differing <> 0::bigint THEN
        RAISE SQLSTATE 'LG006' USING
            MESSAGE = format(
                'the posting of payment step %s is not exactly its lines',
                NEW.transition_id
            ),
            DETAIL = format(
                'Posting %s: written by this transaction: %s, currency %s against %s, %s lines differ.',
                NEW.ledger_transaction_id, written_here, posting_currency, payment_currency,
                differing
            ),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- INITIALLY DEFERRED runs them at COMMIT, so a line added after the record is
-- still seen.
CREATE CONSTRAINT TRIGGER payment_captures_commit_check
    AFTER INSERT ON payment_captures
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_payment_posting();
CREATE CONSTRAINT TRIGGER payment_refunds_commit_check
    AFTER INSERT ON payment_refunds
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_payment_posting();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_captures ENABLE ALWAYS TRIGGER payment_captures_commit_check;
ALTER TABLE payment_refunds ENABLE ALWAYS TRIGGER payment_refunds_commit_check;
