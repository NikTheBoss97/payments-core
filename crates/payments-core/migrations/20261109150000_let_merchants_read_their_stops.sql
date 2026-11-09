-- Author: Nicola Antonello

-- A merchant reads its own payments' stops (ADR-0042). A request on a stopped payment
-- is refused with a 409, and `record_request` reads the stop under the payment's
-- lock, as the merchant. It only reads the stops, and only its own payments'
-- (ADR-0025); it writes none, and the notifications and the links of steps stay out
-- of its reach (ADR-0037). The catch-up, which writes the stops, runs as
-- `payments_app`, which bypasses row-level security.
GRANT SELECT ON payment_stops TO payments_tenant;
ALTER TABLE payment_stops ENABLE ROW LEVEL SECURITY;
CREATE POLICY payment_stops_tenant_isolation ON payment_stops
    FOR SELECT TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
