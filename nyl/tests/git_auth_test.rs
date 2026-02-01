//! Integration tests for Git authentication

use nyl::git::{CredentialProvider, GitCredential};
use std::collections::HashMap;

#[test]
fn test_credential_provider_creation() {
    let provider = CredentialProvider::new();
    assert!(provider.get_credential("https://github.com/org/repo").is_none());
}

#[test]
fn test_credential_provider_with_ssh_key() {
    let mut credentials = HashMap::new();
    credentials.insert(
        "https://github.com/org/repo".to_string(),
        GitCredential::SshKey {
            username: "git".to_string(),
            private_key: "test-key".to_string(),
            public_key: None,
            passphrase: None,
        },
    );

    let provider = CredentialProvider::with_credentials(credentials);

    // Test exact match
    assert!(provider.get_credential("https://github.com/org/repo").is_some());

    // Test hostname fallback
    assert!(provider.get_credential("https://github.com/org/other-repo").is_some());

    // Different hostname should not match
    assert!(provider.get_credential("https://gitlab.com/org/repo").is_none());
}

#[test]
fn test_credential_provider_with_https_token() {
    let mut credentials = HashMap::new();
    credentials.insert(
        "https://github.com/org/repo".to_string(),
        GitCredential::HttpsToken {
            username: "user".to_string(),
            token: "ghp_token123".to_string(),
        },
    );

    let provider = CredentialProvider::with_credentials(credentials);

    assert!(provider.get_credential("https://github.com/org/repo").is_some());
}

#[test]
fn test_credential_provider_with_ssh_agent() {
    let mut credentials = HashMap::new();
    credentials.insert(
        "git@github.com:org/repo".to_string(),
        GitCredential::SshAgent {
            username: "git".to_string(),
        },
    );

    let provider = CredentialProvider::with_credentials(credentials);

    assert!(provider.get_credential("git@github.com:org/repo").is_some());
}

#[test]
fn test_credential_provider_url_matching() {
    let mut credentials = HashMap::new();
    credentials.insert(
        "https://github.com/org/repo.git".to_string(),
        GitCredential::SshAgent {
            username: "git".to_string(),
        },
    );

    let provider = CredentialProvider::with_credentials(credentials);

    // Should match without .git suffix
    assert!(provider.get_credential("https://github.com/org/repo").is_some());

    // Should match with .git suffix
    assert!(provider.get_credential("https://github.com/org/repo.git").is_some());
}

#[test]
fn test_credential_provider_add_credential() {
    let provider = CredentialProvider::new();

    provider.add_credential(
        "https://github.com/org/repo".to_string(),
        GitCredential::SshAgent {
            username: "git".to_string(),
        },
    );

    assert!(provider.get_credential("https://github.com/org/repo").is_some());
}

#[test]
fn test_build_callbacks() {
    let mut credentials = HashMap::new();
    credentials.insert(
        "https://github.com/org/repo".to_string(),
        GitCredential::SshAgent {
            username: "git".to_string(),
        },
    );

    let provider = CredentialProvider::with_credentials(credentials);

    // Build callbacks for URL
    let _callbacks = provider.build_callbacks("https://github.com/org/repo");

    // Just verify it doesn't panic - actual credential callback testing
    // requires a real Git operation which is tested in integration tests
}
