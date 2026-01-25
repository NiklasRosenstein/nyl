use sha2::{Digest, Sha256};
use std::path::PathBuf;

use super::error::Result;

/// Manages the cache directory layout for Git repositories
///
/// Directory structure:
/// ```text
/// $NYL_CACHE_DIR/git/
/// ├── bare/
/// │   └── {url_hash}-{repo_name}/  # Bare repository
/// └── worktrees/
///     └── {url_hash}-{ref_hash}/    # Worktree checkout
/// ```
pub struct CacheLayout {
    root: PathBuf,
}

impl CacheLayout {
    /// Create a new cache layout
    ///
    /// Uses NYL_CACHE_DIR environment variable if set, otherwise falls back to
    /// `.nyl/cache/` in the current directory.
    pub fn new() -> Result<Self> {
        let root = if let Ok(cache_dir) = std::env::var("NYL_CACHE_DIR") {
            PathBuf::from(cache_dir)
        } else {
            std::env::current_dir()?.join(".nyl").join("cache")
        };

        Ok(Self { root })
    }

    /// Get the path to the bare repository for a given URL
    pub fn bare_repo_path(&self, url: &str) -> PathBuf {
        let normalized = Self::normalize_url(url);
        let hash = Self::url_hash(&normalized);
        let repo_name = Self::extract_repo_name(&normalized);

        self.root
            .join("git")
            .join("bare")
            .join(format!("{}-{}", &hash[..16], repo_name))
    }

    /// Get the path to the worktree for a given URL and ref
    pub fn worktree_path(&self, url: &str, git_ref: &str) -> PathBuf {
        let normalized = Self::normalize_url(url);
        let url_hash = Self::url_hash(&normalized);
        let ref_hash = Self::ref_hash(git_ref);

        self.root
            .join("git")
            .join("worktrees")
            .join(format!("{}-{}", &url_hash[..16], &ref_hash[..16]))
    }

    /// Normalize a Git URL for consistent hashing
    ///
    /// - Convert to lowercase
    /// - Remove trailing slashes
    /// - Remove .git suffix
    fn normalize_url(url: &str) -> String {
        let mut normalized = url.trim().to_lowercase();

        // Remove trailing slash first
        if normalized.ends_with('/') {
            normalized.truncate(normalized.len() - 1);
        }

        // Then remove .git suffix (case-insensitive check)
        if std::path::Path::new(&normalized)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
        {
            normalized.truncate(normalized.len() - 4);
        }

        normalized
    }

    /// Extract repository name from URL for human-readable directory names
    fn extract_repo_name(url: &str) -> String {
        url.split('/')
            .next_back()
            .unwrap_or("repo")
            .to_string()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect()
    }

    /// Compute SHA256 hash of URL
    fn url_hash(url: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Compute SHA256 hash of ref
    fn ref_hash(git_ref: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(git_ref.as_bytes());
        hex::encode(hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_url() {
        assert_eq!(
            CacheLayout::normalize_url("https://github.com/user/repo.git"),
            "https://github.com/user/repo"
        );
        assert_eq!(
            CacheLayout::normalize_url("https://GitHub.com/User/Repo.git/"),
            "https://github.com/user/repo"
        );
    }

    #[test]
    fn test_extract_repo_name() {
        assert_eq!(
            CacheLayout::extract_repo_name("https://github.com/user/my-repo"),
            "my-repo"
        );
        assert_eq!(
            CacheLayout::extract_repo_name("https://github.com/user/repo_name"),
            "repo_name"
        );
    }

    #[test]
    fn test_url_hash_deterministic() {
        let hash1 = CacheLayout::url_hash("https://github.com/user/repo");
        let hash2 = CacheLayout::url_hash("https://github.com/user/repo");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_ref_hash_deterministic() {
        let hash1 = CacheLayout::ref_hash("main");
        let hash2 = CacheLayout::ref_hash("main");
        assert_eq!(hash1, hash2);
    }
}
