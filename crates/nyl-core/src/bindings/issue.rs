//! The problem a binding provider or the resolver reports.
//!
//! Contract: [Bindings](../../../../design/release-inputs.md#bindings), and the
//! orchestration core contract's stable codes. A provider that rejects a
//! binding knows why, so it states the stable code and the fix with the
//! message, and callers read them from the issue instead of recovering them
//! from the message text.

use crate::orchestration::codes::Code;

/// One problem found while resolving bindings or references.
///
/// `coded` is `None` when the reporter has no more specific diagnosis than the
/// message; a caller that reports with stable codes then applies its own
/// default. The resolver never reads it. A code and its fix travel together,
/// so an issue with one and not the other cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingIssue {
    pub message: String,
    /// The stable code and fix the provider reports the problem with.
    pub coded: Option<Coded>,
}

/// The stable code a provider reports a problem with, and what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coded {
    pub code: Code,
    pub fix: String,
}

impl BindingIssue {
    /// An issue with a message only.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            coded: None,
        }
    }

    /// An issue the provider reports with its own stable `code` and `fix`.
    pub fn coded(code: Code, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            coded: Some(Coded { code, fix: fix.into() }),
        }
    }
}

impl From<String> for BindingIssue {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl std::fmt::Display for BindingIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
