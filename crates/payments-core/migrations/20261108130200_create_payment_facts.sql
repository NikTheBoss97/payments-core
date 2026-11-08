-- Author: Nicola Antonello

-- The PSP's facts about a payment (ADR-0031, ADR-0034, ADR-0037): what the service
-- holds of the PSP's history, always a prefix of it, chained by position as each
-- step names its predecessor (ADR-0017). Only the catch-up writes them, as the
-- system role. A fact's kind is the step that it makes, or one of three refusals,
-- so the links below can prove that a step, its fact and its request agree.
-- Business times stay on the steps' records (ADR-0022): a fact keeps what the
-- chain and the links need.

-- The target of the facts' foreign key, which proves that a fact comes from the
-- payment's own PSP.
ALTER TABLE payments ADD CONSTRAINT payments_id_psp_key UNIQUE (id, psp);

CREATE TABLE payment_facts (
    payment_id    uuid        NOT NULL,
    -- 1 for the PSP's first fact about the payment, then one more than
    -- prev_position.
    position      integer     NOT NULL,
    prev_position integer     NOT NULL,
    psp           text        NOT NULL,
    -- The PSP's own ID for the fact, which every catch-up compares, position by
    -- position, with the history that it reads.
    psp_fact_id   text        NOT NULL,
    kind          text        NOT NULL,
    -- What an authorization, a capture or a refund moved, up to the cap of a
    -- ledger amount (ADR-0003). The other kinds move nothing.
    amount        bigint      NOT NULL,
    recorded_at   timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT payment_facts_pkey PRIMARY KEY (payment_id, position),
    -- The first fact points to itself, so every column stays NOT NULL.
    CONSTRAINT payment_facts_prev_fkey
        FOREIGN KEY (payment_id, prev_position) REFERENCES payment_facts (payment_id, position),
    CONSTRAINT payment_facts_position_check CHECK (
        (position = 1 AND prev_position = 1) OR (position >= 2 AND prev_position = position - 1)
    ),
    CONSTRAINT payment_facts_payment_fkey
        FOREIGN KEY (payment_id, psp) REFERENCES payments (id, psp),
    CONSTRAINT payment_facts_psp_fact_id_key UNIQUE (psp, psp_fact_id),
    -- The shape of the PSP's event IDs (ADR-0030).
    CONSTRAINT payment_facts_psp_fact_id_check
        CHECK (psp_fact_id ~ '^[0-9A-Za-z_-]{1,255}$'),
    -- A closed set of labels, parsed once into a Rust enum (ADR-0009): the kinds of
    -- step that the PSP decides (ADR-0016), and the refusals that close a request
    -- without one.
    CONSTRAINT payment_facts_kind_check CHECK (kind IN (
        'authorize', 'decline', 'capture', 'capture_refused', 'void', 'void_refused',
        'expire', 'settle', 'refund', 'refund_refused'
    )),
    CONSTRAINT payment_facts_amount_check CHECK (
        CASE WHEN kind IN ('authorize', 'capture', 'refund')
            THEN amount BETWEEN 1 AND 1000000000000000
            ELSE amount = 0
        END
    ),
    -- The targets of the links' foreign keys, which carry a fact's kind, and its
    -- amount, across.
    CONSTRAINT payment_facts_kind_key UNIQUE (payment_id, position, kind),
    CONSTRAINT payment_facts_kind_amount_key UNIQUE (payment_id, position, kind, amount)
);

-- The fact that decided a request (ADR-0031): one decision per request, and a fact
-- decides at most one. Both kinds come across through the foreign keys, so the
-- CHECK can compare them: an authorization request is decided by an authorization
-- or a decline, and each of the others by its own kind or its refusal. A decision
-- commits with its fact, so it has no recorded_at of its own (ADR-0012).
CREATE TABLE payment_request_decisions (
    request_id   uuid    NOT NULL,
    payment_id   uuid    NOT NULL,
    request_kind text    NOT NULL,
    position     integer NOT NULL,
    fact_kind    text    NOT NULL,
    CONSTRAINT payment_request_decisions_pkey PRIMARY KEY (request_id),
    CONSTRAINT payment_request_decisions_request_fkey
        FOREIGN KEY (request_id, payment_id, request_kind)
        REFERENCES payment_requests (id, payment_id, kind),
    CONSTRAINT payment_request_decisions_fact_fkey
        FOREIGN KEY (payment_id, position, fact_kind)
        REFERENCES payment_facts (payment_id, position, kind),
    CONSTRAINT payment_request_decisions_fact_key UNIQUE (payment_id, position),
    CONSTRAINT payment_request_decisions_kinds_check CHECK ((request_kind, fact_kind) IN (
        ('authorize', 'authorize'), ('authorize', 'decline'),
        ('capture', 'capture'), ('capture', 'capture_refused'),
        ('void', 'void'), ('void', 'void_refused'),
        ('refund', 'refund'), ('refund', 'refund_refused')
    ))
);

