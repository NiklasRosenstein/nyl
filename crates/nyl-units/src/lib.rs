#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::module_name_repetitions
)]

//! Nyl's built-in unit kinds.
//!
//! Each kind's module holds its declaration (spec types and validation) and
//! its [`Driver`](nyl_core::orchestration::driver::Driver), so the rules of a
//! kind live in one place. [`catalog`] publishes their schema descriptors,
//! [`registry`] registers their drivers, and [`process`] is the runner with
//! secret masking that every driver executes tools through. This crate knows
//! nothing about the state layout or orchestration decisions; `nyl-render`
//! never depends on it, and the CLI passes the catalogue in. See
//! `design/implementation-architecture.md`.

pub mod catalog;
pub mod command;
#[cfg(any(test, feature = "test-kinds"))]
pub mod fake;
pub mod kubernetes_publication;
pub mod oci_image;
pub mod process;
pub mod registry;
pub mod terraform;
