#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

//! Nyl - Kubernetes manifest generator with Helm integration
//!
//! This is the Rust rewrite of the Python-based nyl tool, focusing on
//! performance and clean architecture.

pub mod cli;
pub mod config;
pub mod error;
pub mod generator;
pub mod kubernetes;
pub mod resources;
pub mod template;
pub mod util;

// Re-export commonly used types
pub use error::{NylError, Result};
