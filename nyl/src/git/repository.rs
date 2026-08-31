use git2::{ErrorCode, FetchOptions, FetchPrune, Oid, Repository};
use std::path::Path;
use std::sync::Arc;

use super::auth::CredentialProvider;
use super::error::{GitError, Result};

/// Manages a bare Git repository
pub struct BareRepository {
    repo: Repository,
    url: String,
    credential_provider: Option<Arc<CredentialProvider>>,
}

impl BareRepository {
    fn resolve_object_to_commit_oid(&self, oid: Oid) -> Result<Oid> {
        let object = match self.repo.find_object(oid, None) {
            Ok(object) => object,
            Err(error) if error.code() == ErrorCode::NotFound => {
                self.fetch_objects(oid)?;
                self.repo.find_object(oid, None)?
            }
            Err(error) => return Err(GitError::Repository(error)),
        };

        let commit = object.peel_to_commit()?;
        let commit_oid = commit.id();
        if commit_oid != oid {
            tracing::debug!("Resolved object {} to commit {}", oid, commit_oid);
        }
        Ok(commit_oid)
    }

    fn resolve_reference_to_commit_oid(&self, reference_name: &str) -> Result<Option<Oid>> {
        let Ok(reference) = self.repo.find_reference(reference_name) else {
            return Ok(None);
        };

        if let Some(oid) = reference.target() {
            return self.resolve_object_to_commit_oid(oid).map(Some);
        }

        Ok(None)
    }

    /// Get or create a bare repository at the specified path
    pub fn get_or_create(url: &str, path: &Path, credential_provider: Option<Arc<CredentialProvider>>) -> Result<Self> {
        let repo = if path.exists() {
            tracing::debug!("Reusing cached bare repository for {} at {}", url, path.display());
            Repository::open(path)?
        } else {
            tracing::debug!("Creating bare repository cache for {} at {}", url, path.display());
            Self::clone_bare(url, path, credential_provider.as_deref())?
        };

        Ok(Self {
            repo,
            url: url.to_string(),
            credential_provider,
        })
    }

    /// Clone a bare repository with lazy fetching (refs only initially)
    fn clone_bare(url: &str, path: &Path, credential_provider: Option<&CredentialProvider>) -> Result<Repository> {
        // Create parent directory
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        tracing::trace!("Starting bare clone for {} into {}", url, path.display());
        tracing::debug!("Initializing bare Git repository for {} at {}", url, path.display());
        // Initialize bare repository
        let repo = Repository::init_bare(path).map_err(|e| GitError::CloneFailed {
            url: url.to_string(),
            source: e,
        })?;

        // Add remote
        repo.remote("origin", url).map_err(|e| GitError::CloneFailed {
            url: url.to_string(),
            source: e,
        })?;

        tracing::debug!("Fetching initial refs for {}", url);
        // Fetch refs only (no objects yet - lazy loading)
        Self::fetch_refs_with_auth(&repo, url, credential_provider)?;
        tracing::debug!("Initial ref fetch complete for {}", url);
        tracing::trace!("Bare clone completed successfully for {}", url);

        Ok(repo)
    }

    /// Fetch refs from the remote using git2 API with authentication
    fn fetch_refs_with_auth(
        repo: &Repository,
        url: &str,
        credential_provider: Option<&CredentialProvider>,
    ) -> Result<()> {
        tracing::trace!(
            "Fetching refs for {} (credential_provider={})",
            url,
            if credential_provider.is_some() {
                "present"
            } else {
                "absent"
            }
        );
        let callbacks = if let Some(provider) = credential_provider {
            provider.build_callbacks(url)
        } else {
            Self::build_default_callbacks()
        };

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);
        fetch_options.prune(FetchPrune::On);

        let mut remote = repo.find_remote("origin").map_err(GitError::Repository)?;
        remote
            .fetch(
                &[
                    "+refs/heads/*:refs/heads/*",
                    "+refs/tags/*:refs/tags/*",
                    "+HEAD:refs/remotes/origin/HEAD",
                ],
                Some(&mut fetch_options),
                None,
            )
            .map_err(|e| GitError::Command(format!("git fetch failed: {}", e)))?;

