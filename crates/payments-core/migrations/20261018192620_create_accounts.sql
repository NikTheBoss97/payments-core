-- Author: Nicola Antonello

-- Accounts (ADR-0009): append-only rows identified by their kind, party and
-- currency. Every column is NOT NULL (ADR-0003). Every CHECK, key and
-- foreign key is named, because tests and the application match on the
-- name; a NOT NULL error names the column instead.

CREATE TABLE accounts (
    id          uuid          NOT NULL,
    -- What the account is: its place in the chart of accounts, such as
    -- fee_revenue or merchant_payable. Phase 2 adds the chart itself and a
    -- foreign key (kind, normal_side) to it.
    kind        text          NOT NULL,
    -- Whom the account is kept for: 'platform' for the business's own
    -- accounts, a lowercase label such as a PSP's name, or a merchant's ID as
    -- a canonical lowercase UUID. A label has no hyphen, which a UUID always
    -- has, and is never 32 hexadecimal digits, a UUID without its hyphens,
    -- so every party has exactly one spelling.
    party       text          NOT NULL,
    currency    currency_code NOT NULL,
    -- The side on which the balance increases (ADR-0003). For display only:
    -- it never decides the sign of an entry.
    normal_side text          NOT NULL,
    -- The start of the inserting database transaction (ADR-0012): a lower
    -- bound on the insert and commit times, never a cursor or a cut-off.
    recorded_at timestamptz   NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT accounts_pkey PRIMARY KEY (id),
    -- uuid_extract_version returns NULL for a UUID that is not of the RFC 9562
    -- variant, such as the nil UUID, and a CHECK that evaluates to NULL
    -- passes, hence IS NOT DISTINCT FROM (ADR-0005).
    CONSTRAINT accounts_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT accounts_kind_check CHECK (kind ~ '^[a-z][a-z0-9_]{0,62}$'),
    CONSTRAINT accounts_party_check CHECK (
        (party ~ '^[a-z][a-z0-9_]{0,62}$' AND party !~ '^[0-9a-f]{32}$')
        OR party ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    ),
    CONSTRAINT accounts_normal_side_check CHECK (normal_side IN ('debit', 'credit')),
    CONSTRAINT accounts_currency_fkey FOREIGN KEY (currency) REFERENCES currencies (code),
    -- The business key: how code finds an account, and what makes creating
    -- one idempotent (ADR-0009).
    CONSTRAINT accounts_kind_party_currency_key UNIQUE (kind, party, currency),
    -- The target of the entries' foreign key, which ties each entry's
    -- currency to its account's (ADR-0003).
    CONSTRAINT accounts_id_currency_key UNIQUE (id, currency)
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER accounts_append_only
    BEFORE UPDATE OR DELETE ON accounts
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER accounts_no_truncate
    BEFORE TRUNCATE ON accounts
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE accounts ENABLE ALWAYS TRIGGER accounts_append_only;
ALTER TABLE accounts ENABLE ALWAYS TRIGGER accounts_no_truncate;

-- The runtime role reads accounts and inserts every column except
-- recorded_at, which only its default may fill (ADR-0012). Never grant
-- INSERT on the whole table: that would let the role set recorded_at, and a
-- column-level REVOKE cannot take a table-level grant back.
GRANT SELECT ON accounts TO payments_app;
GRANT INSERT (id, kind, party, currency, normal_side) ON accounts TO payments_app;
