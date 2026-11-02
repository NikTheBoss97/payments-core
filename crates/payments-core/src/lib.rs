// Author: Nicola Antonello

//! payments-core: a payment processing backend built around a double-entry ledger.

pub mod accounts;
pub mod balances;
pub mod db;
pub mod ids;
pub mod ledger;
pub mod lifecycle;
pub mod money;
pub mod payments;
pub mod posting;
