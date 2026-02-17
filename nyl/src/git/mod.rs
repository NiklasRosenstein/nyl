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
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

fn is_argocd_env() -> bool {
    std::env::var_os("ARGOCD_APP_NAME").is_some() || std::env::var_os("ARGOCD_APP_NAMESPACE").is_some()
}

pub async fn argocd_credential_provider_from_cluster() -> Option<Arc<CredentialProvider>> {
    if !is_argocd_env() {
        tracing::trace!("Not running in ArgoCD environment; skipping credential discovery");
        return None;
    }
    tracing::debug!("ArgoCD environment detected, attempting credential discovery from cluster");

    let Ok(config) = kube::Config::incluster_env() else {
        tracing::debug!("In-cluster Kubernetes config not available; skipping ArgoCD credential discovery");
        return None;
    };

    let client = match kube::Client::try_from(config) {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!("Failed to create in-cluster Kubernetes client: {}", e);
            return None;
        }
    };

    let discovery = match ArgoCDCredentialDiscovery::new(client) {
        Ok(discovery) => discovery,
        Err(e) => {
            tracing::warn!("Failed to initialize ArgoCD credential discovery: {}", e);
            return None;
        }
    };

    match discovery.discover_credentials().await {
        Ok(credentials) => {
            if credentials.is_empty() {
                tracing::debug!("No ArgoCD Git credentials discovered");
                return None;
            }
            tracing::debug!("Loaded {} ArgoCD Git credentials", credentials.len());
            Some(Arc::new(CredentialProvider::with_credentials(credentials)))
        }
        Err(e) => {
            tracing::warn!("Failed to discover ArgoCD credentials: {}", e);
            None
        }
    }
}

fn try_resolve_ref_from_argocd_env(url: &str, git_ref: &str, subpath: Option<&str>) -> Option<PathBuf> {
    let env_repo_url = std::env::var("ARGOCD_APP_SOURCE_REPO_URL").ok()?;
    let env_target_revision = std::env::var("ARGOCD_APP_SOURCE_TARGET_REVISION").ok()?;
    let env_source_path = std::env::var("ARGOCD_APP_SOURCE_PATH").ok()?;

    let requested_ref = git_ref.trim();
    let target_revision = env_target_revision.trim();
    if requested_ref != target_revision {
        tracing::trace!(
            "Skipping ArgoCD local checkout reuse: targetRevision mismatch (requested={}, argocd={})",
            requested_ref,
            target_revision
        );
        return None;
    }

    let requested_url = normalize_git_url_for_equality(url);
    let source_url = normalize_git_url_for_equality(&env_repo_url);
    if requested_url != source_url {
        tracing::trace!(
            "Skipping ArgoCD local checkout reuse: repoURL mismatch (requested={}, argocd={})",
            crate::util::sanitize_url(url),
            crate::util::sanitize_url(&env_repo_url)
        );
        return None;
    }

    let repo_root = derive_repo_root_from_source_path(&env_source_path)?;
    if !repo_root.is_dir() {
        tracing::debug!(
            "Skipping ArgoCD local checkout reuse: derived repo root is not a directory: {}",
            repo_root.display()
        );
        return None;
    }

    let path = if let Some(sub) = subpath {
        repo_root.join(sub)
    } else {
        repo_root
    };

    tracing::debug!(
        "Reusing ArgoCD local checkout for {} at {} (targetRevision={})",
        crate::util::sanitize_url(url),
        path.display(),
        target_revision
    );
    Some(path)
}

