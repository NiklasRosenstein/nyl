use thiserror::Error;

/// Git-specific errors
#[derive(Error, Debug)]
pub enum GitError {
    #[error("Git repository error: {0}")]
    Repository(#[from] git2::Error),

    #[error("Failed to clone {url}: {source}")]
    CloneFailed {
        url: String,
        #[source]
        source: git2::Error,
    },

    #[error("Reference '{ref_name}' not found in repository")]
    RefNotFound { ref_name: String },

    #[error("Worktree operation failed: {0}")]
    WorktreeFailed(String),

    #[error("Command execution failed: {0}")]
    Command(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid Git URL: {0}")]
    InvalidUrl(String),

    #[error("Object {oid} not found in repository")]
    ObjectNotFound { oid: String },

    #[error("Authentication failed for {url}: {reason}")]
    AuthenticationFailed { url: String, reason: String },

    #[error("No credentials found for repository: {url}")]
    CredentialNotFound { url: String },

    #[error("Failed to query ArgoCD secrets: {0}")]
    ArgoCDSecretQueryFailed(String),

    #[error("Invalid credential format in secret {secret_name}: {reason}")]
    InvalidCredentialFormat { secret_name: String, reason: String },
}

pub type Result<T> = std::result::Result<T, GitError>;
