//! Wiring of orchestration: the concrete clock and identifier generator the
//! command line passes to the orchestration shell.
//!
//! Only this wiring reads the system clock and generates identifiers; every
//! decision receives them as inputs. See `design/implementation-architecture.md`.

pub mod clock;
pub mod ids;
pub mod project;
pub mod run;
pub mod session;

pub mod multi_environment;
pub mod secrets;

pub mod approval;

pub mod direct;
