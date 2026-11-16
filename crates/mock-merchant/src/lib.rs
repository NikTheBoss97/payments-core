// Author: Nicola Antonello

//! A fake merchant that receives the events of `payments-core` (ADR-0054, ADR-0056).
//!
//! It shares no code with the server, only the wire format: a thin event (ADR-0049),
//! signed as Standard Webhooks `v1` under its endpoint's secret (ADR-0051). A
//! misunderstanding of that format then fails a test, instead of hiding in code that both
//! sides share. [`signature`] checks a delivery as a merchant's endpoint does, and
//! [`event`] reads its body.

pub mod event;
pub mod signature;
