<!-- Author: Nicola Antonello -->

# ADR-0032: Answers and retries

- Status: accepted
- Date: 2026-11-07

## Context

Every answer promises something about durable state. A `2xx` tells the other side to
stop retrying, because the work is now the answerer's, so it may be sent only once what
it promises has committed. Any other answer asks for the message again, which is safe
only because the receiver drops copies (ADR-0028, ADR-0031).

Retries are how at-least-once delivery is built, and also how outages grow. The Amazon
Builders' Library: "Retries are 'selfish.'", and "When failures are caused by overload,
retries that increase load can make matters significantly worse. They can even delay
recovery by keeping the load high long after the original issue is resolved." Bronson
et al. name the pattern: a metastable failure is "a bad state that persists even when
the trigger is removed", held there by "a sustaining effect—often involving work
amplification", and "the root cause of these failures is often features that improve
the efficiency or reliability of the system". Retries are such a feature.

This record decides what the server answers the PSP, how the sender reads the PSP's
answers, and how both space their retries.

## Options

When the server answers a notification with `2xx`:

| Option | Why not |
|---|---|
| **Once the notification is recorded; catching up comes after** | Chosen |
| Once its facts are applied | The PSP's delivery would wait for the server's read of the PSP's history and for the payment's lock. Adyen waits 10 seconds for an answer, the spec tells senders to wait 15 to 30, and `lock_timeout` is 35 (ADR-0018). A busy payment would time deliveries out, and each retry would add another read and another wait on the lock |
| Before it is recorded | The sweep of ADR-0031 would still find the facts, but the PSP would stop retrying and nothing would record what it sent, to save one insert |

How retries are spaced and bounded:

| Option | Why not |
|---|---|
| **Exponential backoff with full jitter and a capped delay; no limit per request; one token bucket for the process** | Chosen |
| A fixed schedule, such as the spec's example, then give up and flag | Giving up leaves undecided a request for which the merchant already holds a 202 |
| Exponential backoff without jitter | Brooker's measurements make it "the clear loser": more work and more time than any jittered variant, of which "Full Jitter" does the least work |

## Decision

- **The server's answers to a notification:**

  | Situation | Answer |
  |---|---|
  | Verified and recorded, now or before | 202, whose RFC 9110 meaning, "accepted for processing, but the processing has not been completed", is this promise |
  | Unreadable, or failing its signature or timestamp (ADR-0030) | 400, as Stripe's example endpoints answer |
  | Not recorded, because the database failed or the `COMMIT`'s outcome is unknown (ADR-0014) | 503 with `Retry-After`; a copy that the PSP sends again is dropped by its event ID |
  | Never | 410, on which Standard Webhooks tells a sender to "disable the webhook endpoint" |

  Recording takes a short transaction of its own. A `23505` on `(psp, event_id)` means
  that a copy committed first: drop the transaction, find the copy, and answer 202. A
  verified notification about a payment that the server does not know is recorded,
  flagged and answered 202, because no retry can change that.
- **Catching up follows the answer,** in the server. A crash before it ends leaves the
  sweep to finish it (ADR-0031). Adyen describes the same order: "Store the webhook
  message in your database or a queue so you can process it later", then accept it, and
  "Do not validate or process the data at this step"; Stripe asks an endpoint to "quickly
  return a successful status code (`2xx`) before any complex logic that could cause a
  timeout".
- **The PSP's answers to the sender.** The fake answers a request at once with whether it
  holds it, and every decision, refusals included, is in its history (ADR-0029,
  ADR-0031):

  | The PSP answers | Meaning | The sender |
  |---|---|---|
  | `2xx` | it holds the request | records the acknowledgement and stops |
  | 400 or 422 | the request is malformed, or its ID came with other content | a bug of ours: flags it, and never sends it again |
  | 429, or 503 with `Retry-After` | try later | waits at least that long |
  | any other `5xx`, a timeout, a reset | the outcome is unknown | backs off, then reads the history before sending again (ADR-0028) |

  A read of the history follows the same table, except that a 404 means that the PSP
  holds nothing for the payment: the lookup's answer to send again.