-- The fact that made a step (ADR-0031, ADR-0034), keyed by the step, as ADR-0017
-- planned for later facts about a step. The kind and the amount come across
-- through both foreign keys, so a step and its fact agree on both, and only a kind
-- that is both a step and a fact can be linked: never a creation or a
-- cancellation, which are the service's own, nor a refusal, which makes no step. A
-- fact makes at most one step. LG007 will refuse a step of the PSP's without its
-- row (ADR-0034). It commits with its step, so it has no recorded_at of its own.
CREATE TABLE payment_transition_facts (
    transition_id uuid    NOT NULL,
    payment_id    uuid    NOT NULL,
    kind          text    NOT NULL,
    amount        bigint  NOT NULL,
    position      integer NOT NULL,
    CONSTRAINT payment_transition_facts_pkey PRIMARY KEY (transition_id),
    CONSTRAINT payment_transition_facts_step_fkey
        FOREIGN KEY (transition_id, payment_id, kind, amount)
        REFERENCES payment_transitions (id, payment_id, kind, amount),
    CONSTRAINT payment_transition_facts_fact_fkey
        FOREIGN KEY (payment_id, position, kind, amount)
        REFERENCES payment_facts (payment_id, position, kind, amount),
    CONSTRAINT payment_transition_facts_fact_key UNIQUE (payment_id, position)
);

-- A payment that the catch-up stopped for an operator (ADR-0031): the facts that
-- the service holds are not the PSP's first facts, or the fact at this position
-- has no place in the model, or contradicts what the service holds, such as a
-- decision on a request that it never sent. Nothing more is applied to the
-- payment, and what an operator does next is outside phase 4. The fact is never
-- skipped, nor recorded as one, so the chain stays the prefix that agreed.
CREATE TABLE payment_stops (
    payment_id  uuid        NOT NULL,
    -- The position of the PSP's history where the catch-up stopped.
    position    integer     NOT NULL,
    reason      text        NOT NULL,
    recorded_at timestamptz NOT NULL DEFAULT transaction_timestamp(),
    CONSTRAINT payment_stops_pkey PRIMARY KEY (payment_id),
    CONSTRAINT payment_stops_payment_fkey FOREIGN KEY (payment_id) REFERENCES payments (id),
    CONSTRAINT payment_stops_position_check CHECK (position >= 1),
    CONSTRAINT payment_stops_reason_check
        CHECK (reason IN ('history_changed', 'no_place', 'contradiction'))
);

-- Append-only (ADR-0004). TRUNCATE has only a statement-level form.
CREATE TRIGGER payment_facts_append_only
    BEFORE UPDATE OR DELETE ON payment_facts
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_facts_no_truncate
    BEFORE TRUNCATE ON payment_facts
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_request_decisions_append_only
    BEFORE UPDATE OR DELETE ON payment_request_decisions
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_request_decisions_no_truncate
    BEFORE TRUNCATE ON payment_request_decisions
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_transition_facts_append_only
    BEFORE UPDATE OR DELETE ON payment_transition_facts
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_transition_facts_no_truncate
    BEFORE TRUNCATE ON payment_transition_facts
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_stops_append_only
    BEFORE UPDATE OR DELETE ON payment_stops
    FOR EACH ROW EXECUTE FUNCTION reject_change();
CREATE TRIGGER payment_stops_no_truncate
    BEFORE TRUNCATE ON payment_stops
    FOR EACH STATEMENT EXECUTE FUNCTION reject_change();

-- Fire even in a session with session_replication_role = replica (ADR-0004).
ALTER TABLE payment_facts ENABLE ALWAYS TRIGGER payment_facts_append_only;
ALTER TABLE payment_facts ENABLE ALWAYS TRIGGER payment_facts_no_truncate;
ALTER TABLE payment_request_decisions ENABLE ALWAYS TRIGGER payment_request_decisions_append_only;
ALTER TABLE payment_request_decisions ENABLE ALWAYS TRIGGER payment_request_decisions_no_truncate;
ALTER TABLE payment_transition_facts ENABLE ALWAYS TRIGGER payment_transition_facts_append_only;
ALTER TABLE payment_transition_facts ENABLE ALWAYS TRIGGER payment_transition_facts_no_truncate;
ALTER TABLE payment_stops ENABLE ALWAYS TRIGGER payment_stops_append_only;
ALTER TABLE payment_stops ENABLE ALWAYS TRIGGER payment_stops_no_truncate;

-- Only the catch-up writes these, as the system role: every column except
-- recorded_at (ADR-0012), granted by column.
GRANT SELECT ON payment_facts, payment_request_decisions, payment_transition_facts,
    payment_stops TO payments_app;
GRANT INSERT (payment_id, position, prev_position, psp, psp_fact_id, kind, amount)
    ON payment_facts TO payments_app;
GRANT INSERT (request_id, payment_id, request_kind, position, fact_kind)
    ON payment_request_decisions TO payments_app;
GRANT INSERT (transition_id, payment_id, kind, amount, position)
    ON payment_transition_facts TO payments_app;
GRANT INSERT (payment_id, position, reason) ON payment_stops TO payments_app;

-- A merchant follows its requests (ADR-0033): a decision and its fact tell it
-- whether one was applied or refused. It only reads them, and only its own
-- payments' (ADR-0025).
GRANT SELECT ON payment_facts, payment_request_decisions TO payments_tenant;
ALTER TABLE payment_facts ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_request_decisions ENABLE ROW LEVEL SECURITY;
CREATE POLICY payment_facts_tenant_isolation ON payment_facts
    FOR SELECT TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
CREATE POLICY payment_request_decisions_tenant_isolation ON payment_request_decisions
    FOR SELECT TO payments_tenant
    USING (EXISTS (SELECT 1 FROM payments p WHERE p.id = payment_id));
