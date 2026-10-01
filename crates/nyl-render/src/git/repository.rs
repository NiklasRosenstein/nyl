use git2::{ErrorCode, Oid, Repository};
use std::path::Path;

use super::error::{GitError, Result};
use super::transport;

/// Manages a bare Git repository
pub struct BareRepository {
    repo: Repository,
    url: String,
}

impl BareRepository {
    fn resolve_object_to_commit_oid(&self, oid: Oid, fetch: bool) -> Result<Oid> {
        let object = match self.repo.find_object(oid, None) {
            Ok(object) => object,
            Err(error) if fetch && error.code() == ErrorCode::NotFound => {
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

    fn resolve_reference_to_commit_oid(&self, reference_name: &str, fetch: bool) -> Result<Option<Oid>> {
        let Ok(reference) = self.repo.find_reference(reference_name) else {
            return Ok(None);
        };

        if let Some(oid) = reference.target() {
            return self.resolve_object_to_commit_oid(oid, fetch).map(Some);
        }

        Ok(None)
    }

    /// Get or create a bare repository at the specified path
    pub fn get_or_create(url: &str, path: &Path) -> Result<Self> {
        let repo = if path.exists() {
            tracing::debug!("Reusing cached bare repository for {} at {}", url, path.display());
            Repository::open(path)?
        } else {
            tracing::debug!("Creating bare repository cache for {} at {}", url, path.display());
            Self::clone_bare(url, path)?
        };

        // Cached checkouts preserve repository bytes so ownership hashes and
        // rendered diffs are identical on every host platform.
        let mut config = repo.config()?;
        config.set_bool("core.autocrlf", false)?;
        config.set_str("core.eol", "lf")?;
        drop(config);

        Ok(Self {
            repo,
            url: url.to_string(),
        })
    }

    /// Clone a bare repository with lazy fetching (refs only initially)
    fn clone_bare(url: &str, path: &Path) -> Result<Repository> {
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
        Self::fetch_refs_from(&repo, url)?;
        tracing::debug!("Initial ref fetch complete for {}", url);
        tracing::trace!("Bare clone completed successfully for {}", url);

        Ok(repo)
    }

    /// Fetch the branches, tags, and remote `HEAD` of `url`, pruning deleted ones.
    fn fetch_refs_from(repo: &Repository, url: &str) -> Result<()> {
        tracing::trace!("Fetching refs for {}", url);
        // `HEAD*` matches the remote HEAD as a glob, so a repository without
        // one (an empty repository) fetches nothing instead of failing.
        transport::fetch(
            repo,
            url,
            &[
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
                "+HEAD*:refs/remotes/origin/HEAD*",
            ],
            true,
        )?;
        tracing::trace!("Fetch refs completed for {}", url);
        Ok(())
    }

    /// Update refs from the remote
    pub fn fetch_refs(&self) -> Result<()> {
        tracing::debug!("Refreshing remote refs for {}", self.url);
        Self::fetch_refs_from(&self.repo, &self.url)
    }

    /// Resolve a ref (branch, tag, or commit) to an OID, fetching a commit
    /// that is missing from the cache by ID.
    pub fn resolve_ref(&self, ref_name: &str) -> Result<Oid> {
        self.resolve_ref_with_fetch(ref_name, true)
    }

    /// Resolve a ref (branch, tag, or commit) to an OID from the cache alone.
    /// A commit missing from the cache does not resolve.
    pub fn resolve_cached_ref(&self, ref_name: &str) -> Result<Oid> {
        self.resolve_ref_with_fetch(ref_name, false)
    }

    fn resolve_ref_with_fetch(&self, ref_name: &str, fetch: bool) -> Result<Oid> {
        // Try direct ref lookup first (branches, tags)
        if let Some(oid) = self.resolve_reference_to_commit_oid(ref_name, fetch)? {
            return Ok(oid);
        }

        // Try with refs/heads/ prefix (branches)
        let branch_ref = format!("refs/heads/{}", ref_name);
        if let Some(oid) = self.resolve_reference_to_commit_oid(&branch_ref, fetch)? {
            return Ok(oid);
        }

        // Try with refs/tags/ prefix (tags)
        let tag_ref = format!("refs/tags/{}", ref_name);
        if let Some(oid) = self.resolve_reference_to_commit_oid(&tag_ref, fetch)? {
            return Ok(oid);
        }

        // Try parsing as OID (commit hash)
        if let Ok(oid) = Oid::from_str(ref_name) {
            if let Ok(commit_oid) = self.resolve_object_to_commit_oid(oid, fetch) {
                return Ok(commit_oid);
            }
        }

        // Try HEAD if ref_name is "HEAD"
        if ref_name == "HEAD" {
            if let Some(oid) = self.resolve_reference_to_commit_oid("HEAD", fetch)? {
                return Ok(oid);
            }

            // Bare repos created via init+fetch have no local HEAD.
            // Use the remote HEAD fetched into refs/remotes/origin/HEAD.
            if let Some(oid) = self.resolve_reference_to_commit_oid("refs/remotes/origin/HEAD", fetch)? {
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

        transport::fetch(&self.repo, &self.url, &[&oid_str], false)?;

        tracing::debug!("Fetched commit objects for {} at {}", self.url, oid_str);
        Ok(())
    }

    /// Fetch `commit` by ID; when the server refuses unadvertised objects,
    /// fetch the refs instead, which brings any commit reachable from a
    /// branch or tag.
    fn fetch_commit(&self, commit: Oid) -> Result<()> {
        let by_id = match self.fetch_objects(commit) {
            Ok(()) if self.has_object(commit) => return Ok(()),
            Ok(()) => None,
            Err(error) => Some(error),
        };
        tracing::debug!("Fetching refs of {} to find {commit}", self.url);
        match (self.fetch_refs(), by_id) {
            (Ok(()), _) if self.has_object(commit) => Ok(()),
            (Ok(()), by_id) => Err(GitError::Other(format!(
                "commit {commit} is not reachable from any branch or tag of {}{}",
                crate::util::sanitize_url(&self.url),
                by_id
                    .map(|error| format!(", and fetching it by ID failed: {error}"))
                    .unwrap_or_default()
            ))),
            (Err(refs), Some(by_id)) => Err(GitError::Other(format!(
                "fetching {commit} by ID failed ({by_id}), and fetching refs failed ({refs})"
            ))),
            (Err(refs), None) => Err(refs),
        }
    }

    /// Read the file at `path` in `commit`, fetching the commit by ID when it
    /// is not cached, or through the refs when the server refuses that. Returns `None` when the commit has no such path.
    ///
    /// The path is repository-relative. A symbolic link at the path is an
    /// error, and a path through a symbolic link does not resolve.
    pub fn read_blob(&self, commit: Oid, path: &str) -> Result<Option<Vec<u8>>> {
        if !self.has_object(commit) {
            self.fetch_commit(commit)?;
        }
        let tree = self.repo.find_commit(commit)?.tree()?;
        // Check every component, so a symbolic link to a directory is refused
        // rather than read as a missing file.
        let mut prefix = std::path::PathBuf::new();
        let mut entry = None;
        for component in Path::new(path).components() {
            prefix.push(component);
            let found = match tree.get_path(&prefix) {
                Ok(found) => found,
                Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
                Err(error) => return Err(GitError::Repository(error)),
            };
            if found.filemode() == 0o120_000 {
                return Err(GitError::Other(format!(
                    "{} at {commit} is a symbolic link; Nyl reads only regular files",
                    prefix.display()
                )));
            }
            entry = Some(found);
        }
        let Some(entry) = entry else {
            return Ok(None);
        };
        let object = entry.to_object(&self.repo)?;
        let blob = object
            .as_blob()
            .ok_or_else(|| GitError::Other(format!("{path} at {commit} is not a file")))?;
        Ok(Some(blob.content().to_vec()))
    }

    /// The commit a branch names, or `None` when the branch does not exist.
    pub fn branch_commit(&self, branch: &str) -> Result<Option<Oid>> {
        let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        self.resolve_reference_to_commit_oid(&format!("refs/heads/{branch}"), false)
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

        let url = source_dir.path().to_string_lossy();
        BareRepository::get_or_create(&url, &repo_path).unwrap();

        let config = Repository::open_bare(&repo_path)
            .unwrap()
            .config()
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(!config.get_bool("core.autocrlf").unwrap());
        assert_eq!(config.get_str("core.eol").unwrap(), "lf");
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
        let bare = BareRepository::get_or_create(&url, &cache_dir.path().join("cache.git")).unwrap();
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
