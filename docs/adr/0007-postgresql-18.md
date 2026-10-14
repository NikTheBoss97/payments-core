<!-- Author: Nicola Antonello -->

# ADR-0007: PostgreSQL 18

- Status: accepted
- Date: 2026-10-14

## Context

The project was scaffolded on PostgreSQL 17. PostgreSQL 18 is the current
major release (first released 2025-09-25, now at 18.6). No data exists yet,
so changing the major version costs almost nothing now and a data migration
later.

## Options

| Option | Why not |
|---|---|
| **PostgreSQL 18** | Chosen |
| PostgreSQL 17 | One major version behind: supported a year less (until November 2029 rather than November 2030), and phase-8 vacuum tuning would have to be redone after an upgrade (see Consequences). It also lacks `uuidv7()` and UUIDv7 decoding for ad-hoc SQL |
| PostgreSQL 19 | Not yet released: general availability is scheduled for 2026-11-19, after release candidates on 2026-10-15 and 2026-11-12; a `.0` release is a weaker base than a mature 18.x |

## Decision

Use the `postgres:18-alpine` image for local development (Docker Compose)
and in CI, and link to the version 18 manual in the documentation.

## Consequences

- `uuidv7()` and a UUIDv7-aware `uuid_extract_timestamp()` are available
  for ad-hoc SQL and debugging, even though the application generates IDs
  itself (ADR-0005).
- From version 18 the image keeps its data in `/var/lib/postgresql/18/docker`
  and declares its volume at `/var/lib/postgresql`. The Compose file
  therefore mounts the named volume at `/var/lib/postgresql` instead of
  `/var/lib/postgresql/data`, and existing local volumes must be recreated
  (`docker compose down -v`).
- PostgreSQL 18 changed the insert threshold that triggers autovacuum: it
  now scales with the part of the table that vacuum has not yet frozen
  (frozen pages need no further vacuuming unless they change) instead of
  with the whole table, so a large, mostly frozen table is vacuumed after far
  fewer inserts. Vacuum is also what marks pages all-visible, so this changes
  how often balance reads can skip the table (ADR-0002). Tuning happens in
  phase 8, on version 18.

## References

- PostgreSQL, [Versioning policy](https://www.postgresql.org/support/versioning/).
- Jonathan S. Katz, [Updated PostgreSQL RC / GA dates](https://www.postgresql.org/message-id/f161fbb7-e193-42eb-bec7-4cfc1af74a50%40postgresql.org), pgsql-hackers, 2026-10-10.
- PostgreSQL 18 manual, [Routine Vacuuming](https://www.postgresql.org/docs/18/routine-vacuuming.html).
- Docker Official Images, [postgres](https://hub.docker.com/_/postgres).
