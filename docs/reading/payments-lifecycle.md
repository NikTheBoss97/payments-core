<!-- Author: Nicola Antonello -->

# Payments lifecycle

Payment statuses, authorization and capture, compensation and reconciliation. Part of the [reading list](README.md).

- ★ Adyen, [Payments lifecycle](https://docs.adyen.com/account/payments-lifecycle). Real payment statuses, including why a payment can no longer be cancelled once it is sent for settlement.
- Stripe, [Place a hold on a payment method](https://docs.stripe.com/payments/place-a-hold-on-a-payment-method). Separate authorization and capture, authorization expiry, and partial capture.
- TigerBeetle, [Two-Phase Transfers](https://docs.tigerbeetle.com/coding/two-phase-transfers/). Holds in a ledger: pending transfers that are later posted, voided or expired.
- Gregor Hohpe, [Your Coffee Shop Doesn't Use Two-Phase Commit](https://www.enterpriseintegrationpatterns.com/docs/IEEE_Software_Design_2PC.pdf) (IEEE Software, 2005). Error handling without distributed transactions: write-off, retry, compensation and reconciliation.
- Hector Garcia-Molina and Kenneth Salem, [Sagas](https://doi.org/10.1145/38713.38742) (SIGMOD, 1987). Long-lived work as a sequence of transactions, with compensating transactions to undo a partial run.
