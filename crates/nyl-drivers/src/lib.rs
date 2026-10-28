#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions
)]

//! Nyl orchestration drivers.
//!
//! Implementations of [`Driver`](nyl_core::orchestration::driver::Driver):
//! the execution context, the process runner with secret masking, and the
//! built-in unit kinds. The Kubernetes adapter uses `nyl-render`; this crate
//! knows nothing about the state layout or orchestration decisions. See
//! `design/implementation-architecture.md`.

pub mod command;
mod dockerignore;
#[cfg(any(test, feature = "test-kinds"))]
pub mod fake;
pub mod oci_image;
pub mod process;
pub mod registry;
pub mod terraform;
