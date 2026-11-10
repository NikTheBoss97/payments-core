<!-- Author: Nicola Antonello -->

# ADR-0044: What the sender's task logs, and how it runs

- Status: accepted
- Date: 2026-11-10
- Supersedes: in ADR-0043's Decision, "The sender's task", "A report that is not empty
  is logged".

## Context

ADR-0043 gives the sender a task of its own in the server. It owns the `Sender`, takes a
step every second and a pass of the sweep every 60 seconds, picks each event with a
`select!` and runs the work after it, and logs each report that is not empty.

A step's report also lists what has not changed. A request that a stopped payment holds
is in every step's report until an operator acts (ADR-0042), and so is a request
unacknowledged an hour after its first attempt. During an outage, each waiting request
that finds no token of the retry budget is an attempt of every step. Logged as ADR-0043
says, each of them prints a line every second.

The task runs beside the HTTP server. When a spawned task panics, tokio ends that task
alone and hands its `JoinHandle` "a `JoinError` that contains the panic", so a bug that
panics in a step would leave a server that records requests and answers 202 while
nothing sends them. The failure model already assumes that "the process crashes between
any two steps" (DESIGN.md).

## Options

What the task logs:

| Option | Why not |
|---|---|
| **Changes, once: each attempt and each catch-up that did something, and a request when it becomes held or overdue** | Chosen |
| The same, with the held and overdue requests listed again at each pass of the sweep | A stuck request stays in a recent log, but N stuck requests cost N lines a minute, and the log repeats what has not changed |
| Every report that is not empty, as ADR-0043 says | One line a second for each held or overdue request until an operator acts, and in an outage one for each waiting request |

What a task that ends does to the server:

| Option | Why not |
|---|---|
| **The server stops with an error, as a crash** | Chosen |
| The server goes on | It records requests and answers 202, and none is sent until a restart |
| The task starts again | A bug that panics on the rows that it reads panics again on the same rows, in a loop |

## Decision

- **What is logged,** in place of every report that is not empty:
  - each attempt that reached the PSP or recorded something, with its outcome: a send
    that the PSP holds, a lookup that found the request, a malformed request, a
    `Retry-After`, an unknown outcome, a lookup that failed, a request whose payment
    stopped, and an answer that could not be recorded. What waits for an operator, and
    what failed, is a warning;
  - each catch-up, a lookup's or the sweep's, that applied facts, stopped its payment or
    failed;
  - a request once when it becomes held by a stop, or overdue, and again if it leaves
    that state and comes back. The task keeps both sets in memory, so after a restart it
    logs each such request once more;
  - a step or a pass that cannot list its work, each time, as the API logs the cause of
    each 5xx (ADR-0023);
  - nothing for an attempt that found no token, a catch-up that applied nothing, or a
    report that holds nothing else.
- **What to log is a pure function** of the step's or the pass's result and of the two
  sets, so its tests need no clock and no log.
- **Timing.** The first step and the first pass run as the server starts, since an
  interval's "first tick completes immediately", so a restart sweeps at once. A step that
  runs late delays the next one by a whole period, `MissedTickBehavior::Delay`, rather
  than running the missed steps in a burst.
- **A task that ends stops the server,** which exits with an error, as from a crash: a
  supervisor starts it again, and everything that the task knew is in the database or
  was only timing (ADR-0032).

## Consequences

- The log holds what happened, once. What lasts, such as a stopped payment or an overdue
  request, is in the tables, and phase 8 counts it.
- A bug that panics in a step brings the server down, rather than leaving requests that
  nothing sends.
- Tests: the choice of lines, as a pure function, for each attempt and outcome, and for a
  request held, overdue, released and held again; a step that sends a recorded request; a
  pass that catches up a payment whose notification never came; the binary sending a
  request and sweeping as it starts; a held request in the binary's log; the binary
  stopping when its task ends.

## References

- tokio, [`interval`](https://docs.rs/tokio/1.54.1/tokio/time/fn.interval.html), [`MissedTickBehavior`](https://docs.rs/tokio/1.54.1/tokio/time/enum.MissedTickBehavior.html) and [`JoinHandle`](https://docs.rs/tokio/1.54.1/tokio/task/struct.JoinHandle.html) (tokio 1.54.1 API docs).
- The records that it builds on: ADR-0023, ADR-0032, ADR-0042 and ADR-0043, and the
  failure model of [DESIGN.md](../DESIGN.md).
