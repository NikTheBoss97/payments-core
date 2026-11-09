<!-- Author: Nicola Antonello -->

# ADR-0040: The local server's limit and parallel runs

- Status: accepted
- Date: 2026-11-09

## Context

The tests that need PostgreSQL run against the server that Docker Compose starts
(ADR-0007). It keeps the default limit of 100 connections, of which the last 3 are kept
for superusers, so the two login roles (ADR-0025) are refused once 97 are open. The test
harness runs a binary's tests in parallel, one thread per hardware thread: 32 on the
development machine, 4 on CI's runner.

sqlx caps its own test connections at 20 per process: every test's pool borrows its
permits from one shared pool. Outside that cap are sqlx's connection that migrates each
test's database, the two login pools of every HTTP test, runtime connections detached
from a pool, direct logins, and the two pools of the server binary. Measured on
2026-11-09 against a copy of the Compose server, counting its backends ten times a
second, one `cargo test --workspace` peaks at 99 connections at once, and two runs
together at 165. At 100 slots, one run met 2 refusals and two runs met 844.

A pool retries a refused connection (SQLSTATE `53300`) for up to 30 seconds, so most
refusals only slow a run. The connection that migrates a test's database tries once, so
a refusal there fails a test whose code is not at fault. A test failed once on
2026-11-08 with refusals logged in the same second, and this project treats such a
failure as a race until something explains it.

Since sqlx 0.8.5 (PR #3334), a test's database is named after the test, the same name
in every process. Two runs of the same tests against one server collide: one drops or
recreates the other's databases. At 1000 slots, two full runs at once still failed 29
tests, every one this way, so no limit prevents it, and more than one checkout of this
repository may run the suite at a time.

## Options

The limit:

| Option | Why not |
|---|---|
| **Raise the Compose server's limit to 300** | Chosen |
| Run fewer tests at once: `RUST_TEST_THREADS=8` through `.cargo/config.toml` | No refusal and a peak of 55, but a run took 135 seconds instead of 99, unit tests included, and two runs at once would pass 100 again. CI's runner would run 8 threads on 4 CPUs. The rustc book marks the variable deprecated, and Cargo documents `[env]` only for build scripts, `rustc`, `cargo run` and `cargo build` |
| Fewer connections per test: smaller pools in the test helpers, or a limit on tests in flight | The race tests send 8 copies at once and need 8 connections to race at all. A limit on tests in flight is the option above with more code |

A second run at the same time:

| Option | Why not |
|---|---|
| **The host port from `POSTGRES_HOST_PORT`, 5432 by default, and another project name** | Chosen |
| A script that runs a command against a throwaway server, then removes it | One command that leaves nothing behind, but a shell script to keep working on Windows and Linux, and seconds of setup on every use |
| A second, fixed server in the Compose file | One more server only, on a fixed port, and removed by hand all the same |

## Decision

- **The limit.** The Compose service starts PostgreSQL with `-c max_connections=300`:
  three times one run's peak, which leaves room for a second run, a running server and
  `psql`. The 3 superuser slots stay. CI keeps the default: at 4 threads, the service
  logs of its last three runs held no refusal.
- **A server per overlapping run.** The host port comes from `POSTGRES_HOST_PORT`, 5432
  by default. A second run at the same time starts a server under another project
  name, which gives it a volume of its own, points `DATABASE_URL` at its port, and
  removes it with `docker compose down -v`. A checkout that keeps a server of its own
  sets `COMPOSE_PROJECT_NAME` and `POSTGRES_HOST_PORT` in its `.env`, which both Compose
  and the tests read.

## Consequences

- PostgreSQL sizes its shared memory from the limit: 162 MB instead of 150 MB. A busy
  backend held about 1.9 MiB of memory of its own, so 300 at once would take about
  570 MiB.
- The limit is read only at server start. `docker compose up -d --wait` applies it by
  recreating the container; the data stays in its volume, and a test that runs at that
  moment fails.
- The limit bounds nothing: more hardware threads or more runs at once raise the demand
  again, and a refusal shows in the server's log.
- One server runs one suite at a time. A second server costs a container, a volume and
  a few seconds of setup, needs a free port, and stays until `down -v` removes it.

## References

- PostgreSQL 18 manual, [19.3. Connections and Authentication](https://www.postgresql.org/docs/18/runtime-config-connection.html): `max_connections` and `superuser_reserved_connections`.
- Docker Official Images, [postgres](https://hub.docker.com/_/postgres): server options passed with `-c`.
- Docker Docs, [Interpolation](https://docs.docker.com/reference/compose-file/interpolation/): `${VAR:-default}`.
- Docker Docs, [Configure pre-defined environment variables in Docker Compose](https://docs.docker.com/compose/how-tos/environment-variables/envvars/): `COMPOSE_PROJECT_NAME`, set in the shell or in `.env`.
- The rustc book, [Tests](https://doc.rust-lang.org/rustc/tests/index.html): `--test-threads`, its default, and `RUST_TEST_THREADS`.
- GitHub Docs, [GitHub-hosted runners reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners): the CPUs of a public repository's Linux runner.
- sqlx at the pinned commit (ADR-0006): [the PostgreSQL test harness](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-postgres/src/testing/mod.rs), with its pool sizes; [the test setup](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/testing/mod.rs), with the migrating connection and the database names; and [the pool](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/pool/inner.rs), with its retries.
- sqlx [PR #3334](https://github.com/transact-rs/sqlx/pull/3334), "Fix: nextest cleanup race condition" (2025): database names taken from the test's path.
