// Author: Nicola Antonello

//! Catalog tests: what the behavioural tests cannot see (ADR-0003,
//! ADR-0004, ADR-0005, ADR-0012, ADR-0017).
//!
//! `#[sqlx::test]` connects as a superuser, which skips privilege checks, so
//! only the tests that switch to the runtime role, `payments_app`, meet
//! them, and those notice only the grants they exercise. These tests read
//! the catalogs instead. They pin the runtime role's table and column
//! privileges, its grant options and its session defaults, check that it can
//! neither create objects in the public schema nor act through another role,
//! pin each guard trigger and the function it runs, and pin the type and
//! nullability of every column and the name of every constraint. The tenant
//! role, `payments_tenant`, is pinned the same way, with the tables under
//! row-level security and the text of every policy (ADR-0025).

use sqlx::{Connection, PgConnection, PgPool};

/// A role's privileges, sorted.
async fn granted_to(pool: &PgPool, role: &str) -> sqlx::Result<Vec<String>> {
    // Effective privileges, so a grant to PUBLIC or through an inherited role
    // counts. Column privileges are listed only where the table-level one is
    // absent, and any privilege the role may grant on to others is listed
    // with WITH GRANT OPTION.
    let mut granted = sqlx::query_scalar!(
        r#"
        SELECT format('%s %s', c.relname, p.privilege) AS "grant!"
          FROM pg_class c
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE',
                                 'REFERENCES', 'TRIGGER', 'MAINTAIN']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND has_table_privilege($1::text, c.oid, p.privilege)
        UNION ALL
        SELECT format('%s %s WITH GRANT OPTION', c.relname, p.privilege)
          FROM pg_class c
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE',
                                 'REFERENCES', 'TRIGGER', 'MAINTAIN']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND has_table_privilege($1::text, c.oid, p.privilege || ' WITH GRANT OPTION')
        UNION ALL
        SELECT format('%s.%s %s', c.relname, a.attname, p.privilege)
          FROM pg_attribute a
          JOIN pg_class c ON c.oid = a.attrelid
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'REFERENCES']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND a.attnum > 0
           AND NOT a.attisdropped
           AND has_column_privilege($1::text, c.oid, a.attnum, p.privilege)
           AND NOT has_table_privilege($1::text, c.oid, p.privilege)
        UNION ALL
        SELECT format('%s.%s %s WITH GRANT OPTION', c.relname, a.attname, p.privilege)
          FROM pg_attribute a
          JOIN pg_class c ON c.oid = a.attrelid
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'REFERENCES']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND a.attnum > 0
           AND NOT a.attisdropped
           AND has_column_privilege($1::text, c.oid, a.attnum,
                                    p.privilege || ' WITH GRANT OPTION')
        "#,
        role
    )
    .fetch_all(pool)
    .await?;
    granted.sort();
    Ok(granted)
}

