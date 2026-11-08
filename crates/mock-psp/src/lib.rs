// Author: Nicola Antonello

//! A fake payment service provider (PSP) that the tests of `payments-core` drive
//! (ADR-0029): a library, the fake, and a thin binary that serves it for local runs.
//!
//! It shares no code with the server, only the wire format: requests, histories and
//! signed notifications. A misunderstanding of that format then fails a test, instead
//! of hiding in code that both sides share.

pub mod signing;
