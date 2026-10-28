<!-- Author: Nicola Antonello -->

# ADR-0008: Provisioning the database roles

- Status: accepted
- Date: 2026-10-14

## Context

ADR-0004 separates an owner role, which runs migrations and owns the tables,
from a runtime role, which the application uses. Roles are cluster-wide in
PostgreSQL: one role is shared by every database in the server. Migrations,
by contrast, run once per database, and `#[sqlx::test]` creates a fresh
database for every test, connecting as a superuser to do so.

Since PostgreSQL 15, only the owner of a database may create objects in its
`public` schema: the schema belongs to `pg_database_owner`, and everyone else
gets `USAGE` only. Test databases are created, and therefore owned, by the
superuser.

## Options

| Option | Why not |
|---|---|
| **A SQL script run when the server is set up: by Docker Compose on first start, and by a CI step** | Chosen |
| Roles created in a migration, skipped if they exist | Parallel tests race to create the same cluster-wide role, the migration role needs the right to create roles, and passwords end up in migrations |
| Roles created by the test harness before the tests | More moving parts, and local development and CI still need their own setup |
| A separate owner role in development too, with `CREATE` on `public` granted in `template1` so new databases inherit it | Works, but hides a privilege in the template database, to make tests mimic a deployment detail |

## Decision

- `docker/postgres/roles.sql` creates the runtime role `payments_app`: it
  can log in, is not a superuser, cannot create databases or roles, and
  belongs to no other role. It also sets
  `idle_in_transaction_session_timeout = '30s'` on that role, so a session
  left inside an open transaction after a lost `COMMIT` releases its locks
  within 30 seconds (ADR-0005). No transaction of this project waits for
  anything outside PostgreSQL while it is open, so 30 seconds is far above
  any legitimate pause.
- Docker Compose mounts the script into `/docker-entrypoint-initdb.d/`, which
  the image runs once, when it starts with an empty data directory. The CI
  test job runs the same file inside the PostgreSQL service container after
  checkout.
- Migrations create tables and triggers and grant the runtime role its
  privileges (ADR-0004); they never create roles.
- In development and CI, the bootstrap superuser created by the image
  (`payments`) runs the migrations and owns the tables, because
  `#[sqlx::test]` needs a superuser to create its databases. A deployment
  would run migrations as a dedicated owner role that is not a superuser and
  owns its database.

## Consequences

- The runtime role exists before any migration or test runs, so parallel
  tests never race to create it, and no migration needs the right to create
  roles.
- In development and CI the owner is a superuser. Tests that need "the
  owner" (ADR-0004, the I2 trigger test) run as that superuser, which also
  holds every privilege, so the trigger is still what stops them.
- Init scripts run only on an empty data directory: after a change to
  `roles.sql`, the local volume must be recreated with
  `docker compose down -v`.
- The script holds a password for local use only, like the one in
  `docker-compose.yml`.

## Amendments

- 2026-10-20: `roles.sql` also sets `lock_timeout = '35s'` on the runtime
  role, so that a retried posting waiting for a first attempt's rows
  outlasts the 30-second idle timeout (ADR-0005). An existing local volume
  picks it up after `docker compose down -v`, as for any change to the
  script.
- 2026-10-28: `roles.sql` also sets `idle_session_timeout = '30min'` on the
  runtime role, so that a session-level advisory lock leaked by hand-written
  SQL ends with its session (ADR-0018). The limit sits above the longest
  time sqlx's pool keeps a connection idle, and a catalog test pins it with
  the other two settings. An existing local volume picks it up after
  `docker compose down -v`.

## References

- PostgreSQL 18 manual, [Schemas](https://www.postgresql.org/docs/18/ddl-schemas.html) (privileges on `public`).
- PostgreSQL 18 manual, [Database Roles](https://www.postgresql.org/docs/18/user-manag.html).
- Docker Official Images, [postgres](https://hub.docker.com/_/postgres) (initialization scripts).
