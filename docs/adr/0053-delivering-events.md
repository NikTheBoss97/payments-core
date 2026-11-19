<!-- Author: Nicola Antonello -->

# ADR-0053: Delivering events

- Status: accepted
- Date: 2026-11-15
- Supersedes: in ADR-0032, "One budget for the process" and "One point retries", which
  now hold for calls to the PSP: the relay retries deliveries under a budget of its own.

## Context

Standard Webhooks sets out what a sender owes. A delivery is "considered successful if it
was responded to with a `2xx` status code"; redirects are failures, since "Following
redirects causes unnecessary load on both the sender and the receiver"; on a `410` the
sender "should disable the webhook endpoint, and stop sending it messages"; 429, 502 and
504 call for throttling, and `retry-after` "should be taken into consideration". It
recommends a timeout "somewhere between 15 and 30s", retries "spanning multiple days, with
an exponential backoff", with jitter, and, when delivery fails consistently over a long
period, telling the consumer by other means and disabling future delivery to the
endpoint.

Stripe retries for up to three days with exponential backoff, cancels the remaining
retries of a destination that has been disabled, and does not deliver events in the
order in which they were generated. Svix retries over about 28 hours, from at once to two
waits of 10 hours, and disables an endpoint whose attempts have all failed for 5 days.

ADR-0035 lets phase 5 reuse the shape of the polling relay, full-jitter backoff and the
token bucket, but not the machinery of commands: "Events go to every subscriber, and
nothing comes back."

## Options

Which endpoints an event goes to:

| Option | Why not |
|---|---|
| **Those enabled when the event is written, fixed by deliveries written with it** | Chosen |
| Those enabled when the relay first takes it | A restart between taking it and recording what was sent could give an old event to an endpoint registered since, and the search could not tell which endpoints an event owes |

An endpoint that keeps failing:

| Option | Why not |
|---|---|
| **Disabled once one of its deliveries has gone 3 days without a success** | Chosen |
| Disabled after 3 days without any success while it had deliveries due | A delivery that the endpoint always refuses, while it accepts the rest, would be retried for ever and hold ADR-0052's mark at its transaction, so the search would grow without end |
| Retried until the operator or a `410` disables it | An endpoint that nobody disables would hold the mark at its oldest delivery for ever, with the same result |
| Each delivery given up after 3 days, the endpoint staying enabled | A table of abandoned deliveries to add, and a merchant whose endpoint was down for 3 days, or refuses one event, would lose events without being told |

The budget for retries:

| Option | Why not |
|---|---|
| **A token bucket of the relay's own** | Chosen |
| ADR-0032's one bucket for the process | A dead merchant endpoint would spend the PSP's retries, and an outage at the PSP the merchants' |

How the tokens are shared:

| Option | Why not |
|---|---|
| **In turn across endpoints, from where the last step stopped** | Chosen |
| Oldest first | A dead endpoint with a long backlog would take every token until it is disabled, as ADR-0042 found for the sweep: "The lowest IDs take every token" |
| A bucket per endpoint | The process's retries would grow with the number of endpoints, which the budget exists to bound |

## Decision

- **The fan-out is fixed when the event is written.** With each event, the catch-up's
  transaction writes one delivery for each endpoint of the merchant that is enabled when
  its statement runs. A delivery holds its ID, a UUIDv7 made in Rust (`DeliveryId`), its
  event, its endpoint and its merchant (ADR-0054). A merchant without an endpoint gets the
  event and no delivery, and an endpoint registered later never gets it.
- **An attempt** posts the event's body (ADR-0049) to the endpoint's URL, signed as
  ADR-0051 says, through the client of ADR-0036, which follows no redirect and sends
  nothing again by itself, inside a timeout of 15 seconds. A timeout means that the outcome
  is unknown, as it does with the PSP (ADR-0032).
- **The answers:**

  | The endpoint answers | The relay |
  |---|---|
  | `2xx` | records the success, and the delivery is finished |
  | `410` | disables the endpoint, and its deliveries are finished |
  | 429, 502 or 504, or any answer with `Retry-After` | waits at least that long, then tries again |
  | `3xx`, any other status, a timeout, a reset | backs off and tries again |

- **A success is one row,** under a unique key on its delivery, inserted as
  `payments_app` through the one function of ADR-0014. A `23505` means that it is recorded
  already. Failed attempts are not recorded: timing lives in memory (ADR-0032).
- **Retries.** Before the *n*th retry of a delivery the relay waits a time drawn uniformly
  between zero and `min(1 hour, 1 second × 2^n)`, and never less than a `Retry-After`
  (ADR-0042). A retry costs a token from a bucket of the relay's own, with ADR-0042's shape
  and numbers; a first attempt costs none. The tokens go to the endpoints in turn: at each
  step the relay takes the due retries one endpoint at a time, starting after the endpoint
  it served last and wrapping round, as ADR-0042's sweep resumes after its last payment.
  One endpoint's backlog can never take every token.
