<!-- Author: Nicola Antonello -->

# ADR-0070: Running reconciliation

- Status: accepted
- Date: 2026-11-24
- Supersedes: in ADR-0018's Decision, the words "such as a phase-6 settlement batch", and
  its consequence "A batch waits behind the payments it touches and then commits. While it
  runs, their commands wait in turn, so phase 8 measures batch sizes."; and, for a
  settlement batch, whose booking converges by its key (below): in ADR-0004's Decision,
  "never by re-posting with new IDs"; in ADR-0014's Decision, "by sending the identical
  insert again with the same IDs, never by rebuilding the posting with new IDs"; and in
  ADR-0015's Consequences, "after `NotConfirmed`, call `post()` again with the same value".

## Context

The server runs two tasks beside its routes. The sender's steps every second and runs a
pass of the sweep every minute; the relay's delivers events (ADR-0044, ADR-0063). One token
bucket bounds the calls to the PSP that are retries, 60 tokens and one more each second; a
first send, and the catch-up after a notification, are not retries, while each catch-up of
the sweep is one (ADR-0042). The sender owns the bucket and lends it to the sweep. A task
that ends stops the server, because the server would otherwise go on recording requests
and answering 202 while nothing sends them; restarting the task was rejected, because a bug
would panic again on the same rows, in a loop (ADR-0044).

ADR-0015 settles an unknown `post()` by calling it again with the same value, and lets
opening an account converge by its business key. ADR-0018 has a unit of work that changes
several payments lock all of them first, "such as a phase-6 settlement batch", and rejected
keys alone as a payment's point of order, partly because "a batch aborts on one collision".
Both predate phase 4, when a batch was to write the payments' steps (ADR-0019, ADR-0067).
PostgreSQL keeps advisory and regular locks in one pool that all sessions share, sized by
`max_locks_per_transaction` and `max_connections`; once it is exhausted, the server can
grant no lock to anyone.

`recorded_at` is never a cut-off (ADR-0012). The failure model assumes a PSP that is down
for hours, and no agreement between the services' clocks. ADR-0021 says that phase 6's
reconciliation runs its query of the ledger transactions that no business record
references; today the query is a helper of the tests, and knows captures and refunds.

## Options

Where the job runs:

| Option | Why not |
|---|---|
| **A task of its own, sharing the sender's bucket behind a lock, whose end does not stop the server** | Chosen |
| A pass of the sender's task, after the sweep's | The smallest change, but a large booking would hold up the sends, and a panic on one batch would stop the server again at every restart |
| A task of its own with a bucket of its own | Two buckets would draw on one PSP, so the bound on retries during an outage would no longer be one number |

A gap in a currency's numbering:

| Option | Why not |
|---|---|
| **Each batch booked as soon as it can be read, past a gap** | Chosen |
| Every later batch held until the gap fills | One missing file would stop the currency's settlement, and every movement in the held batches would go overdue although the PSP settled it |

Locks:

| Option | Why not |
|---|---|
| **None: the keys of the batch and of the chains order every writer** | Chosen |
| The payments' locks, every payment of the batch in ascending order, as ADR-0018 says | They guard nothing that booking writes; every command and catch-up on the batch's payments would wait for the whole booking; and each lock takes a slot in the shared pool, which a large batch could exhaust for every session |
| One lock per PSP, taken by every writer of reconciliation's rows | No booking would abort on a collision, but every outcome would wait behind a whole booking, and a writer that skips the lock must still be stopped by the keys, so reading again on a collision is needed anyway |

As of when a movement is overdue:

| Option | Why not |
|---|---|
| **As of the last pass that held every batch that the PSP listed, and never earlier than now less the agreed delay** | Chosen |
| The clock | Our own blindness would count against the PSP: while its files cannot be read, every movement whose deadline falls in that time would be found overdue, although the PSP may have settled it |
| The PSP's time: its newest batch's settlement time, or a time that its listing reports | A PSP that stops settling, or whose reporting stops, stops that time too, and the check goes silent exactly when it should speak |

Postings that nothing causes:

| Option | Why not |
|---|---|
| **Found by the job at every pass and recorded as discrepancies** | Chosen |
| Logged by the job | Nothing that the operator lists would show one |
| Left to the tests | ADR-0021's word goes unkept, and such a posting in production unseen |

## Decision

- **The reconciler's task.** A task of its own, beside the sender's and the relay's, with
  a third `PspClient`, runs a pass every minute. Each step takes `now` as an input, which
  the running task reads from the clock at each event; tests call the steps. Each of its
  calls to the PSP, for a listing, a file or a catch-up, costs a token, as each catch-up of
  the sweep does. The bucket moves out of the `Sender` into a handle that both tasks hold,
  `Arc<Mutex<RetryBudget>>`, locked only to take a token and never across an `.await`, so
  one bound covers every retried call to one PSP.
