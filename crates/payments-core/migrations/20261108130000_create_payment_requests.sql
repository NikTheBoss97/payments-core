-- Author: Nicola Antonello

-- Requests to the PSP (ADR-0028, ADR-0037). A merchant's authorize, capture, refund
-- and void each record a request here and answer 202. The sender sends it after
-- that commit, under its ID, and records the PSP's answer to the send. A request's
-- state is derived from these rows and from the PSP's decisions, never stored in a
-- column (ADR-0033).

CREATE TABLE payment_requests (
    -- The RequestId, which the sender passes to the PSP as the request's
    -- idempotency key and reference on every send (ADR-0028).
    id          uuid        NOT NULL,
    payment_id  uuid        NOT NULL,
    kind        text        NOT NULL,
    -- What the request asks for, up to the cap of a ledger amount (ADR-0003). A
    -- void asks for nothing.
    amount      bigint      NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT payment_requests_pkey PRIMARY KEY (id),
    CONSTRAINT payment_requests_id_check
        CHECK (uuid_extract_version(id) IS NOT DISTINCT FROM 7),
    CONSTRAINT payment_requests_payment_fkey FOREIGN KEY (payment_id) REFERENCES payments (id),
    -- A closed set of labels, parsed once into a Rust enum (ADR-0009).
    CONSTRAINT payment_requests_kind_check
        CHECK (kind IN ('authorize', 'capture', 'refund', 'void')),
    CONSTRAINT payment_requests_amount_check CHECK (
        CASE kind
            WHEN 'void' THEN amount = 0
            ELSE amount BETWEEN 1 AND 1000000000000000
        END
    ),
    -- The targets of the foreign keys that carry a request's payment, and its kind,
    -- across to its answer and to its decision.
    CONSTRAINT payment_requests_id_payment_id_key UNIQUE (id, payment_id),
    CONSTRAINT payment_requests_id_payment_id_kind_key UNIQUE (id, payment_id, kind)
);

-- A payment has one authorization, so it gets one request for it (ADR-0028). The
-- other refusals at request time depend on what is still in flight, and stay in
-- Rust under the payment's lock: a request moves no money, only a fact of the PSP's
-- makes a step, and the steps' constraints bound every amount (ADR-0017).
CREATE UNIQUE INDEX payment_requests_authorize_key ON payment_requests (payment_id)
    WHERE kind = 'authorize';

-- The PSP's answer to a send, once per request (ADR-0028, ADR-0032): it holds the
-- request, or it refuses it as malformed, which is a bug of ours. Either stops the
-- sender. Correctness never rests on this row, since decisions come only from the
-- PSP's history (ADR-0031).
CREATE TABLE payment_request_answers (
    request_id  uuid        NOT NULL,
    payment_id  uuid        NOT NULL,
    answer      text        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT payment_request_answers_pkey PRIMARY KEY (request_id),
    CONSTRAINT payment_request_answers_request_fkey
        FOREIGN KEY (request_id, payment_id) REFERENCES payment_requests (id, payment_id),
    CONSTRAINT payment_request_answers_answer_check
        CHECK (answer IN ('acknowledged', 'malformed'))
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER payment_requests_append_only
    BEFORE UPDATE OR DELETE ON payment_requests
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_requests_no_truncate
    BEFORE TRUNCATE ON payment_requests
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_request_answers_append_only
    BEFORE UPDATE OR DELETE ON payment_request_answers
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_request_answers_no_truncate
    BEFORE TRUNCATE ON payment_request_answers
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_requests ENABLE ALWAYS TRIGGER payment_requests_append_only;
ALTER TABLE payment_requests ENABLE ALWAYS TRIGGER payment_requests_no_truncate;
ALTER TABLE payment_request_answers ENABLE ALWAYS TRIGGER payment_request_answers_append_only;
ALTER TABLE payment_request_answers ENABLE ALWAYS TRIGGER payment_request_answers_no_truncate;

-- Only a merchant asks the PSP for something, so only the tenant role inserts a
-- request. The system role runs the sender: it reads the requests and records the
-- answers. Every column except recorded_at (ADR-0012), granted by column.
GRANT SELECT ON payment_requests, payment_request_answers TO payments_app, payments_tenant;
GRANT INSERT (id, payment_id, kind, amount) ON payment_requests TO payments_tenant;
GRANT INSERT (request_id, payment_id, answer) ON payment_request_answers TO payments_app;

-- A merchant sees its own payments' requests and answers (ADR-0025, ADR-0033). A
-- row is visible when its payment is, and with no WITH CHECK, USING also judges a
-- new request, so a merchant cannot ask about another merchant's payment.
ALTER TABLE payment_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_request_answers ENABLE ROW LEVEL SECURITY;
CREATE POLICY payment_requests_tenant_isolation ON payment_requests TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
CREATE POLICY payment_request_answers_tenant_isolation ON payment_request_answers
    FOR SELECT TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