fn derive_repo_root_from_source_path(raw_source_path: &str) -> Option<PathBuf> {
    let source_path_raw = raw_source_path.trim();
    if source_path_raw.is_empty() {
        return None;
    }

    let source_path = Path::new(source_path_raw);
    if source_path.is_absolute() {
        tracing::trace!(
            "Skipping ArgoCD local checkout reuse: ARGOCD_APP_SOURCE_PATH must be relative, got {}",
            source_path.display()
        );
        return None;
    }

    let cwd = std::env::current_dir().ok()?;
    let source_dir_from_cwd = cwd.join(source_path);
    if source_dir_from_cwd.is_dir() {
        return Some(cwd);
    }

    let source_components = relative_normal_components(source_path)?;
    let cwd_components = normal_components(&cwd);
    if ends_with_components(&cwd_components, &source_components) {
        let mut repo_root = cwd.clone();
        for _ in 0..source_components.len() {
            repo_root.pop();
        }
        if repo_root.join(source_path).is_dir() {
            return Some(repo_root);
        }
    }

    tracing::trace!(
        "Skipping ArgoCD local checkout reuse: could not derive repo root from cwd={} and sourcePath={}",
        cwd.display(),
        source_path.display()
    );
    None
}

fn normalize_git_url_for_equality(url: &str) -> String {
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

fn relative_normal_components(path: &Path) -> Option<Vec<String>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => components.push(part.to_string_lossy().to_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(components)
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| {
            if let Component::Normal(part) = component {
                Some(part.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect()
}

fn ends_with_components(path: &[String], suffix: &[String]) -> bool {
    path.len() >= suffix.len() && path[path.len() - suffix.len()..] == *suffix
}

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
        Self::with_cache_dir_and_provider(cache_dir, None)
    }

    pub fn with_credential_provider(credential_provider: Option<Arc<CredentialProvider>>) -> Result<Self> {
        Ok(Self {
            cache: CacheLayout::new()?,
            bare_repos: HashMap::new(),
            credential_provider,
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
        if let Some(path) = try_resolve_ref_from_argocd_env(url, git_ref, subpath) {
            return Ok(path);
        }

        // Get or create bare repository
        let bare_repo = self.get_or_create_bare_repo(url)?;

        // Always fetch latest refs to ensure we have the most recent version
        // Fall back to cached refs if fetch fails (e.g., offline scenarios)
        let fetch_error = {
            let repo = bare_repo.lock().unwrap();
            if let Err(e) = repo.fetch_refs() {
                tracing::warn!("Failed to fetch refs for {}: {}. Falling back to cached refs.", url, e);
                Some(e)
            } else {
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
    use git2::Repository;
    use repository::BareRepository;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard};
    use tempfile::TempDir;

    static ARGOCD_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_argocd_env() -> MutexGuard<'static, ()> {
        ARGOCD_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn canonicalize_for_assert(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    struct EnvCwdGuard {
        repo_url: Option<String>,
        target_revision: Option<String>,
        source_path: Option<String>,
        original_cwd: PathBuf,
    }

    impl EnvCwdGuard {
        fn new() -> Self {
            Self {
                repo_url: std::env::var("ARGOCD_APP_SOURCE_REPO_URL").ok(),
                target_revision: std::env::var("ARGOCD_APP_SOURCE_TARGET_REVISION").ok(),
                source_path: std::env::var("ARGOCD_APP_SOURCE_PATH").ok(),
                original_cwd: std::env::current_dir().expect("current dir should be available"),
            }
        }
    }

    impl Drop for EnvCwdGuard {
        fn drop(&mut self) {
            match &self.repo_url {
                Some(v) => std::env::set_var("ARGOCD_APP_SOURCE_REPO_URL", v),
                None => std::env::remove_var("ARGOCD_APP_SOURCE_REPO_URL"),
            }
            match &self.target_revision {
                Some(v) => std::env::set_var("ARGOCD_APP_SOURCE_TARGET_REVISION", v),
                None => std::env::remove_var("ARGOCD_APP_SOURCE_TARGET_REVISION"),
            }
            match &self.source_path {
                Some(v) => std::env::set_var("ARGOCD_APP_SOURCE_PATH", v),
                None => std::env::remove_var("ARGOCD_APP_SOURCE_PATH"),
            }
            let _ = std::env::set_current_dir(&self.original_cwd);
        }
    }

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
    }

    #[test]
    fn test_resolve_ref_errors_when_fetch_fails_and_no_cached_ref() {
        let cache_dir = TempDir::new().unwrap();
        let mut manager = GitManager::with_cache_dir(cache_dir.path());
        let url = "/tmp/nyl-does-not-exist-cred-test".to_string();

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
    fn test_try_resolve_ref_from_argocd_env_reuses_checkout_on_exact_match() {
        let _guard = lock_argocd_env();
        let _env_guard = EnvCwdGuard::new();

        let repo_root = TempDir::new().unwrap();
        std::fs::create_dir_all(repo_root.path().join("clusters/default")).unwrap();
        std::env::set_current_dir(repo_root.path()).unwrap();
        std::env::set_var("ARGOCD_APP_SOURCE_REPO_URL", "https://github.com/example/repo.git");
        std::env::set_var("ARGOCD_APP_SOURCE_TARGET_REVISION", "HEAD");
        std::env::set_var("ARGOCD_APP_SOURCE_PATH", "clusters/default");

        let resolved = try_resolve_ref_from_argocd_env("https://github.com/example/repo", "HEAD", None).unwrap();
        assert_eq!(canonicalize_for_assert(&resolved), canonicalize_for_assert(repo_root.path()));
    }

    #[test]
    fn test_try_resolve_ref_from_argocd_env_reuses_checkout_with_subpath() {
        let _guard = lock_argocd_env();
        let _env_guard = EnvCwdGuard::new();

        let repo_root = TempDir::new().unwrap();
        std::fs::create_dir_all(repo_root.path().join("apps")).unwrap();
        std::fs::create_dir_all(repo_root.path().join("charts/service")).unwrap();
        std::env::set_current_dir(repo_root.path()).unwrap();
        std::env::set_var("ARGOCD_APP_SOURCE_REPO_URL", "git@github.com:example/repo.git");
        std::env::set_var("ARGOCD_APP_SOURCE_TARGET_REVISION", "main");
        std::env::set_var("ARGOCD_APP_SOURCE_PATH", "apps");

        let resolved =
            try_resolve_ref_from_argocd_env("ssh://git@github.com/example/repo", "main", Some("charts/service"))
                .unwrap();
        assert_eq!(
            canonicalize_for_assert(&resolved),
            canonicalize_for_assert(&repo_root.path().join("charts/service"))
        );
    }

    #[test]
    fn test_try_resolve_ref_from_argocd_env_skips_on_target_revision_mismatch() {
        let _guard = lock_argocd_env();
        let _env_guard = EnvCwdGuard::new();

        std::env::set_var("ARGOCD_APP_SOURCE_REPO_URL", "https://github.com/example/repo.git");
        std::env::set_var("ARGOCD_APP_SOURCE_TARGET_REVISION", "main");
        std::env::set_var("ARGOCD_APP_SOURCE_PATH", "apps");

        let resolved = try_resolve_ref_from_argocd_env("https://github.com/example/repo.git", "HEAD", None);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_try_resolve_ref_from_argocd_env_skips_on_repo_mismatch() {
        let _guard = lock_argocd_env();
        let _env_guard = EnvCwdGuard::new();

        std::env::set_var("ARGOCD_APP_SOURCE_REPO_URL", "https://github.com/example/other.git");
        std::env::set_var("ARGOCD_APP_SOURCE_TARGET_REVISION", "HEAD");
        std::env::set_var("ARGOCD_APP_SOURCE_PATH", "apps");

        let resolved = try_resolve_ref_from_argocd_env("https://github.com/example/repo.git", "HEAD", None);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_try_resolve_ref_from_argocd_env_skips_on_unresolvable_source_path() {
        let _guard = lock_argocd_env();
        let _env_guard = EnvCwdGuard::new();

        let repo_root = TempDir::new().unwrap();
        std::env::set_current_dir(repo_root.path()).unwrap();
        std::env::set_var("ARGOCD_APP_SOURCE_REPO_URL", "https://github.com/example/repo.git");
        std::env::set_var("ARGOCD_APP_SOURCE_TARGET_REVISION", "HEAD");
        std::env::set_var("ARGOCD_APP_SOURCE_PATH", "missing/path");

        let resolved = try_resolve_ref_from_argocd_env("https://github.com/example/repo.git", "HEAD", None);
        assert!(resolved.is_none());
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
