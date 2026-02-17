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

//! Nyl - Kubernetes manifest generator with Helm integration
//!
//! This is the Rust rewrite of the Python-based nyl tool, focusing on
//! performance and clean architecture.

pub mod cli;
pub mod components;
pub mod config;
pub mod constants;
pub mod error;
pub mod generator;
pub mod git;
pub mod helm;
pub mod kubernetes;
pub mod postprocess;
pub mod profiles;
pub mod resources;
pub mod secrets;
pub mod template;
pub mod util;
pub mod yaml;

// Re-export commonly used types
pub use error::{NylError, Result};