- **Backoff.** Before the *n*th retry the sender waits a random time between zero and
  `min(cap, base × 2^n)`: Brooker's "Full Jitter". The implementation sets `base` and
  `cap`, near a second and an hour, and phase 8 measures them.
- **No limit per request.** The SRE book says "Don't retry a given request indefinitely"
  for callers that wait; here the merchant already holds a 202, and the request is a
  durable promise. A request left unacknowledged too long is flagged for an operator.
- **One budget for the process.** A token bucket bounds how often the process retries,
  as the Builders' Library does, "limiting retries locally using a token bucket", and the
  SRE book's "server-wide retry budget". A long outage at the PSP therefore costs a steady
  trickle, not a storm when it recovers.
- **One point retries.** Only the sender and the sweep retry, under the one budget. The
  catch-up after a notification tries once and leaves a failure to the sweep, the HTTP
  client never retries (ADR-0036), and no caller above them does. Retries at several
  layers multiply: five layers of three tries give "243x" (the Builders' Library).
- **Timing lives in memory.** It decides how soon, never whether, so it needs no table.
  After a restart, due requests are spread by jitter rather than sent at once, and each
  resend is preceded by the lookup anyway. The step "send what is due at time *t*" takes
  its random source as an input, so a test seeds it (ADR-0029).
- **The fake's deliveries** follow the spec's example schedule, from at once to a day
  apart over three days, with jitter. It honours `Retry-After` and stops after the last
  attempt, after which the sweep finds what it gave up on.

## Consequences

- Notifications are answered in one short transaction, so the PSP's retries cover only
  the server's failures to record.
- A request can keep retrying for days, at the cap. A flagged request needs an operator.
- During an outage at the PSP, the token bucket bounds the load that the server adds.
- Stripe's API v1 replays a saved response for the same key, "including `500` errors", so
  sending again can never succeed there. The lookup before a resend copes, and the fake
  does not model it.
- Tests: each answer of the table, both ways; a `23505` between two copies of one
  notification; the server killed between recording a notification and catching up; the
  sender against 429, 503 and a hung PSP; the token bucket under an outage; the delay's
  bounds, as a property of the pure function.

## References

- Marc Brooker, [Timeouts, retries, and backoff with jitter](https://builder.aws.com/content/3EumjoZascWd1oZiEgL8ORlv3qE/timeouts-retries-and-backoff-with-jitter) (Amazon Builders' Library, 2019).
- Marc Brooker, [Exponential Backoff And Jitter](https://aws.amazon.com/blogs/architecture/exponential-backoff-and-jitter/) (AWS Architecture Blog, 2015).
- Alejandro Forero Cuervo, [Handling Overload](https://sre.google/sre-book/handling-overload/), and Mike Ulrich, [Addressing Cascading Failures](https://sre.google/sre-book/addressing-cascading-failures/): chapters 21 and 22 of Site Reliability Engineering (O'Reilly).
- Nathan Bronson, Abutalib Aghayev, Aleksey Charapko and Timothy Zhu, [Metastable Failures in Distributed Systems](https://doi.org/10.1145/3458336.3465286) (HotOS, 2021).
- Lexiang Huang et al., [Metastable Failures in the Wild](https://www.usenix.org/conference/osdi22/presentation/huang-lexiang) (OSDI, 2022).
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): status codes, timeouts and the example schedule.
- Adyen, [Handle webhook events](https://docs.adyen.com/development-resources/webhooks/handle-webhook-events) (docs); Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks).
- R. Fielding, M. Nottingham and J. Reschke, [RFC 9110: HTTP Semantics](https://www.rfc-editor.org/rfc/rfc9110.html) (IETF, 2022), sections 15.3.3 (202) and 10.2.3 (`Retry-After`).
- M. Nottingham and R. Fielding, [RFC 6585: Additional HTTP Status Codes](https://www.rfc-editor.org/rfc/rfc6585) (IETF, 2012), section 4 (429).
