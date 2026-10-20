-- Author: Nicola Antonello

-- Ledger transactions and their entries (ADR-0004, ADR-0010, ADR-0012,
-- ADR-0013). A ledger transaction is one header row plus exactly
-- entry_count entries, all in its one currency, summing to zero. Every
-- column is NOT NULL (ADR-0003), and every CHECK, key and foreign key is
-- named.

CREATE TABLE ledger_transactions (
    id          uuid          NOT NULL,
    -- Its entries fill lines 1 to entry_count, and no more (ADR-0004).
    entry_count integer       NOT NULL,
    -- The one currency of all its entries (ADR-0010). No foreign key to
    -- currencies: every entry's account already references it, and the key
    -- would lock the currency's row in every posting.
    currency    currency_code NOT NULL,
    -- The start of the inserting database transaction (ADR-0012).
    recorded_at timestamptz   NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT ledger_transactions_pkey PRIMARY KEY (id),
    CONSTRAINT ledger_transactions_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT ledger_transactions_entry_count_check CHECK (entry_count >= 2),
    -- The target of the entries' key, which repeats all three columns. A
    -- foreign key must reference a unique key, so it is declared although
    -- id alone is unique (ADR-0010).
    CONSTRAINT ledger_transactions_id_entry_count_currency_key
        UNIQUE (id, entry_count, currency)
);

CREATE TABLE ledger_entries (
    transaction_id uuid          NOT NULL,
    line_no        integer       NOT NULL,
    entry_count    integer       NOT NULL,
    account_id     uuid          NOT NULL,
    currency       currency_code NOT NULL,
    -- Minor units, debit positive (ADR-0003).
    amount         bigint        NOT NULL,
    CONSTRAINT ledger_entries_pkey PRIMARY KEY (transaction_id, line_no),
    CONSTRAINT ledger_entries_line_no_check CHECK (line_no BETWEEN 1 AND entry_count),
    -- Never zero, and within plus or minus 10^15 minor units (ADR-0003).
    CONSTRAINT ledger_entries_amount_check
        CHECK (amount <> 0 AND amount BETWEEN -1000000000000000 AND 1000000000000000),
    -- The same ledger transaction, its declared count and its currency
    -- (ADR-0004, ADR-0010). Not deferrable: the header is inserted first.
    CONSTRAINT ledger_entries_header_fkey
        FOREIGN KEY (transaction_id, entry_count, currency)
        REFERENCES ledger_transactions (id, entry_count, currency),
    -- The same currency as the account (ADR-0003).
    CONSTRAINT ledger_entries_account_fkey
        FOREIGN KEY (account_id, currency) REFERENCES accounts (id, currency)
);

-- Covers the balance query, so PostgreSQL can answer it with an index-only
-- scan, which still reads the table for pages that vacuum has not yet marked
-- all-visible (ADR-0002).
CREATE INDEX ledger_entries_account_id_idx ON ledger_entries (account_id) INCLUDE (amount);

-- Checks a new ledger transaction at COMMIT, once all its entries exist:
-- complete first, then balanced, each rule with its own SQLSTATE
-- (ADR-0013). The currency needs no check here: the entries' key already
-- ties every entry to the header's currency (ADR-0010). In the RAISE
-- SQLSTATE form, a malformed code fails this migration rather than the
-- check.
CREATE FUNCTION check_ledger_transaction() RETURNS trigger
    LANGUAGE plpgsql
    -- Look up nothing in the session's temporary schema first (ADR-0004).
    SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
    found_entries bigint;
    -- sum(bigint) returns numeric, so the sum is exact.
    entry_sum     numeric;
BEGIN
    SELECT count(*), COALESCE(sum(amount), 0)
      INTO found_entries, entry_sum
      FROM ledger_entries
     WHERE transaction_id = NEW.id;

    IF found_entries <> NEW.entry_count THEN
        RAISE SQLSTATE 'LG002' USING
            MESSAGE = format('ledger transaction %s is incomplete', NEW.id),
            DETAIL = format('It declares %s entries and has %s.', NEW.entry_count, found_entries),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    -- 0::numeric, not 0: pg_catalog has no <> for numeric and integer, and
    -- PostgreSQL prefers an exact match anywhere on the search path over an
    -- operator that needs a cast, so an operator in public could take its
    -- place. Every other comparison here has an exact match in pg_catalog.
    IF entry_sum <> 0::numeric THEN
        RAISE SQLSTATE 'LG003' USING
            MESSAGE = format('ledger transaction %s is unbalanced', NEW.id),
            DETAIL = format('Its entries sum to %s minor units of %s.', entry_sum, NEW.currency),
            CONSTRAINT = TG_NAME,
            TABLE = TG_TABLE_NAME,
            SCHEMA = TG_TABLE_SCHEMA;
    END IF;
    RETURN NULL;
END;
$$;

-- A constraint trigger can be deferred like a constraint: INITIALLY
-- DEFERRED runs it at COMMIT instead of right after the header's INSERT,
-- before any entry exists (ADR-0004).
CREATE CONSTRAINT TRIGGER ledger_transactions_commit_check
    AFTER INSERT ON ledger_transactions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION check_ledger_transaction();

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER ledger_transactions_append_only
    BEFORE UPDATE OR DELETE ON ledger_transactions
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER ledger_transactions_no_truncate
    BEFORE TRUNCATE ON ledger_transactions
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER ledger_entries_append_only
    BEFORE UPDATE OR DELETE ON ledger_entries
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER ledger_entries_no_truncate
    BEFORE TRUNCATE ON ledger_entries
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE ledger_transactions ENABLE ALWAYS TRIGGER ledger_transactions_commit_check;
ALTER TABLE ledger_transactions ENABLE ALWAYS TRIGGER ledger_transactions_append_only;
ALTER TABLE ledger_transactions ENABLE ALWAYS TRIGGER ledger_transactions_no_truncate;
ALTER TABLE ledger_entries ENABLE ALWAYS TRIGGER ledger_entries_append_only;
ALTER TABLE ledger_entries ENABLE ALWAYS TRIGGER ledger_entries_no_truncate;

-- The runtime role reads both tables, inserts every column of a header
-- except recorded_at (ADR-0012), and inserts entries.
GRANT SELECT ON ledger_transactions, ledger_entries TO payments_app;
GRANT INSERT (id, entry_count, currency) ON ledger_transactions TO payments_app;
GRANT INSERT ON ledger_entries TO payments_app;
