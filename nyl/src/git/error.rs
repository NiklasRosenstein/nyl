use thiserror::Error;

/// Git-specific errors
#[derive(Error, Debug)]
pub enum GitError {
    #[error("Git repository error: {0}")]
    Repository(#[from] git2::Error),

    #[error("Failed to clone {url}: {source}\nHint: Check network connectivity and verify the repository URL. For private repos, ensure credentials are configured.")]
    CloneFailed {
        url: String,
        #[source]
        source: git2::Error,
    },

    #[error("Reference '{ref_name}' not found in repository\nHint: Check that the branch, tag, or commit exists. Use 'git ls-remote <url>' to list available refs.")]
    RefNotFound { ref_name: String },

    #[error(
        "Failed to fetch refs for {url}, and no cached ref '{ref_name}' was available: {fetch_error}\nHint: Verify repository credentials and network access, or use a resolvable targetRevision."
    )]
    FetchFailedNoCachedRef {
        url: String,
        ref_name: String,
        fetch_error: String,
    },

    #[error("Worktree operation failed: {0}\nHint: Ensure the worktree directory is writable and not already in use.")]
    WorktreeFailed(String),

    #[error("Command execution failed: {0}\nHint: Ensure git is installed and available in PATH.")]
    Command(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "Invalid Git URL: {0}\nHint: Use format 'https://github.com/user/repo.git' or 'git@github.com:user/repo.git'"
    )]
    InvalidUrl(String),

    #[error("Object {oid} not found in repository\nHint: The commit may not have been fetched. Try fetching latest changes.")]
    ObjectNotFound { oid: String },

    #[error("Authentication failed for {url}: {reason}\nHint: For SSH, ensure your SSH key is added to the agent and authorized on the server. For HTTPS, verify your credentials in ArgoCD repository secrets.")]
    AuthenticationFailed { url: String, reason: String },

    #[error("No credentials found for repository: {url}\nHint: Create an ArgoCD repository secret with label 'argocd.argoproj.io/secret-type=repository' containing SSH key or HTTPS credentials.")]
    CredentialNotFound { url: String },

    #[error("Failed to query ArgoCD secrets: {0}\nHint: Ensure you have permissions to read secrets in the 'argocd' namespace. Check RBAC configuration.")]
    ArgoCDSecretQueryFailed(String),

    #[error("Invalid credential format in secret {secret_name}: {reason}\nHint: Repository secrets must contain 'sshPrivateKey' for SSH or 'username'+'password' for HTTPS authentication.")]
    InvalidCredentialFormat { secret_name: String, reason: String },
}

pub type Result<T> = std::result::Result<T, GitError>;

impl GitError {
    /// Returns true if this error is authentication-related
    pub fn is_auth_error(&self) -> bool {
        matches!(
            self,
            GitError::AuthenticationFailed { .. } | GitError::CredentialNotFound { .. }
        )
    }

    /// Returns true if this error is likely transient (network issues)
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            GitError::CloneFailed { .. } | GitError::ArgoCDSecretQueryFailed(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_auth_error() {
        let auth_err = GitError::AuthenticationFailed {
            url: "https://example.com".to_string(),
            reason: "invalid key".to_string(),
        };
        assert!(auth_err.is_auth_error());

        let cred_err = GitError::CredentialNotFound {
            url: "https://example.com".to_string(),
        };
        assert!(cred_err.is_auth_error());

        let other_err = GitError::InvalidUrl("bad url".to_string());
        assert!(!other_err.is_auth_error());
    }

    #[test]
    fn test_is_transient() {
        let clone_err = GitError::CloneFailed {
            url: "https://example.com".to_string(),
            source: git2::Error::from_str("network error"),
        };
        assert!(clone_err.is_transient());

        let secret_err = GitError::ArgoCDSecretQueryFailed("timeout".to_string());
        assert!(secret_err.is_transient());

        let ref_err = GitError::RefNotFound {
            ref_name: "main".to_string(),
        };
        assert!(!ref_err.is_transient());
    }

    #[test]
    fn test_error_messages_include_hints() {
        let auth_err = GitError::AuthenticationFailed {
            url: "git@github.com:user/repo.git".to_string(),
            reason: "key rejected".to_string(),
        };
        let display = format!("{}", auth_err);
        assert!(display.contains("Hint:"));
        assert!(display.contains("SSH key"));

        let cred_err = GitError::CredentialNotFound {
            url: "https://github.com/user/repo.git".to_string(),
        };
        let display = format!("{}", cred_err);
        assert!(display.contains("Hint:"));
        assert!(display.contains("ArgoCD repository secret"));

        let ref_err = GitError::RefNotFound {
            ref_name: "feature-branch".to_string(),
        };
        let display = format!("{}", ref_err);
        assert!(display.contains("Hint:"));
        assert!(display.contains("git ls-remote"));

        let fetch_cache_err = GitError::FetchFailedNoCachedRef {
            url: "https://github.com/user/repo.git".to_string(),
            ref_name: "HEAD".to_string(),
            fetch_error: "git fetch failed: auth".to_string(),
        };
        let display = format!("{}", fetch_cache_err);
        assert!(display.contains("Hint:"));
        assert!(display.contains("credentials"));
        assert!(display.contains("HEAD"));
    }

    #[test]
    fn test_invalid_credential_format_error() {
        let err = GitError::InvalidCredentialFormat {
            secret_name: "my-secret".to_string(),
            reason: "missing sshPrivateKey field".to_string(),
        };
        let display = format!("{}", err);
        assert!(display.contains("my-secret"));
        assert!(display.contains("sshPrivateKey"));
        assert!(display.contains("Hint:"));
    }
}
