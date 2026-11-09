<!-- Author: Nicola Antonello -->

# ADR-0041: Catching up: stale reads, bad answers and the sweep's pass

- Status: accepted
- Date: 2026-11-09

## Context

ADR-0031 and ADR-0039 decide what a catch-up does: it reads the PSP's history outside
any transaction, takes the payment's lock, checks that the facts it holds are the PSP's
first facts, applies the new ones in one transaction, and stops the payment for an
operator with `history_changed`, `no_place` or `contradiction`. The sweep catches up
the payments that wait for the PSP. Writing that code leaves four questions open.

Two catch-ups of one payment can cross, such as a notification's and the sweep's. Each
reads the history before it takes the lock, so the first to read may be the last to
take it, holding a history shorter than the facts that the other has just committed.
Read literally, ADR-0031 stops the payment then, since the facts held are not a prefix
of the history read. A stale read is normal under concurrency; a PSP that lost facts is
not. One read cannot tell them apart.

The PSP's answer can be wrong as a whole, or in one fact. A test must make two
catch-ups cross at will. And a pass of the sweep could take every payment that is due,
or some of them, one at a time or several at once.

## Options

A history shorter than the facts held:

| Option | Why not |
|---|---|
| **Count the facts held before reading the PSP; a history shorter than that count has changed, and one shorter only than the facts held now is stale** | Chosen |
| Every shorter history is stale | A PSP that loses facts is never caught |
| Every shorter history has changed | A notification and the sweep that cross would stop the payment: a false alarm that the failure model makes routine |
| Read the history under the lock | Holds a connection and the payment's lock across a call to the PSP, which ADR-0031 rules out |

A bad answer from the PSP:

| Option | Why not |
|---|---|
| **An answer wrong as a whole applies nothing and is read again; one fact that cannot be placed stops the payment; unknown fields are ignored** | Chosen |
| Every bad answer stops the payment | A body cut short by a passing fault would stop a payment for good |
| Every bad answer is read again | A fact that never fits would be read forever, and its payment would never reach an operator |

How a test makes two catch-ups cross:

| Option | Why not |
|---|---|
| **The catch-up in two public halves, a read and an apply, which a test calls in any order** | Chosen |
| A hook in the catch-up that pauses it between the read and the lock | Production code whose only use is a test, the kind that ADR-0039 avoided for I9 |
| Run catch-ups at once, in a loop | The crossing happens only sometimes, so a pass proves little |

A pass of the sweep:

| Option | Why not |
|---|---|
| **Every payment that is due, one at a time** | Chosen |
| A bounded batch per pass | Needs an order that starves none of the rest, and a cursor between passes |
| Several catch-ups at once | Concurrency and pressure on the pool before phase 8 measures a need |

## Decision

- **Counting before reading.** A catch-up counts the facts that it holds, then reads
  the history. A fact is written only by a catch-up, after a read that served it, so
  the PSP had served at least that many facts before this read began. Under the lock:
  - a history shorter than that count has lost facts, and the payment stops with
    `history_changed` at the first position that the history lacks;
  - every position that the history and the facts held both have is compared, ID for
    ID, and the first that differs stops the payment with `history_changed`;
  - a history at least as long as the count but shorter than the facts held now is a
    stale read, and nothing is applied;
  - the facts after the last one held are new, and apply in order.
- **Bad answers.** An answer wrong as a whole applies nothing: one that is not the JSON
  of a history, names another payment, numbers its facts other than 1, 2, 3 and so on
  in order, or gives a fact an ID of another shape than ADR-0037's. The catch-up
  reports it, and the sweep reads again. A fact that cannot be placed stops the
  payment with `no_place` at its position, after the facts before it apply: a kind
  that the service does not know, a field missing, an amount outside 1 to the cap of
  ADR-0003, or a time that is not RFC 3339 or cannot be stored (ADR-0022). Fields that
  the service does not know are ignored, as Stripe asks of its integrations: "Design
  your integration to ignore unfamiliar fields and event types".
- **Which reason stops a new fact,** checked in this order:
  1. a fact that cannot be placed: `no_place`;
  2. a decision on a request that the payment never recorded, on one decided already,
     on one of another kind, or for another amount (an authorization above its request,
     a capture or a refund other than its request): `contradiction`;
  3. a fact that the table of moves or a limit refuses: `no_place`.

  A settlement of a payment that was refunded in full and never settled is recorded
  without a step (ADR-0016). A stop commits with the facts before it, which agreed.
- **Two halves.** `read` counts the facts held and reads the history, outside any
  transaction; `apply` takes the lock, checks, applies and commits. `catch_up` runs one
  after the other. A test that runs the halves of two catch-ups in an order of its
  choosing makes them cross at will, as time is a step that a test calls (ADR-0029).
- **The sweep's pass.** One query lists the PSP's payments that are due (ADR-0039),
  with the time now as an input and the stopped payments left out, and each is caught
  up in turn. A failure is reported with its payment, and the pass goes on. The token
  bucket of ADR-0032 comes with the sender, which decides where a pass that the bucket
  ends early resumes.
- **The client reads histories only,** until the sender comes: hyper-util's legacy
  client (ADR-0036), with a timeout of 10 seconds around each call and a body of at
  most 1 MiB, far above any payment's history.

## Consequences

- One more read per catch-up, of the facts held, before the call to the PSP.
- A PSP that loses facts is caught by the first catch-up that reads after the service
  held them, and one that changes a fact's ID is caught at that fact's position.
- An answer wrong as a whole is read again at every pass of the sweep, and reported
  each time, until it reads well.
- The tests of a crossing call the halves in order. Until the server records
  notifications, ADR-0031's property test runs catch-ups in their place: a
  notification only starts one.
- `mock-psp` becomes a dev-dependency of `payments-core` (ADR-0029).

## References

- Stripe, [How API versioning works](https://docs.stripe.com/api-versions) (docs),
  "Compatible changes and breaking changes": adding properties to a response is a
  backward-compatible change.
- The records that it builds on: ADR-0003, ADR-0016, ADR-0022, ADR-0029, ADR-0031,
  ADR-0032, ADR-0036, ADR-0037 and ADR-0039.
