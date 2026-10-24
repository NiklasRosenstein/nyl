//! Orchestration types and traits: identifiers, state documents, the driver
//! interface, and the ports through which the orchestration shell performs
//! effects.
//!
//! Contract: [Orchestration core](../../../../design/orchestration-core.md).
//! This module holds data and trait definitions only; the rules live in
//! `nyl-orchestration`, the Git state store in `nyl-state`, and the drivers
//! in `nyl-drivers`.

pub mod codes;
pub mod driver;
pub mod ids;
pub mod ports;
pub mod state;
pub mod time;
