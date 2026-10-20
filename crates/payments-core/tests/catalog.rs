// Author: Nicola Antonello

//! Catalog tests: what the behavioural tests cannot see (ADR-0003,
//! ADR-0004, ADR-0012).
//!
//! `#[sqlx::test]` connects as a superuser, which skips privilege checks, so
//! only the tests that switch to the runtime role, `payments_app`, meet
//! them, and those notice only the grants they exercise. These tests read
//! the catalogs instead. They pin the runtime role's table and column
//! privileges and its grant options, check that it can neither create
//! objects in the public schema nor act through another role, pin each guard
//! trigger and the function it runs, and pin the type and nullability of
//! every column.

use sqlx::PgPool;

#[sqlx::test]
async fn the_runtime_role_has_exactly_the_expected_privileges(pool: PgPool) -> sqlx::Result<()> {
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
           AND has_table_privilege('payments_app', c.oid, p.privilege)
        UNION ALL
        SELECT format('%s %s WITH GRANT OPTION', c.relname, p.privilege)
          FROM pg_class c
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE',
                                 'REFERENCES', 'TRIGGER', 'MAINTAIN']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND has_table_privilege('payments_app', c.oid, p.privilege || ' WITH GRANT OPTION')
        UNION ALL
        SELECT format('%s.%s %s', c.relname, a.attname, p.privilege)
          FROM pg_attribute a
          JOIN pg_class c ON c.oid = a.attrelid
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'REFERENCES']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND a.attnum > 0
           AND NOT a.attisdropped
           AND has_column_privilege('payments_app', c.oid, a.attnum, p.privilege)
           AND NOT has_table_privilege('payments_app', c.oid, p.privilege)
        UNION ALL
        SELECT format('%s.%s %s WITH GRANT OPTION', c.relname, a.attname, p.privilege)
          FROM pg_attribute a
          JOIN pg_class c ON c.oid = a.attrelid
         CROSS JOIN unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'REFERENCES']) AS p (privilege)
         WHERE c.relnamespace = 'public'::regnamespace
           AND c.relkind = 'r'
           AND a.attnum > 0
           AND NOT a.attisdropped
           AND has_column_privilege('payments_app', c.oid, a.attnum,
                                    p.privilege || ' WITH GRANT OPTION')
        "#
    )
    .fetch_all(&pool)
    .await?;
    granted.sort();

    // INSERT covers every column of accounts except recorded_at, which only
    // the database sets (ADR-0012).
    assert_eq!(
        granted,
        [
            "accounts SELECT",
            "accounts.currency INSERT",
            "accounts.id INSERT",
            "accounts.kind INSERT",
            "accounts.normal_side INSERT",
            "accounts.party INSERT",
            "currencies SELECT",
        ]
    );
    Ok(())
}

#[sqlx::test]
async fn the_runtime_role_cannot_switch_the_guards_off(pool: PgPool) -> sqlx::Result<()> {
    // A superuser can disable any trigger. A member of another role can act as
    // it, even through SET ROLE without inheriting its privileges. CREATE on
    // public would let the role add an operator that the guard functions'
    // search_path finds. A role allowed to set session_replication_role can
    // switch off foreign-key checks (ADR-0004).
    let role = sqlx::query!(
        r#"
        SELECT rolsuper AS "superuser!",
               EXISTS (SELECT 1 FROM pg_auth_members WHERE member = r.oid)
                   AS "member_of_a_role!",
               has_schema_privilege('payments_app', 'public', 'CREATE')
                   AS "can_create_in_public!",
               has_parameter_privilege('payments_app', 'session_replication_role', 'SET')
                   AS "can_set_replication_role!"
          FROM pg_roles r
         WHERE r.rolname = 'payments_app'
        "#
    )
    .fetch_one(&pool)
    .await?;
    assert!(!role.superuser, "payments_app is a superuser");
    assert!(
        !role.member_of_a_role,
        "payments_app can act as another role"
    );
    assert!(
        !role.can_create_in_public,
        "payments_app can create objects in public"
    );
    assert!(
        !role.can_set_replication_role,
        "payments_app can set session_replication_role"
    );
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
        ("accounts", "accounts_append_only", "reject_change"),
        ("accounts", "accounts_no_truncate", "reject_change"),
        ("currencies", "currencies_append_only", "reject_change"),
        ("currencies", "currencies_no_truncate", "reject_change"),
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
        [("reject_change".to_owned(), pinned.to_owned(), false, false)]
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
            "accounts.currency currency_code not null",
            "accounts.id uuid not null",
            "accounts.kind text not null",
            "accounts.normal_side text not null",
            "accounts.party text not null",
            "accounts.recorded_at timestamp with time zone not null",
            "currencies.code currency_code not null",
            "currencies.decimals smallint not null",
        ]
    );
    Ok(())
}
