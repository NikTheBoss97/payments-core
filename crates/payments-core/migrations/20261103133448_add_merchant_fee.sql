-- Author: Nicola Antonello

-- The rate a merchant pays, set by the operator: a merchant that authenticates
-- must not choose its own fee (ADR-0024). A new payment copies it, and ADR-0020
-- fixes it there for the payment's life. No deployment holds rows yet, so the
-- column needs no default, and a merchant without a rate is refused.

ALTER TABLE merchants
    ADD COLUMN fee_bps integer NOT NULL,
    ADD CONSTRAINT merchants_fee_bps_check CHECK (fee_bps BETWEEN 0 AND 10000);

GRANT INSERT (fee_bps) ON merchants TO payments_app;
