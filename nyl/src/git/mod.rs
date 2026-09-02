//! Git repository management with bare repos and worktrees
//!
//! This module provides efficient Git repository cloning and caching using bare repositories
//! with worktrees for optimal disk usage and concurrent access.
//!
//! # Features
//!
//! - **Bare repositories**: Minimal disk usage, shared object store
//! - **Worktrees**: Isolated checkouts for different refs
//! - **Lazy fetching**: Refs fetched first, objects on-demand
//! - **Force checkout**: Clean state when reusing worktrees
//! - **Shared cache**: Cache directory for all Git operations
//!
//! # Cache Directory
//!
//! The cache directory is determined by:
//! 1. `NYL_CACHE_DIR` environment variable (if set)
//! 2. `.nyl/cache/` in the current directory (fallback)
//!
//! Directory structure:
//! ```text
//! $NYL_CACHE_DIR/git/
//! ├── bare/
//! │   └── {url_hash}-{repo_name}/  # Bare repository
//! └── worktrees/
//!     └── {url_hash}-{ref_hash}/    # Worktree checkout
//! ```
//!
//! # Example
//!
//! ```no_run
//! use nyl::git::GitManager;
//!
//! let mut manager = GitManager::new().unwrap();
//! let path = manager.resolve_ref(
//!     "https://github.com/example/repo.git",
//!     Some("main"),
//!     Some("subdir")
//! ).unwrap();
//! ```

mod auth;
mod cache;
mod error;
mod repository;
mod worktree;

pub use auth::{CredentialProvider, GitCredential};
pub use error::{GitError, Result};

use cache::CacheLayout;
use repository::BareRepository;
pub(crate) use worktree::WorktreeManager;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
pub(crate) fn normalize_git_url_for_equality(url: &str) -> String {
    let mut normalized = url.trim().to_lowercase();

    if let Some(at_pos) = normalized.find('@') {
        if !normalized.starts_with("http") && !normalized.starts_with("ssh://") {
            if let Some(colon_pos) = normalized[at_pos..].find(':') {
                let username_host = &normalized[..at_pos + colon_pos];
                let path = &normalized[at_pos + colon_pos + 1..];
                normalized = format!("ssh://{}/{}", username_host, path);
            }
        }
    }

    if normalized.ends_with('/') {
        normalized.truncate(normalized.len() - 1);
    }

    if std::path::Path::new(&normalized)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("git"))
    {
        normalized.truncate(normalized.len() - 4);
    }

    normalized
}
/// Main Git manager for resolving Git references to local paths
pub struct GitManager {
    cache: CacheLayout,
    bare_repos: HashMap<String, Arc<Mutex<BareRepository>>>,
    credential_provider: Option<Arc<CredentialProvider>>,
    render_cache: Option<crate::render::cache::RenderCache>,
}

impl GitManager {
    /// Create a new Git manager (public repositories only)
    pub fn new() -> Result<Self> {
        Ok(Self {
            cache: CacheLayout::new()?,
            bare_repos: HashMap::new(),
            credential_provider: None,
            render_cache: None,
        })
    }

    /// Create a Git manager with an explicit cache directory
    ///
    /// This is useful for testing where you want to avoid environment variable
    /// race conditions between parallel tests.
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Self {
        Self::with_cache_dir_and_provider(cache_dir, None)
    }

    pub fn with_credential_provider(credential_provider: Option<Arc<CredentialProvider>>) -> Result<Self> {
        Ok(Self {
            cache: CacheLayout::new()?,
            bare_repos: HashMap::new(),
            credential_provider,
            render_cache: None,
        })
    }

    pub fn with_cache_dir_and_provider(
        cache_dir: impl Into<PathBuf>,
        credential_provider: Option<Arc<CredentialProvider>>,
    ) -> Self {
        Self {
            cache: CacheLayout::with_path(cache_dir),
            bare_repos: HashMap::new(),
            credential_provider,
            render_cache: None,
        }
    }

    #[must_use]
    pub fn with_render_cache(mut self, cache: Option<crate::render::cache::RenderCache>) -> Self {
        self.render_cache = cache;
        self
    }

