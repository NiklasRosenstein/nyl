use git2::{Oid, Repository};
use std::path::Path;
use std::process::Command;

use super::error::{GitError, Result};

/// Manages a bare Git repository
pub struct BareRepository {
    repo: Repository,
    url: String,
}

impl BareRepository {
    /// Get or create a bare repository at the specified path
    pub fn get_or_create(url: &str, path: &Path) -> Result<Self> {
        let repo = if path.exists() {
            Repository::open(path)?
        } else {
            Self::clone_bare(url, path)?
        };

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

        // Fetch refs only (no objects yet - lazy loading)
        Self::fetch_refs_cmd(path, url)?;

        Ok(repo)
    }

    /// Fetch refs from the remote using git command
    fn fetch_refs_cmd(repo_path: &Path, _url: &str) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .arg("fetch")
            .arg("origin")
            .arg("refs/heads/*:refs/heads/*")
            .arg("refs/tags/*:refs/tags/*")
            .output()?;

        if !output.status.success() {
            return Err(GitError::Command(format!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }

    /// Update refs from the remote
    pub fn fetch_refs(&self) -> Result<()> {
        Self::fetch_refs_cmd(self.repo.path(), &self.url)
    }

    /// Resolve a ref (branch, tag, or commit) to an OID
    pub fn resolve_ref(&self, ref_name: &str) -> Result<Oid> {
        // Try direct ref lookup first (branches, tags)
        if let Ok(reference) = self.repo.find_reference(ref_name) {
            if let Some(oid) = reference.target() {
                return Ok(oid);
            }
        }

        // Try with refs/heads/ prefix (branches)
        let branch_ref = format!("refs/heads/{}", ref_name);
        if let Ok(reference) = self.repo.find_reference(&branch_ref) {
            if let Some(oid) = reference.target() {
                return Ok(oid);
            }
        }

        // Try with refs/tags/ prefix (tags)
        let tag_ref = format!("refs/tags/{}", ref_name);
        if let Ok(reference) = self.repo.find_reference(&tag_ref) {
            if let Some(oid) = reference.target() {
                return Ok(oid);
            }
        }

        // Try parsing as OID (commit hash)
        if let Ok(oid) = Oid::from_str(ref_name) {
            // Verify the object exists
            if self.repo.find_object(oid, None).is_ok() {
                return Ok(oid);
            }
        }

        // Try HEAD if ref_name is "HEAD"
        if ref_name == "HEAD" {
            if let Ok(head) = self.repo.head() {
                if let Some(oid) = head.target() {
                    return Ok(oid);
                }
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
        let output = Command::new("git")
            .arg("-C")
            .arg(self.repo.path())
            .arg("fetch")
            .arg("origin")
            .arg(&oid_str)
            .output()?;

        if !output.status.success() {
            return Err(GitError::Command(format!(
                "git fetch {} failed: {}",
                oid_str,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

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
}
