<!-- Author: Nicola Antonello -->

# ADR-0056: The mock merchant's answers and checks

- Status: accepted; partly superseded by ADR-0065
- Date: 2026-11-16

## Context

ADR-0054 decides what the mock merchant is: a crate built like the mock PSP, one per
test on a random local port, whose answers a test scripts through its handle: a `2xx`,
an error, a `410`, a delay past the timeout, a `Retry-After`. It checks each signature
with code of its own, drops copies by `webhook-id`, and its handle returns what
arrived. It leaves open how a test scripts the answers, what the fake does with a
delivery that fails its checks, how it learns its secret, and when it counts an event as
taken.

Two facts bear on them. The relay sends up to 4 attempts at once to one endpoint, in no
promised order (ADR-0053). And hyper drops a handler's future when its client hangs up
(ADR-0023), so a fake that answers after the relay's timeout never runs the code that
follows its wait.

## Options

How a test scripts the answers:

| Option | Why not |
|---|---|
| **A standing answer, which the test changes between the relay's steps, and answers aimed at one payment's events, which win over it** | Chosen |
| A queue of answers taken in arrival order, then a `2xx`, as the mock PSP's failures are | Which of 4 attempts in flight gets which answer would race, and refusing one event while accepting the rest would need a rule anyway |
| Each delivery held until the test answers it, as the mock PSP holds requests (ADR-0038) | Exact, but every test would answer every arrival while the relay's step awaits it, and a forgotten one would turn into a timeout |

A delivery that fails the fake's checks:

| Option | Why not |
|---|---|
| **A `400`, recorded with its reason, and never taken** | Chosen |
| The scripted answer, with the failure recorded | A merchant does not act on a delivery that it cannot verify, and the relay would record a success for a forged or broken one |
| A panic | It would end the route's task, not the test |

How the fake learns its secret:

| Option | Why not |
|---|---|
| **The test hands it the secret that registering the endpoint answered** | Chosen |
| The fake derives it from the master key | No merchant holds the master key, and the fake would carry a second copy of ADR-0051's derivation, which RFC 5869's test case checks in the library |

## Decision

- **Answers.** A delivery that passes the checks and brings a new `webhook-id` gets the
  answer aimed at its payment, if a test aimed one, and the standing answer otherwise,
  a `200` at the start. An answer is a status, a status with `Retry-After` in seconds,
  or a delay before a `200`. The relay's step is a call that the test makes (ADR-0029),
  so a test changes the standing answer between steps, and every delivery of one step to
  one endpoint gets the same answer unless one is aimed. A stopped fake is a dead
  endpoint: it takes no new connection, and closes each one once it is idle.
- **Checks,** in ADR-0030's order, by code of the fake's own, a pure function of the
  headers, the bytes received, the secrets and the time:
  1. `webhook-id`, `webhook-timestamp` and `webhook-signature` each appear once, in
     visible ASCII;
  2. the ID is not empty and holds no `.`, and the timestamp holds only digits;
  3. the timestamp is within 5 minutes of the fake's clock, either way;
  4. one listed `v1` signature verifies under a secret that the fake trusts, over the
     bytes received, compared in constant time.

  Then the body, strictly (ADR-0049): `type` is `payment.updated`, `timestamp` is RFC
  3339, `data` holds `payment_id` and, only when present, `request_id`, and nothing else
  appears. A delivery that fails a check is a `400`, recorded with its reason, and
  never taken.
- **The true time.** The fake checks the timestamp against its own clock, as a
  merchant's library does. The relay therefore stamps each attempt with the true time,
  even when a test passes a later time to its step, as the mock PSP does (ADR-0038).
- **Taking an event,** ADR-0054's idempotent consumer. When a delivery's answer is a
  `2xx`, or a delay before one, the fake takes its event before answering, so an answer
  that comes after the relay's timeout still counts, as when a merchant acted on an event
  and its answer was lost. A delivery whose `webhook-id` was taken is a copy: answered
  `200` whatever the script, recorded, and not taken again. Copies are counted per fake,
  so during a rotation two endpoints at one URL (ADR-0051) give one event and one copy.
- **Secrets.** The test hands the fake the secret that registering the endpoint
  answered (ADR-0050), as an operator hands it to a merchant. The fake may trust several.
- **What the handle returns:** every arrival in order, with its time, its `webhook-id`
  and timestamp as they came, its body, its event once read, and what the fake made of
  it; the events taken, each once, in the order taken; and the most deliveries that it
  answered at once, for the relay's limit of 4 (ADR-0053).
- **One route,** `POST /webhooks`. Another path is a `404`, so a wrong URL fails a test.
- **The binary** reads `MOCK_MERCHANT_ADDRESS`, `127.0.0.1:4200` unless set, and
  `MOCK_MERCHANT_SECRETS`, the secrets that registering answered, separated by spaces.
  It stops at once on anything else, without repeating what it read. It answers `200` to
  each delivery that passes the checks, copies included, and logs one line per arrival,
  never a secret.
- **Dependencies:** only crates that the workspace builds already.

## Consequences

- A test that wants one event refused aims an answer at its payment; the rest get the
  standing answer.
- The relay's tests pass a time to its step, and its signatures still carry the true
  time.
- `payments-core` lists the crate under `[dev-dependencies]` with the first test that
  uses it, in the library's pull request.
- The fake's tests check its verifier against the specification's test vector, and
  sign their deliveries with HMAC code of their own.

## Amendments

- 2026-11-23: "The true time" and the consequence on the relay's tests are superseded by
  ADR-0065. The relay stamps each attempt with its step's time, including a time that a
  test passes; the running task reads the clock at each event, so the server's attempts
  carry the true time, and a test keeps the times that it passes within the fake's five
  minutes.

## References

- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0): the headers, the signature and its verification.
- Gerard Meszaros, [xUnit Test Patterns: Refactoring Test Code](http://xunitpatterns.com/) (Addison-Wesley, 2007): Configurable Test Double.
