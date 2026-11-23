<!-- Author: Nicola Antonello -->

# ADR-0065: How the outbox's failure tests run

- Status: accepted
- Date: 2026-11-23
- Supersedes: in ADR-0056's Decision, "The true time", and its consequence on the relay's
  tests

## Context

ADR-0054 lists the tests that try to break phase 5's invariants, and most of them stand:
`LG008` and the catalog, the foreign keys behind I7 and I10, an event whose transaction
stays open, the relay's task, and the binaries end to end. Three are left, and each needs a
choice that ADR-0054 does not make: the backend killed around the catch-up's new inserts,
the relay stopped between a `2xx` and its record, and the property test of random
interleavings.

ADR-0056 says that the relay "stamps each attempt with the true time, even when a test
passes a later time to its step". It does not. The task starts each attempt at its step's
time, which signs it (ADR-0051) and from which a `Retry-After` date counts (ADR-0053). The
running task reads the clock at each event, so the server's attempts carry the true time,
but a test's times sign the test's attempts, and the fake refuses a timestamp more than
five minutes from its own clock. The relay's tests keep their times near the clock for that
reason.

## Options

The relay stopped between a `2xx` and its record: (a) the server's process killed while a
lock that the test holds keeps the record from its `COMMIT`, then started again; (b) the
record's session killed at a crash point, the task dropped and a new one started on the
same database: no process dies, and the server's start does not run; (c) both, although
the property test already drops and restarts the task among its operations.

The property test: (a) runs drawn from fixed seeds, against PostgreSQL and real HTTP, as
`payment_model.rs` does for the payments, without shrinking; (b) proptest with shrinking,
against PostgreSQL: the relay's search reads the whole database, so each case would need a
database of its own, and shrinking assumes that a failure recurs, which one that depends on
thread timing may not; (c) a pure model, without PostgreSQL or HTTP: blind to the fan-out's
SQL, the foreign keys, the snapshots and the timeouts that this test is for, and the
relay's rules already have three such models.

The time of an attempt: (a) its step's, as the code does, with ADR-0056 amended; (b) the
clock's, read by each attempt apart from its step's time, as ADR-0056 says: a product
change, and the one place where the relay would read a clock itself; (c) its step's, with
a clock in the fake that its handle sets, so that a test could move time freely: a change
to the fake, and a model that must follow the three days across restarts.

The owner chose (a) for all three.

## Decision

- **The catch-up is killed around each new insert.** The crash triggers cover
  `payment_events` and `webhook_deliveries`, and the catch-up's crash tests register two
  endpoints, so that each fact writes deliveries. A catch-up killed before or after either
  insert, or during its `COMMIT`, leaves none of its rows, and the next one writes one event
  per fact and one delivery per endpoint.
- **The server is killed between a `2xx` and its record.** The test holds `LOCK TABLE
  webhook_delivery_successes IN SHARE MODE`, which conflicts with the `ROW EXCLUSIVE` lock
  that an `INSERT` takes and not with the `ACCESS SHARE` lock of a `SELECT`, so the relay's
  searches run and its record waits. Once the fake has answered `200` and the wait shows in
  `pg_stat_activity`, the test kills the process and lets the lock go, and the dead
  server's session ends without a `COMMIT`. The server started again finds the delivery in
  its first search and sends it again: the fake receives it twice under one `webhook-id`,
  answers the copy `200`, and one success is recorded.
- **The property test is runs against a model,** over PostgreSQL and real HTTP. Each run
  draws eighty operations from a fixed seed: catch-ups for three merchants, some killed at a
  crash point, some whose handover is dropped, some held open by a lock on
  `webhook_deliveries` while the task searches, after a later transaction has completed, so
  that the search's snapshot lists theirs as in progress, half of them without their
  handover; the task's ticks, handovers and attempts' ends; restarts, which drop the task
  with its attempts and start a new one; records of successes that the database refuses for
  a while; and endpoints registered, disabled by the operator, or answering `200`, `500`,
  `410`, `200` after the client's timeout, or `Retry-After`. Each endpoint is a fake of its
  own. A run ends with every enabled endpoint answering `200` while the task steps until
  nothing is unfinished, within a bound that fails the run when reached. The operations and
  their choices come from one generator, and the task's draws, whose number depends on the
  threads' timing, from another.
- **The model** holds, for each event, the endpoints that must get it: those of its
  merchant that were enabled when its catch-up read them (ADR-0053's amendment). At the end
  the deliveries are exactly those; each has a success or its endpoint a disablement; each
  disablement has its cause, the operator's choice or an answer of `410`; each success
  follows an answer of `2xx` that the task could read, or a copy, never one after its
  timeout; and each fake received only its merchant's events and only events that exist.
  Across the runs, every kind of operation, and every answer, must have come up.
- **Registrations and disablements run in sequence with catch-ups, and catch-ups race the
  task's searches.** The catch-up reads the endpoints once, so its race with a registration
  or a disablement ends in a state that some sequence reaches, and racing them would only
  blur the model. A search during a catch-up's transaction sees a state that no sequence
  reaches.
- **Each attempt is stamped with its step's time,** which in the running server is the
  clock, read at each event. A test keeps the times that it passes within the fake's five
  minutes, so the property test draws short delays and `Retry-After` of seconds, and the
  three-day rule stays with the task's tests and the relay's models.

## Consequences

- A failing run prints its operations and repeats them from its seed, but thread timing
  does not repeat, so a failure may need several runs to recur. It is a suspected race,
  never noise.
- The property test checks where each run ends, not each step: a wrong step that a later
  one repairs passes, as long as nothing ends lost or sent astray.
- A copy sent for no reason passes too, since at least once allows it. That each delivery
  is learned once, and a held success not sent again, is for the relay's own models and its
  task's tests to show.
- What the tests do not show stays as ADR-0054 lists it, with the three-day rule outside
  the random runs.
- The runs are batched in databases of their own, so that the harness runs the batches in
  parallel, and their number is set by measurement.

## References

- John Hughes, [Experiences with QuickCheck: Testing the Hard Stuff and Staying Sane](https://publications.lib.chalmers.se/records/fulltext/232550/local_232550.pdf) (2016): testing a stateful system against a model of its state.
- PostgreSQL 18, [Explicit Locking](https://www.postgresql.org/docs/18/explicit-locking.html): the table-level lock modes and the ones that conflict.
