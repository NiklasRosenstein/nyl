#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::redundant_else,
    clippy::doc_markdown,
    clippy::implicit_hasher,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::unnecessary_wraps,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::inefficient_to_string,
    clippy::derivable_impls
)]

//! Nyl rendering: Kubernetes manifest generation and rendered GitOps.
//!
//! This crate holds today's rendering pipeline. It never depends on
//! orchestration crates. See `design/implementation-architecture.md`.

pub mod components;
pub mod config;
pub mod editor;
pub mod error;
pub mod generator;
pub mod git;
pub mod gitops;
pub mod helm;
pub mod kubernetes;
pub mod postprocess;
pub mod render;
pub mod secrets;
pub mod template;
pub mod util;
#[doc(hidden)]
pub mod validation;
pub mod yaml;

pub use nyl_core::{constants, resources};

// Re-export commonly used types
pub use error::{NylError, Result};
