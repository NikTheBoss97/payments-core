-- Author: Nicola Antonello

-- A business time keeps to the years 0000 to 9999 that RFC 3339 writes (ADR-0059),
-- which is what the BusinessTime type accepts, so sqlx can read back every time
-- these columns hold. Each check keeps its name. '0001-01-01 BC' is RFC 3339's
-- year 0000.

ALTER TABLE payment_authorizations
    DROP CONSTRAINT payment_authorizations_expires_at_check,
    ADD CONSTRAINT payment_authorizations_expires_at_check
    CHECK (expires_at BETWEEN '0001-01-01 00:00:00+00 BC' AND '9999-12-31 23:59:59.999999+00');

ALTER TABLE payment_captures
    DROP CONSTRAINT payment_captures_captured_at_check,
    ADD CONSTRAINT payment_captures_captured_at_check
    CHECK (captured_at BETWEEN '0001-01-01 00:00:00+00 BC' AND '9999-12-31 23:59:59.999999+00');

ALTER TABLE payment_refunds
    DROP CONSTRAINT payment_refunds_refunded_at_check,
    ADD CONSTRAINT payment_refunds_refunded_at_check
    CHECK (refunded_at BETWEEN '0001-01-01 00:00:00+00 BC' AND '9999-12-31 23:59:59.999999+00');