#[sqlx::test]
async fn the_runtime_role_has_exactly_the_expected_privileges(pool: PgPool) -> sqlx::Result<()> {
    // INSERT covers every column of accounts and ledger_transactions except
    // recorded_at, which only the database sets (ADR-0012).
    assert_eq!(
        granted_to(&pool, "payments_app").await?,
        [
            "account_kinds SELECT",
            "accounts SELECT",
            "accounts.currency INSERT",
            "accounts.id INSERT",
            "accounts.kind INSERT",
            "accounts.normal_side INSERT",
            "accounts.party INSERT",
            "api_key_revocations SELECT",
            "api_key_revocations.api_key_id INSERT",
            "api_keys SELECT",
            "api_keys.digest INSERT",
            "api_keys.id INSERT",
            "api_keys.merchant_id INSERT",
            "currencies SELECT",
            "ledger_entries INSERT",
            "ledger_entries SELECT",
            "ledger_transactions SELECT",
            "ledger_transactions.currency INSERT",
            "ledger_transactions.entry_count INSERT",
            "ledger_transactions.id INSERT",
            "merchants SELECT",
            "merchants.fee_bps INSERT",
            "merchants.id INSERT",
            "operator_idempotency_records SELECT",
            "operator_idempotency_records.body INSERT",
            "operator_idempotency_records.fingerprint INSERT",
            "operator_idempotency_records.key INSERT",
            "operator_idempotency_records.status INSERT",
            "payment_authorizations SELECT",
            "payment_authorizations.amount INSERT",
            "payment_authorizations.expires_at INSERT",
            "payment_authorizations.payment_amount INSERT",
            "payment_authorizations.payment_id INSERT",
            "payment_authorizations.transition_id INSERT",
            "payment_captures SELECT",
            "payment_captures.amount INSERT",
            "payment_captures.authorized_amount INSERT",
            "payment_captures.captured_at INSERT",
            "payment_captures.fee INSERT",
            "payment_captures.ledger_transaction_id INSERT",
            "payment_captures.payment_id INSERT",
            "payment_captures.transition_id INSERT",
            "payment_facts SELECT",
            "payment_facts.amount INSERT",
            "payment_facts.kind INSERT",
            "payment_facts.payment_id INSERT",
            "payment_facts.position INSERT",
            "payment_facts.prev_position INSERT",
            "payment_facts.psp INSERT",
            "payment_facts.psp_fact_id INSERT",
            "payment_refunds SELECT",
            "payment_refunds.amount INSERT",
            "payment_refunds.ledger_transaction_id INSERT",
            "payment_refunds.payment_id INSERT",
            "payment_refunds.refunded_at INSERT",
            "payment_refunds.transition_id INSERT",
            "payment_request_answers SELECT",
            "payment_request_answers.answer INSERT",
            "payment_request_answers.payment_id INSERT",
            "payment_request_answers.request_id INSERT",
            "payment_request_decisions SELECT",
            "payment_request_decisions.fact_kind INSERT",
            "payment_request_decisions.payment_id INSERT",
            "payment_request_decisions.position INSERT",
            "payment_request_decisions.request_id INSERT",
            "payment_request_decisions.request_kind INSERT",
            "payment_requests SELECT",
            "payment_stops SELECT",
            "payment_stops.payment_id INSERT",
            "payment_stops.position INSERT",
            "payment_stops.reason INSERT",
            "payment_transition_facts SELECT",
            "payment_transition_facts.amount INSERT",
            "payment_transition_facts.kind INSERT",
            "payment_transition_facts.payment_id INSERT",
            "payment_transition_facts.position INSERT",
            "payment_transition_facts.transition_id INSERT",
            "payment_transitions SELECT",
            "payment_transitions.amount INSERT",
            "payment_transitions.from_status INSERT",
            "payment_transitions.id INSERT",
            "payment_transitions.kind INSERT",
            "payment_transitions.payment_id INSERT",
            "payment_transitions.prev_refundable INSERT",
            "payment_transitions.prev_seq INSERT",
            "payment_transitions.refundable INSERT",
            "payment_transitions.seq INSERT",
            "payment_transitions.to_status INSERT",
            "payments SELECT",
            "payments.amount INSERT",
            "payments.currency INSERT",
            "payments.fee_bps INSERT",
            "payments.id INSERT",
            "payments.merchant_id INSERT",
            "payments.psp INSERT",
            "psp_notifications SELECT",
            "psp_notifications.event_id INSERT",
            "psp_notifications.payment_id INSERT",
            "psp_notifications.psp INSERT",
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn the_tenant_role_has_exactly_the_expected_privileges(pool: PgPool) -> sqlx::Result<()> {
    // What a merchant's requests and their deferred checks need, and no more: it
    // cannot create a merchant or see an API key (ADR-0025), and it writes no ledger
    // line and no record of a step, which only the PSP's facts bring (ADR-0034).
    assert_eq!(
        granted_to(&pool, "payments_tenant").await?,
        [
            "account_kinds SELECT",
            "accounts SELECT",
            "accounts.currency INSERT",
            "accounts.id INSERT",
            "accounts.kind INSERT",
            "accounts.normal_side INSERT",
            "accounts.party INSERT",
            "currencies SELECT",
            "idempotency_records SELECT",
            "idempotency_records.body INSERT",
            "idempotency_records.fingerprint INSERT",
            "idempotency_records.key INSERT",
            "idempotency_records.merchant_id INSERT",
            "idempotency_records.status INSERT",
            "ledger_entries SELECT",
            "ledger_transactions SELECT",
            "merchants SELECT",
            "payment_authorizations SELECT",
            "payment_captures SELECT",
            "payment_facts SELECT",
            "payment_refunds SELECT",
            "payment_request_answers SELECT",
            "payment_request_decisions SELECT",
            "payment_requests SELECT",
            "payment_requests.amount INSERT",
            "payment_requests.id INSERT",
            "payment_requests.kind INSERT",
            "payment_requests.payment_id INSERT",
            "payment_stops SELECT",
            "payment_transition_facts SELECT",
            "payment_transitions SELECT",
            "payment_transitions.amount INSERT",
            "payment_transitions.from_status INSERT",
            "payment_transitions.id INSERT",
            "payment_transitions.kind INSERT",
            "payment_transitions.payment_id INSERT",
            "payment_transitions.prev_refundable INSERT",
            "payment_transitions.prev_seq INSERT",
            "payment_transitions.refundable INSERT",
            "payment_transitions.seq INSERT",
            "payment_transitions.to_status INSERT",
            "payments SELECT",
            "payments.amount INSERT",
            "payments.currency INSERT",
            "payments.fee_bps INSERT",
            "payments.id INSERT",
            "payments.merchant_id INSERT",
            "payments.psp INSERT",
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn the_runtime_roles_cannot_switch_the_guards_off(pool: PgPool) -> sqlx::Result<()> {
    // A superuser can disable any trigger. A member of another role can act as
    // it, even through SET ROLE without inheriting its privileges. CREATE on
    // public would let the role add an operator that the guard functions'
    // search_path finds. A role allowed to set session_replication_role can
    // switch off foreign-key checks (ADR-0004). Only the system role may bypass
    // row-level security, and merchant routes never hold its pool (ADR-0025).
    let roles = sqlx::query!(
        r#"
        SELECT r.rolname::text AS "role!",
               rolsuper AS "superuser!",
               rolbypassrls AS "bypasses_row_security!",
               EXISTS (SELECT 1 FROM pg_auth_members WHERE member = r.oid)
                   AS "member_of_a_role!",
               has_schema_privilege(r.rolname, 'public', 'CREATE')
                   AS "can_create_in_public!",
               has_parameter_privilege(r.rolname, 'session_replication_role', 'SET')
                   AS "can_set_replication_role!"
          FROM pg_roles r
         WHERE r.rolname IN ('payments_app', 'payments_tenant')
         ORDER BY r.rolname
        "#
    )
    .fetch_all(&pool)
    .await?;
    let bypassing: Vec<(&str, bool)> = roles
        .iter()
        .map(|role| (role.role.as_str(), role.bypasses_row_security))
        .collect();
    assert_eq!(
        bypassing,
        [("payments_app", true), ("payments_tenant", false)]
    );
    for role in &roles {
        let name = &role.role;
        assert!(!role.superuser, "{name} is a superuser");
        assert!(!role.member_of_a_role, "{name} can act as another role");
        assert!(
            !role.can_create_in_public,
            "{name} can create objects in public"
        );
        assert!(
            !role.can_set_replication_role,
            "{name} can set session_replication_role"
        );
    }
    Ok(())
}

/// The roles that the application logs in as, whose password is their name in
/// local development and CI (ADR-0008).
const LOGIN_ROLES: [&str; 2] = ["payments_app", "payments_tenant"];

#[sqlx::test]
async fn the_runtime_roles_have_exactly_their_session_defaults(pool: PgPool) -> sqlx::Result<()> {
    // Role settings apply at login, not after SET ROLE, so the behaviour
    // tests never see them (ADR-0005). The lock timeout bounds a retried
    // posting's wait, and outlasts the idle timeout, so the retry outlives a
    // first attempt left idle after a lost COMMIT. The session timeout ends a
    // session that leaked a session-level lock (ADR-0018). "*" means every
    // database.
    for role in LOGIN_ROLES {
        let mut settings = sqlx::query_scalar!(
            r#"
            SELECT format('%s %s', COALESCE(d.datname, '*'), c.setting) AS "setting!"
              FROM pg_db_role_setting s
              JOIN pg_roles r ON r.oid = s.setrole
              LEFT JOIN pg_database d ON d.oid = s.setdatabase
             CROSS JOIN unnest(s.setconfig) AS c (setting)
             WHERE r.rolname = $1::text
            "#,
            role
        )
        .fetch_all(&pool)
        .await?;
        settings.sort();
        assert_eq!(
            settings,
            [
                "* idle_in_transaction_session_timeout=30s",
                "* idle_session_timeout=30min",
                "* lock_timeout=35s",
            ],
            "{role}"
        );
    }
    Ok(())
}

#[sqlx::test]
async fn a_runtime_login_gets_those_defaults_at_read_committed(pool: PgPool) -> sqlx::Result<()> {
    // A real login, not SET ROLE, so this is what the application gets. No code
    // sets an isolation level, and a command relies on this one: its read comes
    // after the lock, and sees what the lock's previous holder committed
    // (ADR-0018).
    for role in LOGIN_ROLES {
        let options = (*pool.connect_options())
            .clone()
            .username(role)
            .password(role);
        let mut conn = PgConnection::connect_with(&options).await?;
        let row = sqlx::query!(
            r#"SELECT current_user::text AS "user!",
                      current_setting('transaction_isolation') AS "isolation!",
                      current_setting('lock_timeout') AS "lock_timeout!",
                      current_setting('idle_in_transaction_session_timeout')
                          AS "idle_in_transaction!",
                      current_setting('idle_session_timeout') AS "idle_session!""#
        )
        .fetch_one(&mut conn)
        .await?;
        assert_eq!(row.user, role);
        assert_eq!(row.isolation, "read committed", "{role}");
        assert_eq!(row.lock_timeout, "35s", "{role}");
        assert_eq!(row.idle_in_transaction, "30s", "{role}");
        assert_eq!(row.idle_session, "30min", "{role}");
        conn.close().await?;
    }
    Ok(())
}

#[sqlx::test]
async fn every_guard_trigger_fires_always(pool: PgPool) -> sqlx::Result<()> {
    // ENABLE ALWAYS: the triggers fire even under
    // session_replication_role = replica (ADR-0004). Internal triggers, which
    // implement foreign keys, are not listed.
    let rows = sqlx::query!(
        r#"
        SELECT t.tgrelid::regclass::text AS "table!",
               t.tgname::text AS "trigger!",
               t.tgenabled::text AS "enabled!",
               p.proname::text AS "function!"
          FROM pg_trigger t
          JOIN pg_proc p ON p.oid = t.tgfoid
         WHERE NOT t.tgisinternal
        "#
    )
    .fetch_all(&pool)
    .await?;
    let mut triggers: Vec<[String; 4]> = rows
        .into_iter()
        .map(|r| [r.table, r.trigger, r.enabled, r.function])
        .collect();
    triggers.sort();

    let expected: Vec<[String; 4]> = [
        (
            "account_kinds",
            "account_kinds_append_only",
            "reject_change",
        ),
        (
            "account_kinds",
            "account_kinds_no_truncate",
            "reject_change",
        ),
        ("accounts", "accounts_append_only", "reject_change"),
        ("accounts", "accounts_no_truncate", "reject_change"),
        (
            "api_key_revocations",
            "api_key_revocations_append_only",
            "reject_change",
        ),
        (
            "api_key_revocations",
            "api_key_revocations_no_truncate",
            "reject_change",
        ),
        ("api_keys", "api_keys_append_only", "reject_change"),
        ("api_keys", "api_keys_no_truncate", "reject_change"),
        ("currencies", "currencies_append_only", "reject_change"),
        ("currencies", "currencies_no_truncate", "reject_change"),
        (
            "idempotency_records",
            "idempotency_records_append_only",
            "reject_change",
        ),
        (
            "idempotency_records",
            "idempotency_records_no_truncate",
            "reject_change",
        ),
        (
            "ledger_entries",
            "ledger_entries_append_only",
            "reject_change",
        ),
        (
            "ledger_entries",
            "ledger_entries_no_truncate",
            "reject_change",
        ),
        (
            "ledger_transactions",
            "ledger_transactions_append_only",
            "reject_change",
        ),
        (
            "ledger_transactions",
            "ledger_transactions_commit_check",
            "check_ledger_transaction",
        ),
        (
            "ledger_transactions",
            "ledger_transactions_no_truncate",
            "reject_change",
        ),
        ("merchants", "merchants_append_only", "reject_change"),
        ("merchants", "merchants_no_truncate", "reject_change"),
        (
            "operator_idempotency_records",
            "operator_idempotency_records_append_only",
            "reject_change",
        ),
        (
            "operator_idempotency_records",
            "operator_idempotency_records_no_truncate",
            "reject_change",
        ),
        (
            "payment_authorizations",
            "payment_authorizations_append_only",
            "reject_change",
        ),
        (
            "payment_authorizations",
            "payment_authorizations_no_truncate",
            "reject_change",
        ),
        (
            "payment_captures",
            "payment_captures_append_only",
            "reject_change",
        ),
        (
            "payment_captures",
            "payment_captures_commit_check",
            "check_payment_posting",
        ),
        (
            "payment_captures",
            "payment_captures_no_truncate",
            "reject_change",
        ),
        (
            "payment_facts",
            "payment_facts_append_only",
            "reject_change",
        ),
        (
            "payment_facts",
            "payment_facts_no_truncate",
            "reject_change",
        ),
        (
            "payment_refunds",
            "payment_refunds_append_only",
            "reject_change",
        ),
        (
            "payment_refunds",
            "payment_refunds_commit_check",
            "check_payment_posting",
        ),
        (
            "payment_refunds",
            "payment_refunds_no_truncate",
            "reject_change",
        ),
        (
            "payment_request_answers",
            "payment_request_answers_append_only",
            "reject_change",
        ),
        (
            "payment_request_answers",
            "payment_request_answers_no_truncate",
            "reject_change",
        ),
        (
            "payment_request_decisions",
            "payment_request_decisions_append_only",
            "reject_change",
        ),
        (
            "payment_request_decisions",
            "payment_request_decisions_no_truncate",
            "reject_change",
        ),
        (
            "payment_requests",
            "payment_requests_append_only",
            "reject_change",
        ),
        (
            "payment_requests",
            "payment_requests_no_truncate",
            "reject_change",
        ),
        (
            "payment_stops",
            "payment_stops_append_only",
            "reject_change",
        ),
        (
            "payment_stops",
            "payment_stops_no_truncate",
            "reject_change",
        ),
        (
            "payment_transition_facts",
            "payment_transition_facts_append_only",
            "reject_change",
        ),
        (
            "payment_transition_facts",
            "payment_transition_facts_no_truncate",
            "reject_change",
        ),
        (
            "payment_transitions",
            "payment_transitions_append_only",
            "reject_change",
        ),
        (
            "payment_transitions",
            "payment_transitions_commit_check",
            "check_payment_step_record",
        ),
        (
            "payment_transitions",
            "payment_transitions_fact_check",
            "check_payment_step_fact",
        ),
        (
            "payment_transitions",
            "payment_transitions_no_truncate",
            "reject_change",
        ),
        ("payments", "payments_append_only", "reject_change"),
        ("payments", "payments_commit_check", "check_payment_genesis"),
        ("payments", "payments_no_truncate", "reject_change"),
        (
            "psp_notifications",
            "psp_notifications_append_only",
            "reject_change",
        ),
        (
            "psp_notifications",
            "psp_notifications_no_truncate",
            "reject_change",
        ),
    ]
    .into_iter()
    .map(|(table, trigger, function)| [table.into(), trigger.into(), "A".into(), function.into()])
    .collect();
    assert_eq!(triggers, expected);
    Ok(())
}

#[sqlx::test]
async fn every_guard_function_has_a_pinned_search_path(pool: PgPool) -> sqlx::Result<()> {
    // The pinned search_path keeps the session's temporary objects from
    // standing in for the tables a guard reads (ADR-0004). Owning a function
    // would let the runtime role reset it, or drop it and its triggers.
    let rows = sqlx::query!(
        r#"
        SELECT p.proname::text AS "function!",
               COALESCE(array_to_string(p.proconfig, '; '), '') AS "config!",
               p.prosecdef AS "security_definer!",
               pg_has_role('payments_app', p.proowner, 'MEMBER') AS "runtime_role_owns!"
          FROM pg_proc p
         WHERE p.oid IN (SELECT tgfoid FROM pg_trigger WHERE NOT tgisinternal)
        "#
    )
    .fetch_all(&pool)
    .await?;
    let mut functions: Vec<(String, String, bool, bool)> = rows
        .into_iter()
        .map(|r| {
            (
                r.function,
                r.config,
                r.security_definer,
                r.runtime_role_owns,
            )
        })
        .collect();
    functions.sort();

    let pinned = "search_path=pg_catalog, public, pg_temp";
    assert_eq!(
        functions,
        [
            (
                "check_ledger_transaction".to_owned(),
                pinned.to_owned(),
                false,
                false
            ),
            (
                "check_payment_genesis".to_owned(),
                pinned.to_owned(),
                false,
                false
            ),
            (
                "check_payment_posting".to_owned(),
                pinned.to_owned(),
                false,
                false
            ),
            (
                "check_payment_step_fact".to_owned(),
                pinned.to_owned(),
                false,
                false
            ),
            (
                "check_payment_step_record".to_owned(),
                pinned.to_owned(),
                false,
                false
            ),
            ("reject_change".to_owned(), pinned.to_owned(), false, false),
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn every_column_has_its_type_and_is_not_null(pool: PgPool) -> sqlx::Result<()> {
    // A NULL slips past foreign keys, CHECKs and sum() (ADR-0003), and a
    // column of the wrong type can hold what its CHECKs never anticipated,
    // such as a fraction of a minor unit. The sqlx bookkeeping table is left
    // out.
    let mut columns = sqlx::query_scalar!(
        r#"
        SELECT format('%s.%s %s%s', c.relname, a.attname,
                      format_type(a.atttypid, a.atttypmod),
                      CASE WHEN a.attnotnull THEN ' not null' ELSE '' END) AS "column!"
          FROM pg_attribute a
          JOIN pg_class c ON c.oid = a.attrelid
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND c.relname <> '_sqlx_migrations'
           AND a.attnum > 0
           AND NOT a.attisdropped
        "#
    )
    .fetch_all(&pool)
    .await?;
    columns.sort();

    assert_eq!(
        columns,
        [
            "account_kinds.kind text not null",
            "account_kinds.normal_side text not null",
            "accounts.currency currency_code not null",
            "accounts.id uuid not null",
            "accounts.kind text not null",
            "accounts.normal_side text not null",
            "accounts.party text not null",
            "accounts.recorded_at timestamp with time zone not null",
            "api_key_revocations.api_key_id uuid not null",
            "api_key_revocations.recorded_at timestamp with time zone not null",
            "api_keys.digest bytea not null",
            "api_keys.id uuid not null",
            "api_keys.merchant_id uuid not null",
            "api_keys.recorded_at timestamp with time zone not null",
            "currencies.code currency_code not null",
            "currencies.decimals smallint not null",
            "idempotency_records.body text not null",
            "idempotency_records.fingerprint bytea not null",
            "idempotency_records.key text not null",
            "idempotency_records.merchant_id uuid not null",
            "idempotency_records.recorded_at timestamp with time zone not null",
            "idempotency_records.status smallint not null",
            "ledger_entries.account_id uuid not null",
            "ledger_entries.amount bigint not null",
            "ledger_entries.currency currency_code not null",
            "ledger_entries.entry_count integer not null",
            "ledger_entries.line_no integer not null",
            "ledger_entries.transaction_id uuid not null",
            "ledger_transactions.currency currency_code not null",
            "ledger_transactions.entry_count integer not null",
            "ledger_transactions.id uuid not null",
            "ledger_transactions.recorded_at timestamp with time zone not null",
            "merchants.fee_bps integer not null",
            "merchants.id uuid not null",
            "merchants.recorded_at timestamp with time zone not null",
            "operator_idempotency_records.body text not null",
            "operator_idempotency_records.fingerprint bytea not null",
            "operator_idempotency_records.key text not null",
            "operator_idempotency_records.recorded_at timestamp with time zone not null",
            "operator_idempotency_records.status smallint not null",
            "payment_authorizations.amount bigint not null",
            "payment_authorizations.expires_at timestamp with time zone not null",
            "payment_authorizations.kind text not null",
            "payment_authorizations.payment_amount bigint not null",
            "payment_authorizations.payment_id uuid not null",
            "payment_authorizations.transition_id uuid not null",
            "payment_captures.amount bigint not null",
            "payment_captures.authorized_amount bigint not null",
            "payment_captures.captured_at timestamp with time zone not null",
            "payment_captures.fee bigint not null",
            "payment_captures.kind text not null",
            "payment_captures.ledger_transaction_id uuid not null",
            "payment_captures.payment_id uuid not null",
            "payment_captures.transition_id uuid not null",
            "payment_facts.amount bigint not null",
            "payment_facts.kind text not null",
            "payment_facts.payment_id uuid not null",
            "payment_facts.position integer not null",
            "payment_facts.prev_position integer not null",
            "payment_facts.psp text not null",
            "payment_facts.psp_fact_id text not null",
            "payment_facts.recorded_at timestamp with time zone not null",
            "payment_refunds.amount bigint not null",
            "payment_refunds.kind text not null",
            "payment_refunds.ledger_transaction_id uuid not null",
            "payment_refunds.payment_id uuid not null",
            "payment_refunds.refunded_at timestamp with time zone not null",
            "payment_refunds.transition_id uuid not null",
            "payment_request_answers.answer text not null",
            "payment_request_answers.payment_id uuid not null",
            "payment_request_answers.recorded_at timestamp with time zone not null",
            "payment_request_answers.request_id uuid not null",
            "payment_request_decisions.fact_kind text not null",
            "payment_request_decisions.payment_id uuid not null",
            "payment_request_decisions.position integer not null",
            "payment_request_decisions.request_id uuid not null",
            "payment_request_decisions.request_kind text not null",
            "payment_requests.amount bigint not null",
            "payment_requests.id uuid not null",
            "payment_requests.kind text not null",
            "payment_requests.payment_id uuid not null",
            "payment_requests.recorded_at timestamp with time zone not null",
            "payment_stops.payment_id uuid not null",
            "payment_stops.position integer not null",
            "payment_stops.reason text not null",
            "payment_stops.recorded_at timestamp with time zone not null",
            "payment_transition_facts.amount bigint not null",
            "payment_transition_facts.kind text not null",
            "payment_transition_facts.payment_id uuid not null",
            "payment_transition_facts.position integer not null",
            "payment_transition_facts.transition_id uuid not null",
            "payment_transitions.amount bigint not null",
            "payment_transitions.from_status text not null",
            "payment_transitions.id uuid not null",
            "payment_transitions.kind text not null",
            "payment_transitions.payment_id uuid not null",
            "payment_transitions.prev_refundable bigint not null",
            "payment_transitions.prev_seq integer not null",
            "payment_transitions.recorded_at timestamp with time zone not null",
            "payment_transitions.refundable bigint not null",
            "payment_transitions.seq integer not null",
            "payment_transitions.to_status text not null",
            "payments.amount bigint not null",
            "payments.currency currency_code not null",
            "payments.fee_bps integer not null",
            "payments.id uuid not null",
            "payments.merchant_id uuid not null",
            "payments.psp text not null",
            "payments.recorded_at timestamp with time zone not null",
            "psp_notifications.event_id text not null",
            "psp_notifications.payment_id uuid not null",
            "psp_notifications.psp text not null",
            "psp_notifications.recorded_at timestamp with time zone not null",
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn every_constraint_keeps_its_name(pool: PgPool) -> sqlx::Result<()> {
    // A test that breaks a rule asserts its constraint's name, but only for the
    // rules it breaks, so a migration that drops another would pass them all
    // (ADR-0017). A NOT NULL has a row here too, and the test above pins those.
    // The sqlx bookkeeping table's key is left out.
    let mut constraints = sqlx::query_scalar!(
        r#"
        SELECT conname::text AS "constraint!"
          FROM pg_constraint
         WHERE connamespace = 'public'::regnamespace
           AND contype <> 'n'
           AND conname <> '_sqlx_migrations_pkey'
        "#
    )
    .fetch_all(&pool)
    .await?;
    constraints.sort();

    assert_eq!(
        constraints,
        [
            "account_kinds_kind_check",
            "account_kinds_kind_normal_side_key",
            "account_kinds_normal_side_check",
            "account_kinds_pkey",
            "accounts_currency_fkey",
            "accounts_id_check",
            "accounts_id_currency_key",
            "accounts_kind_check",
            "accounts_kind_normal_side_fkey",
            "accounts_kind_party_currency_key",
            "accounts_normal_side_check",
            "accounts_party_check",
            "accounts_pkey",
            "api_key_revocations_api_key_fkey",
            "api_key_revocations_pkey",
            "api_keys_digest_check",
            "api_keys_digest_key",
            "api_keys_id_check",
            "api_keys_merchant_fkey",
            "api_keys_pkey",
            "currencies_decimals_check",
            "currencies_pkey",
            "currency_code_check",
            "idempotency_records_body_check",
            "idempotency_records_fingerprint_check",
            "idempotency_records_key_check",
            "idempotency_records_merchant_fkey",
            "idempotency_records_pkey",
            "idempotency_records_status_check",
            "ledger_entries_account_fkey",
            "ledger_entries_amount_check",
            "ledger_entries_header_fkey",
            "ledger_entries_line_no_check",
            "ledger_entries_pkey",
            "ledger_transactions_commit_check",
            "ledger_transactions_entry_count_check",
            "ledger_transactions_id_check",
            "ledger_transactions_id_entry_count_currency_key",
            "ledger_transactions_pkey",
            "merchants_fee_bps_check",
            "merchants_id_check",
            "merchants_pkey",
            "operator_idempotency_records_body_check",
            "operator_idempotency_records_fingerprint_check",
            "operator_idempotency_records_key_check",
            "operator_idempotency_records_pkey",
            "operator_idempotency_records_status_check",
            "payment_authorizations_amount_check",
            "payment_authorizations_expires_at_check",
            "payment_authorizations_payment_fkey",
            "payment_authorizations_payment_id_amount_key",
            "payment_authorizations_pkey",
            "payment_authorizations_step_fkey",
            "payment_captures_amount_check",
            "payment_captures_authorization_fkey",
            "payment_captures_captured_at_check",
            "payment_captures_commit_check",
            "payment_captures_fee_check",
            "payment_captures_ledger_transaction_fkey",
            "payment_captures_ledger_transaction_id_key",
            "payment_captures_pkey",
            "payment_captures_step_fkey",
            "payment_facts_amount_check",
            "payment_facts_kind_amount_key",
            "payment_facts_kind_check",
            "payment_facts_kind_key",
            "payment_facts_payment_fkey",
            "payment_facts_pkey",
            "payment_facts_position_check",
            "payment_facts_prev_fkey",
            "payment_facts_psp_fact_id_check",
            "payment_facts_psp_fact_id_key",
            "payment_refunds_commit_check",
            "payment_refunds_ledger_transaction_fkey",
            "payment_refunds_ledger_transaction_id_key",
            "payment_refunds_pkey",
            "payment_refunds_refunded_at_check",
            "payment_refunds_step_fkey",
            "payment_request_answers_answer_check",
            "payment_request_answers_pkey",
            "payment_request_answers_request_fkey",
            "payment_request_decisions_fact_fkey",
            "payment_request_decisions_fact_key",
            "payment_request_decisions_kinds_check",
            "payment_request_decisions_pkey",
            "payment_request_decisions_request_fkey",
            "payment_requests_amount_check",
            "payment_requests_id_check",
            "payment_requests_id_payment_id_key",
            "payment_requests_id_payment_id_kind_key",
            "payment_requests_kind_check",
            "payment_requests_payment_fkey",
            "payment_requests_pkey",
            "payment_stops_payment_fkey",
            "payment_stops_pkey",
            "payment_stops_position_check",
            "payment_stops_reason_check",
            "payment_transition_facts_fact_fkey",
            "payment_transition_facts_fact_key",
            "payment_transition_facts_pkey",
            "payment_transition_facts_step_fkey",
            "payment_transitions_commit_check",
            "payment_transitions_edge_check",
            "payment_transitions_fact_check",
            "payment_transitions_id_check",
            "payment_transitions_id_key",
            "payment_transitions_money_check",
            "payment_transitions_payment_fkey",
            "payment_transitions_pkey",
            "payment_transitions_prev_fkey",
            "payment_transitions_seq_check",
            "payment_transitions_state_key",
            "payment_transitions_step_key",
            "payments_amount_check",
            "payments_commit_check",
            "payments_fee_bps_check",
            "payments_id_amount_key",
            "payments_id_check",
            "payments_id_psp_key",
            "payments_merchant_fkey",
            "payments_pkey",
            "payments_psp_check",
            "psp_notifications_event_id_check",
            "psp_notifications_pkey",
            "psp_notifications_psp_check",
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn row_level_security_covers_exactly_what_a_merchant_owns(pool: PgPool) -> sqlx::Result<()> {
    // Dropping a policy leaves the tenant role seeing nothing, which fails closed, but
    // disabling row-level security on a table would open it to every merchant. Nothing
    // is forced, because only migrations run as the owner (ADR-0025).
    let mut tables = sqlx::query_scalar!(
        r#"
        SELECT format('%s%s', c.relname,
                      CASE WHEN c.relforcerowsecurity THEN ' forced' ELSE '' END) AS "table!"
          FROM pg_class c
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND c.relrowsecurity
        "#
    )
    .fetch_all(&pool)
    .await?;
    tables.sort();
    assert_eq!(
        tables,
        [
            "idempotency_records",
            "merchants",
            "payment_authorizations",
            "payment_captures",
            "payment_facts",
            "payment_refunds",
            "payment_request_answers",
            "payment_request_decisions",
            "payment_requests",
            "payment_stops",
            "payment_transition_facts",
            "payment_transitions",
            "payments",
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn every_policy_keeps_its_text(pool: PgPool) -> sqlx::Result<()> {
    // A policy that went on to compare the wrong column, or that applied to another
    // role, would pass every test that its own merchant runs. With no WITH CHECK,
    // USING also judges new rows, and an INSERT policy has only its check. PostgreSQL
    // stores a policy parsed, so this is its text as PostgreSQL prints it, with the
    // whitespace of its line breaks reduced.
    let mut policies = sqlx::query_scalar!(
        r#"
        SELECT format('%s %s %s %s: %s; check: %s', policyname, permissive, cmd, roles::text,
                      COALESCE(regexp_replace(qual, '\s+', ' ', 'g'), 'none'),
                      COALESCE(with_check, 'none')) AS "policy!"
          FROM pg_policies
         WHERE schemaname = 'public'
        "#
    )
    .fetch_all(&pool)
    .await?;
    policies.sort();
    let of_a_child = |table: &str, name: &str, command: &str| {
        format!(
            "{name} PERMISSIVE {command} {{payments_tenant}}: \
             (EXISTS ( SELECT 1 FROM payments p WHERE (p.id = {table}.payment_id))); check: none"
        )
    };
    let setting = "(NULLIF(current_setting('app.merchant'::text, true), ''::text))::uuid";
    assert_eq!(
        policies,
        [
            format!(
                "idempotency_records_tenant_isolation PERMISSIVE ALL {{payments_tenant}}: \
                 (merchant_id = {setting}); check: none"
            ),
            format!(
                "merchants_tenant_isolation PERMISSIVE SELECT {{payments_tenant}}: \
                 (id = {setting}); check: none"
            ),
            of_a_child(
                "payment_authorizations",
                "payment_authorizations_tenant_isolation",
                "ALL"
            ),
            of_a_child(
                "payment_captures",
                "payment_captures_tenant_isolation",
                "ALL"
            ),
            // What the PSP answered and decided, a merchant only reads (ADR-0033).
            of_a_child("payment_facts", "payment_facts_tenant_isolation", "SELECT"),
            of_a_child("payment_refunds", "payment_refunds_tenant_isolation", "ALL"),
            of_a_child(
                "payment_request_answers",
                "payment_request_answers_tenant_isolation",
                "SELECT"
            ),
            of_a_child(
                "payment_request_decisions",
                "payment_request_decisions_tenant_isolation",
                "SELECT"
            ),
            of_a_child(
                "payment_requests",
                "payment_requests_tenant_isolation",
                "ALL"
            ),
            // A stop refuses a merchant's request, so the merchant reads it (ADR-0042).
            of_a_child("payment_stops", "payment_stops_tenant_isolation", "SELECT"),
            // A merchant follows a request to the record of its step (ADR-0045).
            of_a_child(
                "payment_transition_facts",
                "payment_transition_facts_tenant_isolation",
                "SELECT"
            ),
            of_a_child(
                "payment_transitions",
                "payment_transitions_tenant_isolation",
                "ALL"
            ),
            // Restrictive, so a new step must pass it as well as the policy above; for
            // INSERT only, so the merchant still reads every step of its payments
            // (ADR-0034).
            "payment_transitions_tenant_steps RESTRICTIVE INSERT {payments_tenant}: none; \
             check: (kind = ANY (ARRAY['create'::text, 'cancel'::text]))"
                .to_owned(),
            format!(
                "payments_tenant_isolation PERMISSIVE ALL {{payments_tenant}}: \
                 (merchant_id = {setting}); check: none"
            ),
        ]
    );
    Ok(())
}
