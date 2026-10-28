<!-- Author: Nicola Antonello -->

# Idempotency and messaging

Idempotency keys, retries, transactional messaging and webhooks. Part of the [reading list](README.md).

- ★ Butler Lampson, [Hints and Principles for Computer System Design](https://arxiv.org/abs/2011.02455) (arXiv, 2021). The long revision of his 1983 "Hints". Section 3.6.2 states this project's retry rule in two pages: give an action a unique ID, remember the IDs of completed actions, and discard redundant retries. It also covers at-most-once versus exactly-once messaging and exponential backoff.
- ★ Brandur Leach, [Implementing Stripe-like Idempotency Keys in Postgres](https://brandur.org/idempotency-keys) (2017). A complete Postgres design for idempotency keys, including recovery points around calls to an external API.
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests) (API reference). A production contract: saved responses, an error when a key is reused with different parameters, and key expiry.
- Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks). Real delivery semantics: retries for up to three days, duplicates, and no ordering guarantee.
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0, 2023). Signed webhooks: message IDs for deduplication, timestamps against replay, and retries with backoff.
- Sebastian Burckhardt et al., [Durable Functions: Semantics for Stateful Serverless](https://doi.org/10.1145/3485510) (OOPSLA, 2021). A formal treatment of exactly-once processing on top of at-least-once execution: a worker commits new state together with its outgoing messages, workflows record and replay the results of external calls, and caller-chosen instance IDs filter duplicates.
- Marc Brooker, [Timeouts, retries, and backoff with jitter](https://builder.aws.com/content/3EumjoZascWd1oZiEgL8ORlv3qE/timeouts-retries-and-backoff-with-jitter) (Amazon Builders' Library, 2019). How retries amplify load, and why backoff needs jitter.
- Chris Richardson, [Microservices Patterns](https://www.manning.com/books/microservices-patterns) (Manning, 2018). Chapter 3 covers the transactional outbox and its relays; chapter 4 covers sagas.
- Pat Helland, [Idempotence Is Not a Medical Condition](https://doi.org/10.1145/2181796.2187821) (ACM Queue, 2012). Why messages between systems get retried, and why idempotence makes that safe.
- Pat Helland, [Life beyond Distributed Transactions: an Apostate's Opinion](https://www.cidrdb.org/cidr2007/papers/cidr07p15.pdf) (CIDR 2007). Commit each entity on its own, enqueue messages in the same transaction, and process them idempotently.
- Philip A. Bernstein, Meichun Hsu and Bruce Mann, [Implementing Recoverable Requests Using Queues](https://doi.org/10.1145/93597.98721) (SIGMOD, 1990). Exactly-once request processing: the client tags each request with an ID and, after a failure, asks which IDs the server last received and answered before resending anything, because many requests are not idempotent.
- J. H. Saltzer, D. P. Reed and D. D. Clark, [End-to-End Arguments in System Design](https://doi.org/10.1145/357401.357402) (ACM Transactions on Computer Systems, 1984). The origin of end-to-end duplicate suppression: retries made by the application look like new messages to the layers below, so only the application can detect them, and a lost acknowledgement turns a retry into such a duplicate.
