//! ArgoCD repository secret discovery
//!
//! This module queries Kubernetes for ArgoCD repository secrets and extracts
//! Git credentials for authentication.

use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::auth::GitCredential;
use super::error::{GitError, Result};

/// ArgoCD credential discovery from Kubernetes secrets
pub struct ArgoCDCredentialDiscovery {
    client: Client,
    secret_cache: Arc<Mutex<HashMap<String, Secret>>>,
}

impl ArgoCDCredentialDiscovery {
    /// Create a new ArgoCD credential discovery client
    pub async fn new(client: Client) -> Result<Self> {
        Ok(Self {
            client,
            secret_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Discover all credentials from ArgoCD repository secrets
    pub async fn discover_credentials(&self) -> Result<HashMap<String, GitCredential>> {
        let secrets = self.query_repository_secrets().await?;
        let mut credentials = HashMap::new();

        for (name, secret) in secrets {
            match self.extract_credential_from_secret(&name, &secret) {
                Ok(Some((url, credential))) => {
                    credentials.insert(url, credential);
                }
                Ok(None) => {
                    // Secret doesn't have complete credential data, skip
                    tracing::debug!("Skipping secret {} - incomplete credential data", name);
                }
                Err(e) => {
                    // Log error but continue with other secrets
                    tracing::warn!("Failed to extract credential from secret {}: {}", name, e);
                }
            }
        }

        Ok(credentials)
    }

    /// Find credential for a specific URL with proper precedence:
    /// 1. Exact match in repository secrets
    /// 2. Pattern match in repo-creds secrets
    /// 3. Hostname fallback in repository secrets
    pub async fn find_credential_for_url(&self, url: &str) -> Result<Option<GitCredential>> {
        let secrets = self.query_repository_secrets().await?;

        let mut exact_match: Option<GitCredential> = None;
        let mut pattern_match: Option<GitCredential> = None;
        let mut hostname_match: Option<GitCredential> = None;

        for (name, secret) in secrets.iter() {
            // Get secret type from label
            let secret_type = secret
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("argocd.argoproj.io/secret-type"))
                .map(|s| s.as_str())
                .unwrap_or("unknown");

            if let Ok(Some((secret_url, credential))) =
                self.extract_credential_from_secret(name, secret)
            {
                match secret_type {
                    "repository" => {
                        // Check for exact match
                        let normalized_secret = normalize_url_for_matching(&secret_url);
                        let normalized_requested = normalize_url_for_matching(url);

                        if normalized_secret == normalized_requested {
                            exact_match = Some(credential);
                        } else if matches_repository_url(&secret_url, url)
                            && hostname_match.is_none()
                        {
                            // Hostname fallback
                            hostname_match = Some(credential);
                        }
                    }
                    "repo-creds" => {
                        // Check for pattern match
                        if matches_repo_creds_pattern(&secret_url, url) && pattern_match.is_none()
                        {
                            pattern_match = Some(credential);
                        }
                    }
                    _ => {
                        tracing::debug!("Unknown secret type: {}", secret_type);
                    }
                }
            }
        }

        // Return with precedence: exact > pattern > hostname
        Ok(exact_match.or(pattern_match).or(hostname_match))
    }

    /// Query all ArgoCD repository secrets from Kubernetes
    /// Queries both 'repository' and 'repo-creds' secret types
    async fn query_repository_secrets(&self) -> Result<HashMap<String, Secret>> {
        // Check cache first
        {
            let cache = self.secret_cache.lock().unwrap();
            if !cache.is_empty() {
                return Ok(cache.clone());
            }
        }

        let secrets_api: Api<Secret> = Api::namespaced(self.client.clone(), "argocd");

        // Query repository secrets
        let repo_lp = kube::api::ListParams::default()
            .labels("argocd.argoproj.io/secret-type=repository");
        let repo_secrets = secrets_api
            .list(&repo_lp)
            .await
            .map_err(|e| GitError::ArgoCDSecretQueryFailed(e.to_string()))?;

        // Query repo-creds secrets
        let creds_lp = kube::api::ListParams::default()
            .labels("argocd.argoproj.io/secret-type=repo-creds");
        let creds_secrets = secrets_api
            .list(&creds_lp)
            .await
            .map_err(|e| GitError::ArgoCDSecretQueryFailed(e.to_string()))?;

        // Combine both sets of secrets, filtering for type="git" only
        let mut secrets = HashMap::new();
        for secret in repo_secrets
            .items
            .into_iter()
            .chain(creds_secrets.items.into_iter())
        {
            // Check if secret type is "git" (or not specified, which defaults to "git")
            let repo_type = secret
                .data
                .as_ref()
                .and_then(|d| d.get("type"))
                .and_then(|t| String::from_utf8(t.0.clone()).ok())
                .unwrap_or_else(|| "git".to_string());

            if repo_type == "git" {
                if let Some(name) = secret.metadata.name.clone() {
                    secrets.insert(name, secret);
                }
            } else {
                tracing::debug!(
                    "Skipping secret {} with type={}",
                    secret.metadata.name.as_deref().unwrap_or("unknown"),
                    repo_type
                );
            }
        }

        // Update cache
        {
            let mut cache = self.secret_cache.lock().unwrap();
            *cache = secrets.clone();
        }

        Ok(secrets)
    }

    /// Extract credential from ArgoCD secret
    fn extract_credential_from_secret(
        &self,
        secret_name: &str,
        secret: &Secret,
    ) -> Result<Option<(String, GitCredential)>> {
        let data = match &secret.data {
            Some(data) => data,
            None => return Ok(None),
        };

        // Get repository URL
        let url = match data.get("url") {
            Some(url_bytes) => decode_base64_string(url_bytes)?,
            None => return Ok(None),
        };

        // Determine authentication type and extract credentials
        if let Some(ssh_private_key_bytes) = data.get("sshPrivateKey") {
            // SSH authentication
            let private_key = decode_base64_string(ssh_private_key_bytes)?;
            let username = data
                .get("username")
                .and_then(|u| decode_base64_string(u).ok())
                .unwrap_or_else(|| "git".to_string());

            let credential = GitCredential::SshKey {
                username,
                private_key,
                public_key: None,
                passphrase: None,
            };

            Ok(Some((url, credential)))
        } else if let Some(username_bytes) = data.get("username") {
            // HTTPS authentication
            let username = decode_base64_string(username_bytes)?;
            let token = match data.get("password") {
                Some(password_bytes) => decode_base64_string(password_bytes)?,
                None => {
                    return Err(GitError::InvalidCredentialFormat {
                        secret_name: secret_name.to_string(),
                        reason: "HTTPS credential missing password field".to_string(),
                    })
                }
            };

            let credential = GitCredential::HttpsToken { username, token };

            Ok(Some((url, credential)))
        } else {
            // No valid credential data
            Ok(None)
        }
    }
}

/// Decode base64 string from Kubernetes secret data
fn decode_base64_string(bytes: &k8s_openapi::ByteString) -> Result<String> {
    String::from_utf8(bytes.0.clone()).map_err(|e| GitError::InvalidCredentialFormat {
        secret_name: "unknown".to_string(),
        reason: format!("Invalid UTF-8 in base64 data: {}", e),
    })
}

/// Check if two repository URLs match
pub fn matches_repository_url(secret_url: &str, requested_url: &str) -> bool {
    let normalized_secret = normalize_url_for_matching(secret_url);
    let normalized_requested = normalize_url_for_matching(requested_url);

    // Exact match
    if normalized_secret == normalized_requested {
        return true;
    }

    // Hostname match as fallback
    extract_hostname_for_matching(&normalized_secret)
        .zip(extract_hostname_for_matching(&normalized_requested))
        .map(|(h1, h2)| h1 == h2)
        .unwrap_or(false)
}

/// Check if a repository URL matches a repo-creds pattern
/// Patterns use wildcards (*) to match multiple repositories
/// Example: https://github.com/myorg/* matches https://github.com/myorg/repo1
pub fn matches_repo_creds_pattern(pattern: &str, url: &str) -> bool {
    let normalized_pattern = normalize_url_for_matching(pattern);
    let normalized_url = normalize_url_for_matching(url);

    // Convert wildcard pattern to regex
    // Pattern: https://github.com/myorg/*
    // Regex:   ^https://github\.com/myorg/.*$

    // Escape regex special chars except *
    // Use placeholder \x00 for * during escaping, then replace with .*
    let escaped = regex::escape(&normalized_pattern.replace("*", "\x00")).replace("\x00", ".*");

    let regex_pattern = format!("^{}$", escaped);

    if let Ok(re) = regex::Regex::new(&regex_pattern) {
        re.is_match(&normalized_url)
    } else {
        tracing::warn!("Invalid regex pattern generated from: {}", pattern);
        false
    }
}

/// Normalize URL for matching
fn normalize_url_for_matching(url: &str) -> String {
    let mut normalized = url.to_lowercase();

    // Remove .git suffix
    if normalized.ends_with(".git") {
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

    // Remove trailing slash for consistency
    if normalized.ends_with('/') {
        normalized = normalized[..normalized.len() - 1].to_string();
    }

    normalized
}

/// Extract hostname from URL for matching
fn extract_hostname_for_matching(url: &str) -> Option<String> {
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
        // Find the hostname (before next / or :)
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
    fn test_url_matching() {
        // Exact matches
        assert!(matches_repository_url(
            "https://github.com/org/repo",
            "https://github.com/org/repo"
        ));

        // .git suffix handling
        assert!(matches_repository_url(
            "https://github.com/org/repo.git",
            "https://github.com/org/repo"
        ));

        // SSH shorthand matching
        assert!(matches_repository_url(
            "git@github.com:org/repo",
            "ssh://git@github.com/org/repo"
        ));

        // Hostname fallback
        assert!(matches_repository_url(
            "https://github.com/org/repo1",
            "https://github.com/org/repo2"
        ));

        // Different hostnames should not match
        assert!(!matches_repository_url(
            "https://github.com/org/repo",
            "https://gitlab.com/org/repo"
        ));
    }

    #[test]
    fn test_normalize_url() {
        assert_eq!(
            normalize_url_for_matching("https://github.com/org/repo.git"),
            "https://github.com/org/repo"
        );
        assert_eq!(
            normalize_url_for_matching("git@github.com:org/repo.git"),
            "ssh://git@github.com/org/repo"
        );
    }

    #[test]
    fn test_extract_hostname() {
        assert_eq!(
            extract_hostname_for_matching("https://github.com/org/repo"),
            Some("github.com".to_string())
        );
        assert_eq!(
            extract_hostname_for_matching("git@github.com:org/repo"),
            Some("github.com".to_string())
        );
        assert_eq!(
            extract_hostname_for_matching("ssh://git@gitlab.com/org/repo"),
            Some("gitlab.com".to_string())
        );
    }
}
