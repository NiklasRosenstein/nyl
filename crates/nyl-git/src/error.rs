//! Errors of remote Git operations.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GitError {
    #[error("Command execution failed: {0}\nHint: Ensure git is installed and available in PATH.")]
    Command(String),

    #[error("{operation} failed for {url}: {message}\nHint: Nyl runs git for remote access, with your SSH config, SSH agent, and credential helpers, plus any [[git.credentials]] in nyl.toml; check that `git ls-remote {url}` succeeds in the same environment.")]
    RemoteFailed {
        operation: String,
        url: String,
        message: String,
    },

    /// The remote refused to update refs; with an atomic push, none changed.
    #[error("git push to {url} updated nothing: {}", rejections.iter().map(|rejection| format!("{} ({})", rejection.reference, rejection.reason)).collect::<Vec<_>>().join(", "))]
    Rejected { url: String, rejections: Vec<Rejection> },

    /// A configured credential cannot be used, such as an unset variable.
    #[error("{0}")]
    Credentials(String),
}

/// One ref a push did not update, from `git push --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub reference: String,
    /// Git's reason, such as `stale info`, `non-fast-forward`, or `atomic push failed`.
    pub reason: String,
}

impl Rejection {
    /// The ref was not at the value the push expected: a lost compare-and-swap.
    pub fn is_stale(&self) -> bool {
        self.reason == "stale info" || self.reason == "fetch first" || self.reason.starts_with("non-fast-forward")
    }
}

impl GitError {
    /// Whether a push lost a compare-and-swap: some ref was not at its expected
    /// value. Other refs of an atomic push are then rejected as collateral.
    pub fn is_stale(&self) -> bool {
        matches!(self, Self::Rejected { rejections, .. } if rejections.iter().any(Rejection::is_stale))
    }
}

pub type Result<T> = std::result::Result<T, GitError>;
