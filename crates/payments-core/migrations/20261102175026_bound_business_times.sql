-- Author: Nicola Antonello

-- A business time is one that Rust can read back (ADR-0022). A timestamptz also
-- holds infinity and times beyond chrono's latest, and sqlx panics when it decodes
-- one, so a manual session could make a retry panic. These bounds are exactly
-- what the BusinessTime type accepts, as a CHECK matches every other domain type.

ALTER TABLE payment_authorizations
    ADD CONSTRAINT payment_authorizations_expires_at_check
    CHECK (expires_at BETWEEN '4714-11-24 00:00:00+00 BC' AND '262142-12-31 23:59:59.999999+00');

ALTER TABLE payment_captures
    ADD CONSTRAINT payment_captures_captured_at_check
    CHECK (captured_at BETWEEN '4714-11-24 00:00:00+00 BC' AND '262142-12-31 23:59:59.999999+00');

ALTER TABLE payment_refunds
    ADD CONSTRAINT payment_refunds_refunded_at_check
    CHECK (refunded_at BETWEEN '4714-11-24 00:00:00+00 BC' AND '262142-12-31 23:59:59.999999+00');
