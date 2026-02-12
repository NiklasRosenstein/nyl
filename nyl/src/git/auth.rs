//! Git authentication and credential management
//!
//! This module provides credential types and git2 callback builders for authenticating
//! with private Git repositories.

use git2::{Cred, CredentialType, RemoteCallbacks};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Git credential types for authentication
#[derive(Clone)]
pub enum GitCredential {
    /// SSH key authentication
    SshKey {
        username: String,
        private_key: String, // PEM format
        public_key: Option<String>,
        passphrase: Option<String>,
    },
    /// HTTPS token authentication
    HttpsToken {
        username: String,
        token: String, // Personal access token or password
    },
    /// SSH agent authentication (use SSH agent for key)
    SshAgent { username: String },
}

// Custom Debug implementation that redacts sensitive data
impl std::fmt::Debug for GitCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitCredential::SshKey { username, .. } => f
                .debug_struct("SshKey")
                .field("username", username)
                .field("private_key", &"<redacted>")
                .field("public_key", &"<redacted>")
                .field("passphrase", &"<redacted>")
                .finish(),
            GitCredential::HttpsToken { username, .. } => f
                .debug_struct("HttpsToken")
                .field("username", username)
                .field("token", &"<redacted>")
                .finish(),
            GitCredential::SshAgent { username } => f.debug_struct("SshAgent").field("username", username).finish(),
        }
    }
}

impl GitCredential {
    /// Convert credential to git2::Cred for authentication
    fn to_git2_cred(&self, _url: &str, username_from_url: Option<&str>) -> std::result::Result<Cred, git2::Error> {
        match self {
            GitCredential::SshKey {
                username,
                private_key,
                public_key,
                passphrase,
            } => {
                let username = username_from_url.unwrap_or(username);
                Cred::ssh_key_from_memory(username, public_key.as_deref(), private_key, passphrase.as_deref())
            }
            GitCredential::HttpsToken { username, token } => Cred::userpass_plaintext(username, token),
            GitCredential::SshAgent { username } => {
                let username = username_from_url.unwrap_or(username);
                Cred::ssh_key_from_agent(username)
            }
        }
    }
}

/// Provides credentials for Git operations
pub struct CredentialProvider {
    credentials: Arc<Mutex<HashMap<String, GitCredential>>>,
}

