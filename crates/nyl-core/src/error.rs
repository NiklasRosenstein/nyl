//! Error type of the core crate.
//!
//! Core code performs no effects, so its failures are configuration problems
//! with the declared resources. Crates that report errors to users convert
//! `CoreError` into their own error type without changing its message.

use thiserror::Error;

/// Error returned by resource parsing and validation.
#[derive(Error, Debug)]
pub enum CoreError {
    /// A resource or configuration value is invalid.
    #[error("{0}")]
    Config(String),

    /// A value could not be converted to or from JSON.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

/// Result type alias for core operations.
pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    /// Create a configuration error.
    pub fn config(msg: impl Into<String>) -> Self {
        CoreError::Config(msg.into())
    }
}
