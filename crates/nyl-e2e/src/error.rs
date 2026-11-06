//! The suite's error type: a message that names what failed, with the context
//! of every layer it passed through. Failures here are read by people in CI
//! logs, so a message carries the command, its output, and the last status a
//! wait saw instead of a code.

use std::fmt;

/// A failed step. `pending` marks a variant that is declared but not wired yet,
/// which the binary reports with its own exit code instead of a failure.
#[derive(Debug)]
pub struct Error {
    message: String,
    pending: bool,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            pending: false,
        }
    }

    /// A variant that cannot run yet; `message` says what it waits for.
    pub fn pending(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            pending: true,
        }
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// Prefix the message with what the caller was doing.
    pub fn context(mut self, context: impl fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// `result.context("what we were doing")` for any displayable error or an
/// absent value.
pub trait Context<T> {
    fn context(self, context: impl fmt::Display) -> Result<T>;
}

impl<T, E: fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, context: impl fmt::Display) -> Result<T> {
        self.map_err(|error| Error::new(format!("{context}: {error}")))
    }
}

impl<T> Context<T> for Option<T> {
    fn context(self, context: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| Error::new(context.to_string()))
    }
}

/// Return early with a formatted [`Error`].
#[macro_export]
macro_rules! bail {
    ($($argument:tt)*) => {
        return Err($crate::error::Error::new(format!($($argument)*)))
    };
}

/// Return early with a formatted [`Error`] unless the condition holds.
#[macro_export]
macro_rules! ensure {
    ($condition:expr, $($argument:tt)*) => {
        if !$condition {
            $crate::bail!($($argument)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_prefixes_the_message_of_every_layer() {
        let inner: Result<()> = Err(Error::new("connection refused"));
        let error = inner
            .map_err(|error| error.context("read the Argo CD version"))
            .map_err(|error| error.context("install Argo CD"))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "install Argo CD: read the Argo CD version: connection refused"
        );
        assert!(!error.is_pending());
    }

    #[test]
    fn test_pending_survives_added_context() {
        let error = Error::pending("not wired").context("variant direct");
        assert!(error.is_pending());
        assert_eq!(error.to_string(), "variant direct: not wired");
    }
}