// Custom Debug implementation that doesn't expose credential values
impl std::fmt::Debug for CredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let creds = self.credentials.lock().unwrap();
        f.debug_struct("CredentialProvider")
            .field("credential_count", &creds.len())
            .field("urls", &creds.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CredentialProvider {
    /// Create a new credential provider
    pub fn new() -> Self {
        Self {
            credentials: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a credential provider with pre-loaded credentials
    pub fn with_credentials(credentials: HashMap<String, GitCredential>) -> Self {
        Self {
            credentials: Arc::new(Mutex::new(credentials)),
        }
    }

    /// Add a credential for a specific URL
    pub fn add_credential(&self, url: String, credential: GitCredential) {
        let mut creds = self.credentials.lock().unwrap();
        creds.insert(url, credential);
    }

    /// Get credential for a URL (exact match or hostname fallback)
    pub fn get_credential(&self, url: &str) -> Option<GitCredential> {
        let creds = self.credentials.lock().unwrap();

        // Try exact match first
        if let Some(cred) = creds.get(url) {
            tracing::debug!(
                "Matched Git credential via exact URL for {} (type={})",
                url,
                Self::credential_kind(cred)
            );
            return Some(cred.clone());
        }

        // Try hostname-based fallback
        let normalized_url = normalize_git_url(url);
        if let Some(host) = extract_hostname(&normalized_url) {
            for (stored_url, credential) in creds.iter() {
                if let Some(stored_host) = extract_hostname(stored_url) {
                    if host == stored_host {
                        tracing::debug!(
                            "Matched Git credential via hostname fallback for {} using stored URL {} (type={})",
                            url,
                            stored_url,
                            Self::credential_kind(credential)
                        );
                        return Some(credential.clone());
                    }
                }
            }
        }

        tracing::debug!("No stored Git credential matched URL {}", url);
        None
    }

    /// Build git2 RemoteCallbacks with credential handling
    pub fn build_callbacks<'a>(&self, url: &str) -> RemoteCallbacks<'a> {
        let credential = self.get_credential(url);

        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |url, username_from_url, allowed_types| {
            // If we have a stored credential, use it
            if let Some(ref cred) = credential {
                tracing::trace!(
                    "Using stored Git credential for {} (type={}, username_from_url={})",
                    url,
                    Self::credential_kind(cred),
                    username_from_url.unwrap_or("<none>")
                );
                return cred.to_git2_cred(url, username_from_url);
            }

            // Fallback: Try SSH agent for SSH URLs
            if allowed_types.contains(CredentialType::SSH_KEY) {
                let username = username_from_url.unwrap_or("git");
                if let Ok(cred) = Cred::ssh_key_from_agent(username) {
                    tracing::trace!("Using SSH agent fallback for {} as {}", url, username);
                    return Ok(cred);
                }
            }

            // No credential available
            tracing::trace!("No Git credentials available for {}", url);
            Err(git2::Error::from_str("No credentials available"))
        });

        callbacks
    }

    fn credential_kind(credential: &GitCredential) -> &'static str {
        match credential {
            GitCredential::SshKey { .. } => "ssh-key",
            GitCredential::HttpsToken { .. } => "https-token",
            GitCredential::SshAgent { .. } => "ssh-agent",
        }
    }
}

impl Default for CredentialProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize Git URL for comparison
fn normalize_git_url(url: &str) -> String {
    let mut normalized = url.to_lowercase();

    // Remove .git suffix (case-insensitive check)
    if std::path::Path::new(&normalized)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
    {
        normalized = normalized[..normalized.len() - 4].to_string();
    }

    // Convert SSH shorthand to full URL
    // git@github.com:org/repo -> ssh://git@github.com/org/repo
    if let Some(at_pos) = normalized.find('@') {
        if !normalized.starts_with("http") && !normalized.starts_with("ssh://") {
            if let Some(colon_pos) = normalized[at_pos..].find(':') {
                let username_host = &normalized[..at_pos + colon_pos];
                let path = &normalized[at_pos + colon_pos + 1..];
                normalized = format!("ssh://{}/{}", username_host, path);
            }
        }
    }

    // Ensure trailing slash for consistency
    if !normalized.ends_with('/') {
        normalized.push('/');
    }

    normalized
}

/// Extract hostname from a Git URL
fn extract_hostname(url: &str) -> Option<String> {
    // Handle SSH shorthand: git@github.com:org/repo
    if let Some(at_pos) = url.find('@') {
        if !url.starts_with("http") && !url.starts_with("ssh://") {
            if let Some(colon_pos) = url[at_pos..].find(':') {
                let host = &url[at_pos + 1..at_pos + colon_pos];
                return Some(host.to_lowercase());
            }
        }
    }

    // Handle full URLs: https://github.com/org/repo or ssh://git@github.com/org/repo
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        // Find the hostname (before next / or @)
        let host_end = after_scheme
            .find('/')
            .or_else(|| after_scheme.find(':'))
            .unwrap_or(after_scheme.len());
        let host_part = &after_scheme[..host_end];

        // Remove username if present (git@hostname -> hostname)
        if let Some(at_pos) = host_part.rfind('@') {
            return Some(host_part[at_pos + 1..].to_lowercase());
        } else {
            return Some(host_part.to_lowercase());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_git_url() {
        assert_eq!(
            normalize_git_url("https://github.com/org/repo.git"),
            "https://github.com/org/repo/"
        );
        assert_eq!(
            normalize_git_url("git@github.com:org/repo.git"),
            "ssh://git@github.com/org/repo/"
        );
        assert_eq!(
            normalize_git_url("ssh://git@gitlab.com/org/repo"),
            "ssh://git@gitlab.com/org/repo/"
        );
    }

    #[test]
    fn test_extract_hostname() {
        assert_eq!(
            extract_hostname("https://github.com/org/repo"),
            Some("github.com".to_string())
        );
        assert_eq!(
            extract_hostname("git@github.com:org/repo"),
            Some("github.com".to_string())
        );
        assert_eq!(
            extract_hostname("ssh://git@gitlab.com/org/repo"),
            Some("gitlab.com".to_string())
        );
    }

    #[test]
    fn test_credential_provider() {
        let provider = CredentialProvider::new();

        // Add SSH key credential
        provider.add_credential(
            "https://github.com/org/repo".to_string(),
            GitCredential::SshAgent {
                username: "git".to_string(),
            },
        );

        // Test exact match
        assert!(provider.get_credential("https://github.com/org/repo").is_some());

        // Test hostname fallback
        assert!(provider.get_credential("https://github.com/org/other-repo").is_some());
    }
}
