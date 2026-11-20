<!-- Author: Nicola Antonello -->

# ADR-0064: Learning each delivery by its transaction

- Status: accepted
- Date: 2026-11-20
- Supersedes: in ADR-0062's Decision, which of a search's rows the relay learns, and "No
  mark" where it says that the relay reads no transaction ID of the deliveries that it finds

## Context

A delivery reaches the relay twice when nothing goes wrong: through its handover, after its
catch-up's `COMMIT` (ADR-0052), and through the next search, which reads the transactions not
completed at the previous snapshot (ADR-0062). The relay leaves a delivery that it knows, so
the second copy does nothing while the delivery is known. Once the delivery succeeds and its
success is recorded, the relay forgets it (ADR-0063), and a copy that comes after that is
learned again, and sent again.

Two orders lead there. A search's rows statement runs after its snapshot statement, so it
can see a transaction that committed in between, and learn its delivery before the
delivery's handover is read: the relay sends it, records its success, then reads the
handover. And a catch-up can stall between its `COMMIT` and its handover while a search
finds the delivery. Either way an event already delivered is sent again. At least once
holds, since the merchant drops a copy by its `webhook-id`, but if the copy fails, the relay
retries a delivery whose success is recorded, and three days of failures would disable its
endpoint.

## Options

(a) The task reads every queued handover before each event's work, and the relay remembers
recorded successes until the next search: nothing changes in what is stored, but a catch-up
that stalls about a second between its `COMMIT` and its handover still causes a copy. (b)
Each delivery carries its transaction's ID, which the relay compares with its snapshots:
exact, with no assumption about timing, at the cost of one number in each handover and one
column in each search's rows. (c) Leave it, and say in ADR-0063 that copies come from it too.

The owner chose (b).

## Decision

- **A delivery carries its transaction's ID,** as `webhook_deliveries.xact_id` holds it,
  read through text as a `bigint`, as ADR-0062 reads a snapshot's IDs. The catch-up takes it
  from its insert's `RETURNING`, so that its handover carries it, and a search reads it with
  each row.
- **A search learns only the deliveries of transactions that had completed at its own
  snapshot.** A row of a transaction that committed after the snapshot statement waits for
  its handover, or for the next search, which reads that transaction again, since it had not
  completed at this snapshot.
- **A handover is left when its transaction had completed at the relay's snapshot,** the last
  search's. The first search whose snapshot had that transaction completed read its rows and
  learned the delivery, unless it was finished by then, so the handover adds nothing.
- **So the relay learns each delivery once:** through its handover, when that comes before a
  search's snapshot has its transaction completed, and otherwise through the first search
  whose snapshot has.

## Consequences

- An event already delivered is sent again only after what the relay cannot know: an answer
  lost to a timeout or a reset, and a restart, whose first search sends again each delivery
  without a recorded success.
- A delivery committed just after a search's snapshot statement waits for its handover, or,
  if its handover was dropped, for the next search, a tick later.
- The first search after a start leaves the rows of transactions that committed during it,
  for their handovers or the next search, which learns them as first attempts: they
  committed after the process started, so no earlier process attempted them.
- Before the first search the relay has no snapshot to compare with, so it learns every
  handover.
- Each handover carries one more number, and each search's row one more column; the index on
  `xact_id` serves the search as before.
- Tests: both rules as unit tests, the two orders above among them; random runs that hand a
  delivery over at once, late or never; and the model of PostgreSQL's transactions, checking
  that after each search the relay holds every delivery not finished whose transaction had
  completed at the snapshot.

## References

- PostgreSQL 18 manual, [System Information Functions and Operators](https://www.postgresql.org/docs/18/functions-info.html): which transactions a snapshot shows as completed, from its `xmin`, its `xmax` and its list in progress.