- **At most 4 attempts at once per endpoint,** through a semaphore that the relay tries
  and never waits on, as ADR-0043 bounds catch-ups. A slow merchant holds back only its
  own deliveries.
- **No order** is promised. A merchant reads the state (ADR-0049), so order does not
  matter.
- **An endpoint that keeps failing** is disabled by the relay once one of its deliveries
  has gone 3 days without a success, counted from that delivery's first failed attempt:
  one disablement with the reason `failing`, logged once (ADR-0044). A dead endpoint and
  an event that a merchant's handler always refuses meet the same rule. A `410` adds a
  disablement with the reason `gone`, and the operator's route one with the reason
  `operator`. The endpoint's unfinished deliveries end with it. The time is an input to
  the relay's step (ADR-0029), and the clock lives in memory, so a restart can only delay
  a disablement.
- **After a disablement** the operator registers a new endpoint, which gets the events
  written from then on. What the merchant missed, it reads through the API, as it polls
  today.

## Consequences

- I10: every event reaches every endpoint enabled when it was written, at least once,
  unless that endpoint is disabled first (ADR-0054).
- Every delivery ends within three days of its first failure, delivered or with its
  endpoint disabled, so ADR-0052's search reads about three days behind at most; a
  restart can only push that later.
- One event that a merchant's handler always refuses disables its endpoint. The merchant
  fixes the handler, and the operator registers a new endpoint.
- A crash between a `2xx` and its record, or a timeout after a delivery arrived, sends an
  event again. A merchant drops the copy by its `webhook-id`.
- Tests: each answer of the table; one delivery refused for three days while the endpoint
  accepts the rest, and an endpoint that refuses everything, with the time as an input;
  the bucket under an endpoint's outage, apart from the sender's; a dead endpoint's backlog
  leaving tokens for another endpoint's retries; the semaphore of 4; a crash between a
  `2xx` and its record, which repeats the delivery.

## Amendments

- 2026-11-16: "enabled when its statement runs" means enabled when the catch-up reads the
  merchant's endpoints, once, under the payment's lock, before its first new fact. The
  deliveries' IDs are made in Rust (ADR-0005), so reading the endpoints and writing the
  deliveries are two statements (ADR-0057).
- 2026-11-17: "In turn" is one retry per endpoint per round: at each step the relay gives
  each endpoint with a retry due one token, in the order of the endpoints' IDs from after
  the one served last, wrapping round, and repeats the round until the tokens or the due
  retries run out. A first attempt takes neither a token nor a turn. The semaphore of 4 is
  the relay's own count of attempts in flight per endpoint, which an answer frees: an
  attempt due at a full endpoint waits for a later step. "Waits at least that long" needs a
  length, so a 429, 502 or 504 without `Retry-After` backs off like any other failure. The
  deliveries of an endpoint that the relay disables leave its memory with it, and an
  answer that arrives for one of them later is recorded only if it is a `2xx`.
- 2026-11-18: An attempt parses its endpoint's stored URL with ADR-0060's rules, which
  ADR-0061's check stands behind. A URL that the rules refuse is an attempt without an
  answer: the endpoint cannot be reached at it as written, and the three days end it as
  `failing`.
- 2026-11-18: An attempt that the service could not make, because its database did not give
  the endpoint's URL, its disablement or the event's body, is no failed attempt: it waits
  like a retry, for its delay and a token, and does not start the three days.
- 2026-11-18: An answer is its head: a `2xx` is a success once its head arrives within the
  15 seconds. Its body is then read for at most a second and 64 KiB, only so that the
  connection can be used again, and nothing in it changes the answer.
- 2026-11-18: `Retry-After` is read in either of RFC 9110's forms, seconds or an HTTP-date
  in any of its three formats. A date counts from the attempt's start, one already past
  asks for no wait, and a wait too long for 32 bits reads as the longest. The PSP's
  `Retry-After` is still read in seconds only, its format being ours (ADR-0032).
- 2026-11-19: The relay's records insert with `ON CONFLICT DO NOTHING` on their unique
  keys, a success's on its delivery and a disablement's on its endpoint, as the operator's
  route does (ADR-0050). Recording either again writes nothing and raises no `23505`, and
  a first disablement keeps its reason.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): delivery success and failure, request timeouts, deliverability and reliability.
- Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks) (docs): automatic retries and the order of events.
- Svix, [Retry Schedule](https://docs.svix.com/retries) (docs).
- Marc Brooker, [Exponential Backoff And Jitter](https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/) (AWS Architecture Blog, 2015).
