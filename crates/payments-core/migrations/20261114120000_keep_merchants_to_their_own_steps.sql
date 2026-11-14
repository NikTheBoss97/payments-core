-- Author: Nicola Antonello

-- Only the PSP's word moves a payment (ADR-0034), so a merchant's credential writes no
-- ledger line and no record of a step, and of the steps only its own: a payment's first
-- step and a cancel. Every other step comes from a fact of the PSP's history, which the
-- catch-up applies as `payments_app`.

-- Revoked at the table level, which in PostgreSQL also revokes the privilege on each
-- column, so it takes every column grant of ADR-0012 and ADR-0017 with it. What the role
-- keeps stays granted by column.
REVOKE INSERT ON ledger_transactions, ledger_entries, payment_authorizations,
    payment_captures, payment_refunds FROM payments_tenant;

-- Restrictive: a new step must pass it as well as a permissive policy, here
-- `payment_transitions_tenant_isolation`. For INSERT only, so that the merchant still
-- reads every step of its payments, the PSP's included. Its name departs from ADR-0025's
-- pattern because it guards kinds, not tenants.
CREATE POLICY payment_transitions_tenant_steps ON payment_transitions
    AS RESTRICTIVE FOR INSERT TO payments_tenant
    WITH CHECK (kind IN ('create', 'cancel'));
