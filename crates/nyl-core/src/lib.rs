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

//! Nyl core: resource types, schemas, and pure helpers.
//!
//! This crate holds the types and rules that every other Nyl crate shares. It
//! performs no effects: no Git, processes, network, clock, or process
//! environment. See `design/implementation-architecture.md`.

pub mod constants;
pub mod digest;
pub mod error;
pub mod json_pointer;
pub mod local_path;
pub mod resources;
pub mod settings;

pub use error::{CoreError, Result};