        tracing::trace!("Fetch refs completed for {}", url);
        Ok(())
    }

    /// Build default callbacks with SSH agent fallback
    fn build_default_callbacks<'a>() -> git2::RemoteCallbacks<'a> {
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, allowed_types| {
            if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                let username = username_from_url.unwrap_or("git");
                git2::Cred::ssh_key_from_agent(username)
            } else {
                Err(git2::Error::from_str("No credentials available"))
            }
        });
        callbacks
    }

    /// Update refs from the remote
    pub fn fetch_refs(&self) -> Result<()> {
        tracing::debug!("Refreshing remote refs for {}", self.url);
        Self::fetch_refs_with_auth(&self.repo, &self.url, self.credential_provider.as_deref())
    }

    /// Resolve a ref (branch, tag, or commit) to an OID
    pub fn resolve_ref(&self, ref_name: &str) -> Result<Oid> {
        // Try direct ref lookup first (branches, tags)
        if let Some(oid) = self.resolve_reference_to_commit_oid(ref_name)? {
            return Ok(oid);
        }

        // Try with refs/heads/ prefix (branches)
        let branch_ref = format!("refs/heads/{}", ref_name);
        if let Some(oid) = self.resolve_reference_to_commit_oid(&branch_ref)? {
            return Ok(oid);
        }

        // Try with refs/tags/ prefix (tags)
        let tag_ref = format!("refs/tags/{}", ref_name);
        if let Some(oid) = self.resolve_reference_to_commit_oid(&tag_ref)? {
            return Ok(oid);
        }

        // Try parsing as OID (commit hash)
        if let Ok(oid) = Oid::from_str(ref_name) {
            if let Ok(commit_oid) = self.resolve_object_to_commit_oid(oid) {
                return Ok(commit_oid);
            }
        }

        // Try HEAD if ref_name is "HEAD"
        if ref_name == "HEAD" {
            if let Some(oid) = self.resolve_reference_to_commit_oid("HEAD")? {
                return Ok(oid);
            }

            // Bare repos created via init+fetch have no local HEAD.
            // Use the remote HEAD fetched into refs/remotes/origin/HEAD.
            if let Some(oid) = self.resolve_reference_to_commit_oid("refs/remotes/origin/HEAD")? {
                return Ok(oid);
            }
        }

        Err(GitError::RefNotFound {
            ref_name: ref_name.to_string(),
        })
    }

    /// Check if an object exists in the repository
    pub fn has_object(&self, oid: Oid) -> bool {
        self.repo.find_object(oid, None).is_ok()
    }

    /// Fetch specific objects for a commit
    pub fn fetch_objects(&self, oid: Oid) -> Result<()> {
        let oid_str = oid.to_string();
        tracing::debug!("Fetching commit objects for {} at {}", self.url, oid_str);

        let callbacks = if let Some(provider) = &self.credential_provider {
            provider.build_callbacks(&self.url)
        } else {
            Self::build_default_callbacks()
        };

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        let mut remote = self.repo.find_remote("origin").map_err(GitError::Repository)?;
        remote
            .fetch(&[&oid_str], Some(&mut fetch_options), None)
            .map_err(|e| GitError::Command(format!("git fetch {} failed: {}", oid_str, e)))?;

        tracing::debug!("Fetched commit objects for {} at {}", self.url, oid_str);
        Ok(())
    }

    /// Get the repository path
    pub fn path(&self) -> &Path {
        self.repo.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_bare_repository_creation() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path().join("test.git");

        // Initialize a test repository to clone from
        let source_dir = TempDir::new().unwrap();
        let source_repo = Repository::init(source_dir.path()).unwrap();

        // Create a commit
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = source_repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = source_repo.find_tree(tree_id).unwrap();
        source_repo
            .commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();

        // This test would require a real Git repository to clone from
        // For now, just test that the structure is correct
        assert!(!repo_path.exists());
    }

    #[test]
    fn refreshing_refs_prunes_deleted_remote_branches() {
        let source_dir = TempDir::new().unwrap();
        let source_repo = Repository::init(source_dir.path()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = source_repo.index().unwrap().write_tree().unwrap();
        let tree = source_repo.find_tree(tree_id).unwrap();
        let commit_id = source_repo
            .commit(Some("HEAD"), &signature, &signature, "Initial commit", &tree, &[])
            .unwrap();
        let commit = source_repo.find_commit(commit_id).unwrap();
        source_repo.branch("temporary", &commit, false).unwrap();
        drop(commit);
        drop(tree);

        let cache_dir = TempDir::new().unwrap();
        let url = source_dir.path().to_string_lossy();
        let bare = BareRepository::get_or_create(&url, &cache_dir.path().join("cache.git"), None).unwrap();
        assert_eq!(bare.resolve_ref("temporary").unwrap(), commit_id);

        source_repo
            .find_branch("temporary", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
        bare.fetch_refs().unwrap();
        assert!(bare.resolve_ref("temporary").is_err());
    }
}