    /// Resolve a Git URL and ref to a local path
    ///
    /// # Arguments
    ///
    /// * `url` - Git repository URL
    /// * `git_ref` - Optional ref (branch, tag, commit). Defaults to "HEAD"
    /// * `subpath` - Optional subdirectory within the repository
    ///
    /// # Returns
    ///
    /// Path to the checked-out worktree (with subpath appended if specified)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use nyl::git::GitManager;
    /// let mut manager = GitManager::new().unwrap();
    ///
    /// // Resolve main branch
    /// let path = manager.resolve_ref(
    ///     "https://github.com/example/repo.git",
    ///     Some("main"),
    ///     None
    /// ).unwrap();
    ///
    /// // Resolve with subpath
    /// let chart_path = manager.resolve_ref(
    ///     "https://github.com/bitnami/charts.git",
    ///     Some("main"),
    ///     Some("bitnami/nginx")
    /// ).unwrap();
    /// ```
    pub fn resolve_ref(&mut self, url: &str, git_ref: Option<&str>, subpath: Option<&str>) -> Result<PathBuf> {
        self.resolve_ref_with_freshness(url, git_ref, subpath, false)
    }

    /// Resolve a ref only after a successful remote refresh.
    ///
    /// Use this for freshness-sensitive comparisons and lock updates. Immutable
    /// commit rendering can continue to use [`Self::resolve_ref`] offline.
    pub fn resolve_ref_fresh(&mut self, url: &str, git_ref: Option<&str>, subpath: Option<&str>) -> Result<PathBuf> {
        self.resolve_ref_with_freshness(url, git_ref, subpath, true)
    }

    fn resolve_ref_with_freshness(
        &mut self,
        url: &str,
        git_ref: Option<&str>,
        subpath: Option<&str>,
        require_fresh: bool,
    ) -> Result<PathBuf> {
        let git_ref = git_ref.unwrap_or("HEAD");

        // Get or create bare repository
        let bare_repo = self.get_or_create_bare_repo(url)?;

        // Always fetch latest refs to ensure we have the most recent version
        // Fall back to cached refs if fetch fails (e.g., offline scenarios)
        let fetch_error = {
            let repo = bare_repo.lock().unwrap();
            if let Err(e) = repo.fetch_refs() {
                if require_fresh {
                    return Err(e);
                }
                tracing::warn!("Failed to fetch refs for {}: {}. Falling back to cached refs.", url, e);
                Some(e)
            } else {
                self.observe_source(crate::render::cache::SourceOperation::GitRefRefresh);
                None
            }
        };

        // Resolve ref to OID
        let oid = {
            let repo = bare_repo.lock().unwrap();
            match repo.resolve_ref(git_ref) {
                Ok(oid) => {
                    if fetch_error.is_some() {
                        tracing::debug!("Using cached ref '{}' for {} after fetch failure", git_ref, url);
                    }
                    oid
                }
                Err(resolve_error) => {
                    if let Some(fetch_error) = &fetch_error {
                        tracing::warn!(
                            "Fetch fallback unavailable for {} at ref '{}': no cached ref found after fetch failure",
                            url,
                            git_ref
                        );
                        return Err(GitError::FetchFailedNoCachedRef {
                            url: url.to_string(),
                            ref_name: git_ref.to_string(),
                            fetch_error: fetch_error.to_string(),
                        });
                    }
                    return Err(resolve_error);
                }
            }
        };

        // Check if we have the commit objects, fetch if needed
        {
            let repo = bare_repo.lock().unwrap();
            if !repo.has_object(oid) {
                // Lazy fetch objects for this commit
                repo.fetch_objects(oid)?;
            }
        }

        // Get or create worktree
        let bare_repo_path = {
            let repo = bare_repo.lock().unwrap();
            repo.path().to_path_buf()
        };

        let worktree_path = self.cache.worktree_path(url, git_ref);
        let worktree_exists = worktree_path.exists();
        let worktree_path = WorktreeManager::get_or_create_worktree(&bare_repo_path, git_ref, oid, &worktree_path)?;
        self.observe_source(if worktree_exists {
            crate::render::cache::SourceOperation::GitWorktreeReuse
        } else {
            crate::render::cache::SourceOperation::GitWorktreeCreate
        });

        // Add subpath if specified
        if let Some(sub) = subpath {
            Ok(worktree_path.join(sub))
        } else {
            Ok(worktree_path)
        }
    }

