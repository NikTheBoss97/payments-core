-- Author: Nicola Antonello

-- A merchant follows its requests (ADR-0045). An applied request shows the business
-- time of the step that applied it, which only the link from the request's fact to
-- that step reaches, since a payment may hold several refunds of one amount. The
-- merchant reads its own payments' links (ADR-0025) and writes none, and the
-- notifications stay out of its reach (ADR-0037). The catch-up, which writes the
-- links, runs as `payments_app`, which bypasses row-level security.
GRANT SELECT ON payment_transition_facts TO payments_tenant;
ALTER TABLE payment_transition_facts ENABLE ROW LEVEL SECURITY;
CREATE POLICY payment_transition_facts_tenant_isolation ON payment_transition_facts
    FOR SELECT TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
