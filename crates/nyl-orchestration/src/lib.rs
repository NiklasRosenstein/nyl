#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions
)]

//! Nyl orchestration.
//!
//! Resolution, selection, lifecycle, recovery, teardown readiness, and the
//! shell that executes their decisions. Decisions are pure functions over
//! snapshots; only the shell performs effects, and only through the ports in
//! [`nyl_core::orchestration::ports`]. This crate depends on no other Nyl
//! crate than `nyl-core`, so the compiler rejects any direct Git, process, or
//! cluster call from here. See `design/implementation-architecture.md`.

pub mod fault;
pub mod report;
pub mod rules;
pub mod run_report;
pub mod shell;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transition;
