// Author: Nicola Antonello

//! payments-core: a payment processing backend built around a double-entry ledger.

pub mod accounts;
pub mod balances;
pub mod credentials;
pub mod db;
pub mod idempotency;
pub mod ids;
pub mod ledger;
pub mod lifecycle;
pub mod money;
pub mod operator;
pub mod payments;
pub mod posting;
pub mod tenancy;
pub mod times;
