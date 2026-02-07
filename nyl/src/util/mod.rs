/// Utility functions
///
/// This module will handle:
/// - Hash computation
/// - File system utilities
/// - Process execution helpers
use sha2::{Digest, Sha256};

pub mod fs;
pub use fs::{find_config_file, resolve_path, resolve_paths};

/// Compute SHA256 hash of a string
pub fn compute_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// Execute a command and return output
pub fn execute_command(_cmd: &str, _args: &[&str]) -> crate::Result<String> {
    // Stub implementation
    Ok(String::new())
}

/// Sanitize a URL by redacting userinfo (username/password/tokens)
///
/// This removes credentials from URLs to prevent them from appearing in logs.
/// Examples:
/// - `https://user:token@github.com/repo` -> `https://***@github.com/repo`
/// - `oci://user:pass@registry.io/chart` -> `oci://***@registry.io/chart`
/// - `https://github.com/repo` -> `https://github.com/repo` (unchanged)
pub fn sanitize_url(url: &str) -> String {
    // Find the position of "://" to locate the protocol
    if let Some(proto_end) = url.find("://") {
        let proto_end = proto_end + 3; // Skip past "://"
        let after_proto = &url[proto_end..];

        // Check if there's an '@' indicating userinfo
        if let Some(at_pos) = after_proto.find('@') {
            let proto = &url[..proto_end];
            let host_and_path = &after_proto[at_pos..];
            return format!("{}***{}", proto, host_and_path);
        }
    }

    // No userinfo found, return as-is
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_url_with_credentials() {
        assert_eq!(
            sanitize_url("https://user:token@github.com/repo"),
            "https://***@github.com/repo"
        );
        assert_eq!(
            sanitize_url("oci://username:password@registry.io/chart"),
            "oci://***@registry.io/chart"
        );
    }

    #[test]
    fn test_sanitize_url_without_credentials() {
        assert_eq!(sanitize_url("https://github.com/repo"), "https://github.com/repo");
        assert_eq!(sanitize_url("oci://registry.io/chart"), "oci://registry.io/chart");
    }

    #[test]
    fn test_sanitize_url_with_port() {
        assert_eq!(
            sanitize_url("https://user:pass@example.com:8080/path"),
            "https://***@example.com:8080/path"
        );
    }

    #[test]
    fn test_sanitize_url_no_protocol() {
        // URLs without protocol are returned as-is
        assert_eq!(sanitize_url("user@host:/path"), "user@host:/path");
    }
}
