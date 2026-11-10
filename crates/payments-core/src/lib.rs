// Author: Nicola Antonello

//! payments-core: a payment processing backend built around a double-entry ledger.

pub mod accounts;
pub mod api;
pub mod balances;
pub mod catch_up;
pub mod credentials;
pub mod db;
pub mod idempotency;
pub mod ids;
pub mod ledger;
pub mod lifecycle;
pub mod money;
pub mod notifications;
pub mod operator;
pub mod payments;
pub mod posting;
pub mod psp;
pub mod requests;
pub mod retries;
pub mod sender;
pub mod tenancy;
pub mod times;
