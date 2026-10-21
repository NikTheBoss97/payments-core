<!-- Author: Nicola Antonello -->

# Idempotency and messaging

Idempotency keys, retries, transactional messaging and webhooks. Part of the [reading list](README.md).

- ★ Brandur Leach, [Implementing Stripe-like Idempotency Keys in Postgres](https://brandur.org/idempotency-keys) (2017). A complete Postgres design for idempotency keys, including recovery points around calls to an external API.
- Stripe, [Idempotent requests](https://docs.stripe.com/api/idempotent_requests) (API reference). A production contract: saved responses, an error when a key is reused with different parameters, and key expiry.
- Pat Helland, [Idempotence Is Not a Medical Condition](https://doi.org/10.1145/2181796.2187821) (ACM Queue, 2012). Why messages between systems get retried, and why idempotence makes that safe.
- Pat Helland, [Life beyond Distributed Transactions: an Apostate's Opinion](https://www.cidrdb.org/cidr2007/papers/cidr07p15.pdf) (CIDR 2007). Commit each entity on its own, enqueue messages in the same transaction, and process them idempotently.
- Chris Richardson, [Microservices Patterns](https://www.manning.com/books/microservices-patterns) (Manning, 2018). Chapter 3 covers the transactional outbox and its relays; chapter 4 covers sagas.
- [Standard Webhooks specification](https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md) (v1.0.0, 2023). Signed webhooks: message IDs for deduplication, timestamps against replay, and retries with backoff.
- Stripe, [Receive Stripe events in your webhook endpoint](https://docs.stripe.com/webhooks). Real delivery semantics: retries for up to three days, duplicates, and no ordering guarantee.
- Marc Brooker, [Timeouts, retries, and backoff with jitter](https://builder.aws.com/content/3EumjoZascWd1oZiEgL8ORlv3qE/timeouts-retries-and-backoff-with-jitter) (Amazon Builders' Library, 2019). How retries amplify load, and why backoff needs jitter.
