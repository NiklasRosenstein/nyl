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

pub mod argocd;
mod auth;
mod cache;
mod error;
mod repository;
mod worktree; // Public for testing URL matching

pub use argocd::ArgoCDCredentialDiscovery;
pub use auth::{CredentialProvider, GitCredential};
pub use error::{GitError, Result};

use cache::CacheLayout;
use repository::BareRepository;
use worktree::WorktreeManager;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Main Git manager for resolving Git references to local paths
pub struct GitManager {
    cache: CacheLayout,
    bare_repos: HashMap<String, Arc<Mutex<BareRepository>>>,
    credential_provider: Option<Arc<CredentialProvider>>,
}

impl GitManager {
    /// Create a new Git manager (public repositories only)
    pub fn new() -> Result<Self> {
        Ok(Self {
            cache: CacheLayout::new()?,
            bare_repos: HashMap::new(),
            credential_provider: None,
        })
    }

    /// Create a Git manager with an explicit cache directory
    ///
    /// This is useful for testing where you want to avoid environment variable
    /// race conditions between parallel tests.
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache: CacheLayout::with_path(cache_dir),
            bare_repos: HashMap::new(),
            credential_provider: None,
        }
    }

    /// Create a Git manager with Kubernetes client for ArgoCD credential discovery
    pub async fn with_kubernetes(client: kube::Client) -> Result<Self> {
        let discovery = ArgoCDCredentialDiscovery::new(client)?;
        let credentials = discovery.discover_credentials().await?;

        let provider = CredentialProvider::with_credentials(credentials);

        Ok(Self {
            cache: CacheLayout::new()?,
            bare_repos: HashMap::new(),
            credential_provider: Some(Arc::new(provider)),
        })
    }

    /// Create a Git manager with a pre-built credential provider
    pub fn with_credential_provider(provider: Arc<CredentialProvider>) -> Result<Self> {
        Ok(Self {
            cache: CacheLayout::new()?,
            bare_repos: HashMap::new(),
            credential_provider: Some(provider),
        })
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
        let git_ref = git_ref.unwrap_or("HEAD");

        // Get or create bare repository
        let bare_repo = self.get_or_create_bare_repo(url)?;

        // Resolve ref to OID
        let oid = {
            let repo = bare_repo.lock().unwrap();

            // Try to resolve the ref
            match repo.resolve_ref(git_ref) {
                Ok(oid) => oid,
                Err(GitError::RefNotFound { .. }) => {
                    // Ref might be new, fetch all refs and try again
                    repo.fetch_refs()?;
                    repo.resolve_ref(git_ref)?
                }
                Err(e) => return Err(e),
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
        let worktree_path = WorktreeManager::get_or_create_worktree(&bare_repo_path, git_ref, oid, &worktree_path)?;

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
            return Ok(Arc::clone(repo));
        }

        // Create or open the bare repository
        let bare_repo_path = self.cache.bare_repo_path(url);
        let bare_repo = BareRepository::get_or_create(url, &bare_repo_path, self.credential_provider.clone())?;

        let bare_repo = Arc::new(Mutex::new(bare_repo));
        self.bare_repos.insert(url_key, Arc::clone(&bare_repo));

        Ok(bare_repo)
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
    use tempfile::TempDir;

    #[test]
    fn test_git_manager_creation() {
        let temp_cache = TempDir::new().unwrap();

        let manager = GitManager::with_cache_dir(temp_cache.path());
        // Verify it was created (with_cache_dir doesn't return Result)
        assert!(manager.bare_repos.is_empty());
    }
}
