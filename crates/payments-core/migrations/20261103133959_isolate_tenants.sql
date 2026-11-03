-- Author: Nicola Antonello

-- What a merchant's requests may touch (ADR-0025). The tenant role gets what the
-- commands and their deferred checks need, and row-level security keeps it to the
-- merchant that its transaction names. It sees nothing until one is named, so a
-- path that forgets fails closed.

-- The ledger's tables, the accounts and the reference tables stay outside the
-- policies: one posting touches a merchant's account and the platform's, and the
-- ledger knows nothing of causes (ADR-0011). No route reads them.
GRANT SELECT ON currencies, account_kinds, accounts, ledger_transactions, ledger_entries,
    merchants, payments, payment_transitions, payment_authorizations, payment_captures,
    payment_refunds
    TO payments_tenant;

-- Every column except recorded_at and the generated kinds, as for the system role
-- (ADR-0012), and nothing on merchants: only the operator creates one (ADR-0024).
GRANT INSERT (id, kind, party, currency, normal_side) ON accounts TO payments_tenant;
GRANT INSERT (id, entry_count, currency) ON ledger_transactions TO payments_tenant;
GRANT INSERT ON ledger_entries TO payments_tenant;
GRANT INSERT (id, merchant_id, psp, currency, amount, fee_bps) ON payments TO payments_tenant;
GRANT INSERT (payment_id, seq, prev_seq, id, kind, from_status, to_status, amount,
              prev_refundable, refundable)
    ON payment_transitions TO payments_tenant;
GRANT INSERT (transition_id, payment_id, amount, payment_amount, expires_at)
    ON payment_authorizations TO payments_tenant;
GRANT INSERT (transition_id, payment_id, amount, authorized_amount, fee, captured_at,
              ledger_transaction_id)
    ON payment_captures TO payments_tenant;
GRANT INSERT (transition_id, payment_id, amount, refunded_at, ledger_transaction_id)
    ON payment_refunds TO payments_tenant;

ALTER TABLE merchants ENABLE ROW LEVEL SECURITY;
ALTER TABLE payments ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_transitions ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_authorizations ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_captures ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_refunds ENABLE ROW LEVEL SECURITY;

-- The setting is NULL in a fresh session, and '' once a transaction has set it and
-- ended, which would make a bare ::uuid raise 22P02 on a pooled connection. Spelt
-- out rather than in a function: a policy is stored parsed, so it has no
-- search_path to attack (ADR-0004). With no WITH CHECK, USING also judges new rows.
CREATE POLICY merchants_tenant_isolation ON merchants FOR SELECT TO payments_tenant
    USING (id = NULLIF(current_setting('app.merchant', true), '')::uuid);
CREATE POLICY payments_tenant_isolation ON payments TO payments_tenant
    USING (merchant_id = NULLIF(current_setting('app.merchant', true), '')::uuid);

-- These have no merchant of their own: a row is visible when its payment is, under
-- the policy above. A payment's merchant never changes, so the race that the manual
-- warns of for a sub-select in a policy cannot occur.
CREATE POLICY payment_transitions_tenant_isolation ON payment_transitions TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
CREATE POLICY payment_authorizations_tenant_isolation ON payment_authorizations TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
CREATE POLICY payment_captures_tenant_isolation ON payment_captures TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
CREATE POLICY payment_refunds_tenant_isolation ON payment_refunds TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
