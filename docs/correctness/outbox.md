<!-- Author: Nicola Antonello -->

# Correctness: the outbox

The [README](../../README.md#correctness) lists every invariant of the project; this page shows how the outbox's are tested.

Phase 5 tells each merchant about its payments. Every fact of the PSP's that the catch-up applies
writes one event, and a delivery of it to each endpoint of the payment's merchant that is enabled
when the catch-up reads them, in the transaction that applies the fact. A relay sends each
delivery, signed under its endpoint's secret, until an answer of `2xx` or until the endpoint is
disabled, and records each success once. It learns of a delivery from the catch-up's handover
after the commit, or from a search of the transactions that its previous search saw unfinished,
so neither a lost handover nor a restart loses one. A merchant drops copies by `webhook-id`, so
that at least once reaches it as once.
[`docs/DESIGN.md`](../DESIGN.md#invariants) states how each invariant is enforced,
[ADR-0049](../adr/0049-thin-events-in-an-outbox.md) to
[ADR-0065](../adr/0065-how-the-outboxs-failure-tests-run.md) say why, and
[`docs/api.md`](../api.md#deliveries-to-merchants) says what a merchant can rely on.

| | Invariant | Tested by |
|---|---|---|
| I6 | A payment's state change, its ledger entries and its outbound event commit atomically. | The backend killed before and after the event's insert and its deliveries', and during `COMMIT`, as the catch-up applies an authorization, a void, a capture or a refund: nothing of the transaction stays, and the next catch-up writes one event and one delivery per endpoint. A `COMMIT` that commits and loses its session leaves one event per fact ([`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs)). Each fact that the catch-up applies has one event, and a creation, a cancellation and a stopping fact have none ([`writing_events.rs`](../../crates/payments-core/tests/writing_events.rs)). Each guard broken alone, below. |
| I7 | A merchant can read or change only its own payments, and is told only about its own. | An event goes to each enabled endpoint of its merchant, and to none of another merchant's or a disabled one ([`writing_events.rs`](../../crates/payments-core/tests/writing_events.rs)). Random runs in which each fake merchant receives only its own merchant's events ([`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs)). Each guard broken alone, below: the fan-out's filter on the merchant, the foreign keys behind it, and both at once. |
| I10 | Every event reaches every endpoint enabled when it was written, at least once, unless that endpoint is disabled first. | Random runs of catch-ups, some killed, some whose handover is lost and some held open across a search, with or without their handover, of the relay's steps and restarts, of records that the database refuses, of registrations and disablements, and of every kind of answer, against a model: every delivery ends with a success or a disabled endpoint, each disablement has its cause, the operator's choice or a `410`, and each success follows an answer that the relay could read ([`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs)). The server killed between a `2xx` and its record, after which the event arrives again under its `webhook-id` and one success is recorded ([`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs)). A handover that a full channel drops, found by the next search ([`handing_over.rs`](../../crates/payments-core/tests/handing_over.rs)), and an event whose transaction stays open, found once it commits ([`searching_deliveries.rs`](../../crates/payments-core/tests/searching_deliveries.rs)). The relay's rules against three models ([`relay.rs`](../../crates/payments-core/src/relay.rs)), and its task's answers, retries, disablements and failed records ([`relaying.rs`](../../crates/payments-core/tests/relaying.rs)). |

The outbox's flows also keep the invariants below them. The catch-up's crash tests end with every
currency summing to zero and no posting without a cause (I1), and check the money against
balances derived from the entries (I3).

The tests also attack the guards themselves:

- **A fact without its event.** As the system role, a fact written without its event is refused at `COMMIT` with `LG008`, and commits with it; a second event for a fact is refused by its key ([`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs)).
- **A delivery astray.** In raw SQL, a delivery to another merchant's endpoint, an event under another merchant than its payment's, and a delivery written in a later transaction than its event's are each refused by a foreign key ([`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs)).
- **The fan-out's filter.** With the catch-up's filter on the merchant removed, the foreign key refuses the catch-up's deliveries and the random runs fail. With the key removed as well, they fail on their own check, since the deliveries are no longer the model's ([`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs), each a planted bug in the pull request of [ADR-0065](../adr/0065-how-the-outboxs-failure-tests-run.md)).
- **A transaction's ID.** The database stamps the transaction of an event and of its deliveries, and neither role may write it ([`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs)).
- **The merchant's credential.** The merchant's role reads and writes no endpoint, event or delivery ([`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs), [`endpoints_schema.rs`](../../crates/payments-core/tests/endpoints_schema.rs)).
- **Rows that never change.** Neither role changes an endpoint, a disablement, an event, a delivery or a success, and the owner cannot either ([`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs), [`endpoints_schema.rs`](../../crates/payments-core/tests/endpoints_schema.rs)).
- **An endpoint's URL.** The database's check reads the host and the port as the route does, and a URL stored past both gets no answer ([`endpoints_schema.rs`](../../crates/payments-core/tests/endpoints_schema.rs), [`delivery_client.rs`](../../crates/payments-core/tests/delivery_client.rs)).
- **A signature.** Each attempt is signed afresh under its endpoint's secret, and the fake merchant checks it with code of its own, against the specification's test vector ([`signing_deliveries.rs`](../../crates/payments-core/tests/signing_deliveries.rs), [`delivery_client.rs`](../../crates/payments-core/tests/delivery_client.rs), [`mock-merchant`](../../crates/mock-merchant/src/signature.rs)).
- **What behaviour cannot show.** Catalog tests pin the roles' privileges on the outbox's tables, the merchant role's row-level security, and `LG008`'s trigger with its `ENABLE ALWAYS` ([`catalog.rs`](../../crates/payments-core/tests/catalog.rs)).

## Failures the outbox is tested against

| Failure | What happens | Test |
|---|---|---|
| The backend dies before or after an event's insert or its deliveries', or during the catch-up's `COMMIT` | Nothing of the transaction stays, and the next catch-up writes the event and its deliveries once | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A catch-up's `COMMIT` commits, then the session dies | One event per fact, and the next catch-up writes nothing | [`payment_crashes.rs`](../../crates/payments-core/tests/payment_crashes.rs) |
| A catch-up's `COMMIT` fails | Nothing is handed over | [`handing_over.rs`](../../crates/payments-core/tests/handing_over.rs) |
| A handover is lost, to a full channel or a restart | The next search finds the delivery | [`handing_over.rs`](../../crates/payments-core/tests/handing_over.rs), [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| A catch-up's transaction is open while the relay searches, and its handover may be lost | Its deliveries are found once it commits, through the next search's list of transactions in progress, and delivered | [`searching_deliveries.rs`](../../crates/payments-core/tests/searching_deliveries.rs), [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| A delivery's handover comes after a search found it, or a search finds it after its handover | The relay learns it once: with no token to retry, it attempts each delivery at most once, whether its handover comes at once, late or never | [`relay.rs`](../../crates/payments-core/src/relay.rs) |
| An endpoint answers an error, does not answer, or asks to wait | Tried again after a random delay, never shorter than a `Retry-After`, for a token of the relay's budget | [`delivery_client.rs`](../../crates/payments-core/tests/delivery_client.rs), [`relaying.rs`](../../crates/payments-core/tests/relaying.rs), [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| An endpoint takes an event and answers after the timeout | The event is sent again, and the merchant answers the copy `200` | [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| An endpoint answers `410` | It is disabled as gone, and its deliveries end | [`relaying.rs`](../../crates/payments-core/tests/relaying.rs) |
| An endpoint answers `500`, `503`, or after the timeout, for minutes | It is not disabled for it: short of three days, only the operator or a `410` disables an endpoint | [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| An endpoint fails for three days | It is disabled as failing | [`relaying.rs`](../../crates/payments-core/tests/relaying.rs) |
| The operator disables an endpoint | Its deliveries end, and an attempt that finds it disabled sends nothing | [`delivery_client.rs`](../../crates/payments-core/tests/delivery_client.rs), [`relaying.rs`](../../crates/payments-core/tests/relaying.rs), [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| The server dies between a `2xx` and its record | After a restart the event is sent again under its `webhook-id`, and one success is recorded | [`server_binary.rs`](../../crates/payments-core/tests/server_binary.rs) |
| The relay's task restarts with attempts in flight | Its first search reads every delivery not finished, as retries that wait for tokens, and each ends delivered or disabled | [`relaying.rs`](../../crates/payments-core/tests/relaying.rs), [`outbox_model.rs`](../../crates/payments-core/tests/outbox_model.rs) |
| The database refuses the record of a success | The relay holds the success, records it at a later step, and does not send it again | [`relaying.rs`](../../crates/payments-core/tests/relaying.rs) |
| A record's insert dies | Nothing is written, and the record is made again | [`recording_deliveries.rs`](../../crates/payments-core/tests/recording_deliveries.rs) |
| The database does not give an attempt its endpoint or its event's body | Not sent: the delivery waits like a retry, and its endpoint is not blamed | [`delivery_client.rs`](../../crates/payments-core/tests/delivery_client.rs), [`relaying.rs`](../../crates/payments-core/tests/relaying.rs) |
| An attempt panics | It is read as not sent, and the task runs on | [`relaying.rs`](../../crates/payments-core/tests/relaying.rs) |
| SQL run as the system role commits a fact without its event | `LG008` at `COMMIT`, which proves a rollback | [`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs) |
| SQL writes a delivery to another merchant's endpoint, or outside its event's transaction | Refused by its foreign key | [`events_schema.rs`](../../crates/payments-core/tests/events_schema.rs) |

## What these tests do not show

- **TLS and real networks.** Deliveries go over plain HTTP on the local interface, to fakes. An `https` endpoint waits for the build that adds TLS ([ADR-0050](../adr/0050-webhook-endpoints-registered-by-the-operator.md)).
- **Clocks that disagree.** A merchant whose clock stands more than five minutes from ours refuses every delivery, as the specification's window asks. The tests keep their times near the clock.
- **The three days among random runs.** The rule is tested on its own and in the relay's models, not in the random runs, whose times stay within the fakes' five minutes ([ADR-0065](../adr/0065-how-the-outboxs-failure-tests-run.md)).
- **A bound on attempts across endpoints.** Attempts are bounded per endpoint. Phase 8's benchmarks measure a burst across many and size any bound ([ADR-0063](../adr/0063-the-relays-task.md)).
- **PostgreSQL's own crash.** The tests kill sessions and the server, never the database, whose durability is its own.
- **Exactly once at a real merchant.** The service promises at least once. The fake drops copies by `webhook-id`, as [ADR-0054](../adr/0054-what-phase-5-promises-and-how-it-is-tested.md) asks of a merchant; a real merchant's handler does its own.

## What these tests have found

- **Two URLs that the guards let through.** Aligning the database's check with the route showed that the route took `http://[]/`, whose host is empty, and the check's own test found `http://:80/` stored ([ADR-0060](../adr/0060-refusing-an-endpoints-url.md), [ADR-0061](../adr/0061-the-url-check-reads-the-host-and-the-port.md)).
- **A success sent again.** Once the relay's random runs could refuse a record, they sent again a success whose record had failed. The relay now holds each success until it is recorded, and leaves its delivery meanwhile ([ADR-0063](../adr/0063-the-relays-task.md)).
- **A late handover.** A review found that a handover that came after a search had learned its delivery could send a delivered event again. Without tokens, the relay's random runs found the same interleaving by themselves at 4,000 cases, and each delivery is now learned once, by its transaction ([ADR-0064](../adr/0064-learning-each-delivery-by-its-transaction.md)).
- **A restart that changed every secret.** The binary's tests started a restarted server with a new master key, so the first run of the restart between a `2xx` and its record saw the merchant refuse every resend. A server keeps its master key across a restart, and the tests now do too.
- **Tests that tested too little.** A review found three tests that could not fail for the bug that they named: a transaction held open in another database, against a search from `xmin`; a body held back after its head; and the previous `xmax`, when nothing else took a transaction ID in between. Each now fails under a planted bug that it passed before. Another planted bug was caught only once a test owed two effects at once.
- **Random runs that checked too little, or too much.** A fresh review of the random runs found that they took any disablement as the end of a delivery, never tied a success to an answer that the relay could read, rarely put a held-open catch-up in a snapshot's list, and let the relay's draws shift a seed's operations. Each is now checked or fixed, and three bugs planted for it are caught: a `500` read as gone, a timeout read as delivered, and a search that ignores its list. The same review foresaw that a check on endpoints that answered `410` could fail with no bug, when a `410` came after the client's timeout; looping the runs 40 times failed it twice, and the check on each disablement's cause replaced it.
