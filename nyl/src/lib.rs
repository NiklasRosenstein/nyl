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
//! This crate is the `nyl` command line. It parses arguments, formats output,
//! and wires the rendering crates together; the rendering pipeline lives in
//! `nyl-render` and the shared resource types in `nyl-core`.

pub mod cli;

pub use nyl_render::{
    components, config, constants, editor, error, generator, git, gitops, helm, kubernetes, postprocess, render,
    resources, secrets, template, util, validation, yaml,
};

// Re-export commonly used types
pub use error::{NylError, Result};