- **When the reconciler's task ends,** the server logs an error and goes on taking and
  sending payments, and batches wait for a fix and a restart. ADR-0044 stops the server
  when the sender's or the relay's task ends, because merchants would get answers that
  nothing honours; nothing that a merchant waits for depends on reconciliation.
- **A pass**, for each PSP and each currency with agreed terms (ADR-0068): it lists the
  batches from the first gap (ADR-0066); for each new batch, in number order, it fetches
  the file, catches up what the lines name, and books the batch in one database
  transaction, with its lines, its posting, its discrepancies and the steps that it writes,
  committed through the one function that applies ADR-0014's rule. Then it runs the
  evidence step, records missing batches and overdue movements, and looks for postings that
  nothing causes. It logs what changed, as the sweep's pass does.
- **Each batch once, in any order.** A batch is booked as soon as it can be read, whether
  or not every earlier one is held, under its key `(psp, currency, number)`:
  - a crash inside its transaction leaves nothing, and the next pass books it;
  - a `COMMIT` with an unknown outcome is settled at the next pass by looking the batch up
    by its key: held, it is done; not held, it is booked again with fresh IDs. A process
    that crashed no longer holds the value that ADR-0015 would send again, and the key
    commits with the posting, so a second booking fails on it and rolls back whole: a
    booking converges by its key, as an account does by its business key;
  - a batch listed again is skipped by its key before its file is fetched;
  - a batch that cannot be booked this pass, for a failed fetch, a history that cannot be
    read or no token, waits for the next pass and is logged once while it waits, as the
    sender logs a request that it holds; later batches still book, and no discrepancy is
    recorded, since the cause may pass;
  - a file that cannot be read (ADR-0066) is recorded once as a missing batch with that
    cause, read again at every pass, and closed as any missing batch is once a readable file
    replaces it.
- **No lock.** Booking takes neither the payments' locks nor one of its own. It writes no
  payment step and changes no fact (ADR-0067); the keys of the batch and of the chains
  order every writer (ADR-0069), as one primary key orders the operator's and the relay's
  disablements of an endpoint today. A booking that collides rolls back whole and is booked
  again at the next pass; an outcome that collides drops its transaction and reads again.
  ADR-0018's rule for a unit of work that changes several payments stands; a batch is not
  one.
- **The evidence step** closes, at every pass, each open timing difference whose evidence
  is held: a line in suspense whose fact is now held, by a late match, and a missing batch
  whose batch is now held. A discrepancy recorded beside its evidence, by a writer that
  skipped the job's order, closes at the next pass.
- **Overdue as of the last complete pass.** A movement's deadline is its business time
  plus the agreed delay D (ADR-0068). It is overdue once its deadline is before the as-of
  time: the time of the listing of the latest pass, for its PSP and currency, that ended
  holding every batch that it listed, save those recorded as missing; and never earlier
  than now less D.
  - A gap and a file that cannot be read do not hold the time back: each is already a
    discrepancy. A batch that cannot be booked yet and a listing that fails do, each
    logged, and the floor bounds them to D.
  - The time is ours, read at the listing. A skew between our clock and the PSP's moves
    when a discrepancy appears, never money: an overdue step posts nothing, and evidence
    reverses any outcome on it. A delay of days absorbs minutes of skew, and the sweep
    already compares the PSP's `expires_at` with our clock.
  - The as-of time lives in the task's memory, per PSP and currency. After a start only the
    floor applies, until the first complete pass. The overdue step records the as-of time
    that it used.
- **Postings that nothing causes.** At every pass the job runs ADR-0021's query, which moves
  from the tests into the library with phase 6's causes added, the batches and the records
  that post, and records each ledger transaction that it finds once, as a discrepancy that
  only an operator's acceptance closes (ADR-0069). That record is not a cause, so the query
  keeps listing the transaction, and the record's key keeps it recorded once. The query
  reads the whole ledger.

## Consequences

- Phase 4's code changes: the bucket leaves the `Sender` for a shared handle, and the
  signatures of the `Sender`, the sweep and their tests change.
- The server spawns a third task, whose end is logged, not fatal.
- A booking holds one connection of the system pool while it runs, and nothing waits for
  it.
- The sweep and the reconciler may catch up the same payment in one minute, for two tokens.
- A batch that never books holds back the overdue check by at most D, and the batches after
  it still book.
- Phase 8 measures how long a booking takes, the query's scan of the ledger, and the
  reconciler's share of the bucket.

## References

- PostgreSQL 18 manual, [Advisory Locks](https://www.postgresql.org/docs/18/explicit-locking.html#ADVISORY-LOCKS) and [Lock Management](https://www.postgresql.org/docs/18/runtime-config-locks.html) (`max_locks_per_transaction`).
