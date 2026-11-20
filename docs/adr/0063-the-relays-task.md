<!-- Author: Nicola Antonello -->

# ADR-0063: The relay's task

- Status: accepted
- Date: 2026-11-20

## Context

ADR-0052, ADR-0053 and ADR-0062 decide what the relay does, `relay.rs` holds its rules, and
`deliveries.rs` its input and output. What is left is the task that runs them, and two
failures that those records leave open.

A record can fail: its insert fails, or its `COMMIT` fails and the outcome is unknown
(ADR-0014). The relay forgets a delivered delivery, and a disabled endpoint's deliveries, as
it returns the effect, and a search after the first reads only the transactions not
completed at the previous one (ADR-0062). So before a restart nothing else records a
success or a disablement whose record failed.

An attempt can panic: an `expect` that names an invariant, or a bug. The relay frees an
attempt's slot only on its answer, on finding its endpoint disabled, or on its not being
sent, so without a rule a panicked attempt stays in flight for good.

## Options

A record that fails: (a) the relay keeps each effect until the task reports it recorded,
and every step hands back those not recorded yet; (b) the task stops, and the server with
it (ADR-0044), so that the restart's full search repairs it, and a database blip takes the
server down; (c) it is logged and dropped: a success is sent again only after a restart,
and a disablement that the database lacks leaves the endpoint enabled there while the
relay ignores it.

A panicked attempt: (a) read as not sent; (b) read as no answer, so that our bug counts
toward the merchant's three days; (c) the task stops, and the restart's full search finds
the same delivery, so a panic that repeats stops the server again and again.

The owner chose (a) for both.

## Decision

- **One task owns the relay**, with the client, the system role's pool and a set of
  attempts, each in a tokio task of its own. Its `select!` only picks the next event: a
  tick every second, the first at once, which searches; a handover; or an attempt's end.
  The work runs after the `select!` (ADR-0043). Every event steps at once, so a freed slot
  is used at once.
- **A step starts its attempts before it awaits anything,** so that the relay never counts
  in flight an attempt that no task runs, then records the effects that the relay hands
  back, in order, and stops at the first that fails: the database is then most likely out
  of reach, so an outage costs one failed record per step.
- **The relay keeps each effect until it is recorded.** Every step hands back each effect
  that the relay returned, from an answer or from a step, until the task reports its
  record. Both inserts are `ON CONFLICT DO NOTHING`, so a record tried again after an
  unknown `COMMIT` writes once.
- **A success that the relay holds is not sent again.** A handover or a search leaves a
  delivery whose success the relay holds, as it leaves one of an endpoint that it disabled:
  its record failed, so a search that reads its transaction still finds it, and attempting
  it again would send it twice for nothing.
- **A panicked attempt is read as not sent:** its delivery waits like a retry, for its delay
  and a token, its endpoint's three days do not start, and the panic is logged as an error.
  The task finds its delivery by the attempt's task ID: `JoinSet::spawn` returns a handle
  whose ID the task maps to the delivery, `join_next_with_id` gives that ID with the output,
  and `JoinError::id` with a panic. The entry leaves the map when its attempt is joined,
  before tokio may give the ID to a new task.
- **The log** holds each disablement once, as it is recorded, with its reason; each failed
  search, as the sender's failed step is logged; a record that fails and an attempt not
  sent, once while they keep failing, until a record goes through or an attempt is made;
  and a panic, as an error, each time. An attempt that went as it should has no line.
- **The server runs the task beside the sender's,** and stops if either ends, since neither
  returns (ADR-0044). The route's catch-ups and the sender's hand over through one channel
  of 1,024 deliveries (ADR-0052). `Webhooks::new` and `Sender::new` take the handover as a
  required argument, as `catch_up` does, so that no caller can forget it.

## Consequences

- At least once still holds: until its record goes through, the relay holds a success, and
  the merchant drops any copy by its `webhook-id`.
- An effect whose record can never succeed would hold every record behind it. None can
  come from the database's own rows, since an effect names a delivery or an endpoint that
  its foreign keys keep; only a privilege taken away can do it, and the log shows it.
- During an outage the attempts are not sent either, since each reads its endpoint and its
  body first, so the effects held stay few: those whose record failed after their attempt
  was made.
- A bug that panics in an attempt panics again at each retry, under the budget's tokens,
  and is logged each time.
- Attempts are bounded per endpoint only (ADR-0053). A burst across many endpoints can start
  many at once, each reading its endpoint and its event on the system role's pool, which the
  API's key lookups share. Phase 8's benchmarks measure it and size any bound.
- The tests drive each event with their own times and draws against the fake merchant,
  run the task as the server does on a short or an hour-long tick, and make the database
  refuse a read or a record by revoking the system role's privilege in the test's own
  database.

## References

- tokio 1.54, [`JoinSet`](https://docs.rs/tokio/1.54.1/tokio/task/struct.JoinSet.html),
  [`JoinError`](https://docs.rs/tokio/1.54.1/tokio/task/struct.JoinError.html) and
  [`task::Id`](https://docs.rs/tokio/1.54.1/tokio/task/struct.Id.html): a task's ID with its
  output or its panic, and when an ID can be given again.