    /// Get or create a bare repository for the given URL
    fn get_or_create_bare_repo(&mut self, url: &str) -> Result<Arc<Mutex<BareRepository>>> {
        // Use the URL as the key (will be normalized internally)
        let url_key = url.to_string();

        if let Some(repo) = self.bare_repos.get(&url_key) {
            self.observe_source(crate::render::cache::SourceOperation::GitRepositoryReuse);
            return Ok(Arc::clone(repo));
        }

        // Create or open the bare repository
        let bare_repo_path = self.cache.bare_repo_path(url);
        let repository_exists = bare_repo_path.exists();
        let bare_repo = BareRepository::get_or_create(url, &bare_repo_path, self.credential_provider.clone())?;
        self.observe_source(if repository_exists {
            crate::render::cache::SourceOperation::GitRepositoryReuse
        } else {
            crate::render::cache::SourceOperation::GitRepositoryClone
        });

        let bare_repo = Arc::new(Mutex::new(bare_repo));
        self.bare_repos.insert(url_key, Arc::clone(&bare_repo));

        Ok(bare_repo)
    }

    fn observe_source(&self, operation: crate::render::cache::SourceOperation) {
        if let Some(cache) = &self.render_cache {
            cache.observe_source(operation);
        }
    }
}

impl Default for GitManager {
    fn default() -> Self {
        Self::new().expect("Failed to create GitManager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use repository::BareRepository;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[test]
    fn test_git_manager_creation() {
        let temp_cache = TempDir::new().unwrap();

        let manager = GitManager::with_cache_dir(temp_cache.path());
        // Verify it was created (with_cache_dir doesn't return Result)
        assert!(manager.bare_repos.is_empty());
    }

    #[test]
    fn test_resolve_ref_uses_cached_ref_when_fetch_fails() {
        let source_dir = TempDir::new().unwrap();
        let source_repo = Repository::init(source_dir.path()).unwrap();

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = source_repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = source_repo.find_tree(tree_id).unwrap();
        source_repo
            .commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();

        let cache_dir = TempDir::new().unwrap();
        let mut manager = GitManager::with_cache_dir(cache_dir.path());
        let url = source_dir.path().to_string_lossy().to_string();

        let first_path = manager.resolve_ref(&url, Some("HEAD"), None).unwrap();
        assert!(first_path.exists());

        std::fs::remove_dir_all(source_dir.path()).unwrap();

        let second_path = manager.resolve_ref(&url, Some("HEAD"), None).unwrap();
        assert!(second_path.exists());
        assert!(manager.resolve_ref_fresh(&url, Some("HEAD"), None).is_err());
    }

    #[test]
    fn test_resolve_ref_errors_when_fetch_fails_and_no_cached_ref() {
        let cache_dir = TempDir::new().unwrap();
        let mut manager = GitManager::with_cache_dir(cache_dir.path());
        let url = cache_dir
            .path()
            .join("nyl-does-not-exist-cred-test")
            .to_string_lossy()
            .to_string();

        let bare_repo_path = manager.cache.bare_repo_path(&url);
        let raw_repo = Repository::init_bare(&bare_repo_path).unwrap();
        raw_repo.remote("origin", &url).unwrap();
        let bare_repo = BareRepository::get_or_create(&url, &bare_repo_path, None).unwrap();
        manager.bare_repos.insert(url.clone(), Arc::new(Mutex::new(bare_repo)));

        let err = manager.resolve_ref(&url, Some("HEAD"), None).unwrap_err();
        match err {
            GitError::FetchFailedNoCachedRef {
                url: err_url,
                ref_name,
                fetch_error,
            } => {
                assert_eq!(err_url, url);
                assert_eq!(ref_name, "HEAD");
                assert!(fetch_error.contains("git fetch failed"));
            }
            other => panic!("Expected FetchFailedNoCachedRef, got {other:?}"),
        }
    }

    #[test]
    fn test_normalize_git_url_for_equality_normalizes_https_and_ssh_forms() {
        assert_eq!(
            normalize_git_url_for_equality("https://github.com/example/repo.git"),
            normalize_git_url_for_equality("https://github.com/example/repo/")
        );
        assert_eq!(
            normalize_git_url_for_equality("git@github.com:example/repo.git"),
            normalize_git_url_for_equality("ssh://git@github.com/example/repo/")
        );
    }
}
