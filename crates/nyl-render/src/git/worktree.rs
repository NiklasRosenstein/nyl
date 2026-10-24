use git2::{build::CheckoutBuilder, Oid, Repository};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::error::{GitError, Result};

/// Manages Git worktrees for isolated checkouts
pub struct WorktreeManager;

impl WorktreeManager {
    /// Get or create a worktree for the specified ref and OID
    ///
    /// If the worktree already exists, it performs a force checkout to the specified OID,
    /// dropping any local changes. Otherwise, it creates a new worktree.
    pub fn get_or_create_worktree(
        bare_repo_path: &Path,
        _git_ref: &str,
        oid: Oid,
        worktree_path: &Path,
    ) -> Result<PathBuf> {
        if worktree_path.exists() {
            // Worktree exists, perform force checkout
            Self::force_checkout(worktree_path, oid)?;
        } else {
            // Create new worktree
            Self::create_worktree(bare_repo_path, oid, worktree_path)?;
        }

        Ok(worktree_path.to_path_buf())
    }

    /// Create a new worktree at the specified path
    fn create_worktree(bare_repo_path: &Path, oid: Oid, worktree_path: &Path) -> Result<()> {
        // Create parent directory
        if let Some(parent) = worktree_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let oid_str = oid.to_string();

        tracing::debug!(
            "Creating git worktree at {} for commit {}",
            worktree_path.display(),
            oid_str
        );

        // Use git worktree add command (git2-rs doesn't have direct worktree support)
        let output = Command::new("git")
            .arg("-C")
            .arg(git_cli_path(bare_repo_path))
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(git_cli_path(worktree_path))
            .arg(&oid_str)
            .output()
            .map_err(|e| {
                GitError::WorktreeFailed(format!(
                    "Failed to spawn 'git worktree add' for bare repo {}: {}. Ensure the 'git' CLI is installed and in PATH.",
                    bare_repo_path.display(),
                    e
                ))
            })?;

        if !output.status.success() {
            return Err(GitError::WorktreeFailed(format!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        tracing::debug!("Git worktree created successfully");

        Ok(())
    }

    /// Force checkout to a specific OID, dropping local changes
    fn force_checkout(worktree_path: &Path, oid: Oid) -> Result<()> {
        tracing::debug!(
            "Force checking out commit {} in worktree {}",
            oid,
            worktree_path.display()
        );

        // Open the worktree repository
        let repo = Repository::open(worktree_path)?;

        // Get the commit object
        let commit = repo.find_commit(oid)?;

        // Get the tree from the commit
        let tree = commit.tree()?;

        // Perform force checkout
        let mut checkout_builder = CheckoutBuilder::new();
        checkout_builder.force();
        checkout_builder.remove_untracked(true);

        repo.checkout_tree(tree.as_object(), Some(&mut checkout_builder))?;

        // Set HEAD to detached state at this commit
        repo.set_head_detached(oid)?;

        Ok(())
    }

    /// Remove a worktree (cleanup)
    pub fn remove_worktree(bare_repo_path: &Path, worktree_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(git_cli_path(bare_repo_path))
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(git_cli_path(worktree_path))
            .output()?;

        if !output.status.success() {
            return Err(GitError::WorktreeFailed(format!(
                "Failed to remove worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }

    /// Prune stale worktree references
    #[allow(dead_code)]
    pub fn prune_worktrees(bare_repo_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(git_cli_path(bare_repo_path))
            .arg("worktree")
            .arg("prune")
            .output()?;

        if !output.status.success() {
            return Err(GitError::WorktreeFailed(format!(
                "Failed to prune worktrees: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(())
    }
}

/// Git for Windows needs ordinary drive/UNC paths when writing worktree links.
/// Rust's canonical paths retain their verbatim prefix for filesystem access;
/// convert only the arguments crossing the Git command-line boundary.
fn git_cli_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next() {
            let mut ordinary = match prefix.kind() {
                Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:", char::from(drive))),
                Prefix::VerbatimUNC(server, share) => {
                    let mut unc = PathBuf::from(r"\\");
                    unc.push(server);
                    unc.push(share);
                    unc
                }
                _ => return path.to_path_buf(),
            };
            ordinary.push(components.as_path());
            return ordinary;
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_worktree_lifecycle_with_canonical_paths() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let bare_path = root.join("bare repository");
        let repository = Repository::init_bare(&bare_path).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        let first = repository
            .commit(Some("refs/heads/main"), &signature, &signature, "First", &tree, &[])
            .unwrap();
        let second = repository
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Second",
                &tree,
                &[&repository.find_commit(first).unwrap()],
            )
            .unwrap();
        let worktree_path = root.join("nested/worktree with spaces");
        let checkout = WorktreeManager::get_or_create_worktree(&bare_path, "main", first, &worktree_path).unwrap();
        assert_eq!(
            Repository::open(&checkout).unwrap().head().unwrap().target(),
            Some(first)
        );
        let checkout = WorktreeManager::get_or_create_worktree(&bare_path, "main", second, &worktree_path).unwrap();
        assert_eq!(
            Repository::open(&checkout).unwrap().head().unwrap().target(),
            Some(second)
        );
        WorktreeManager::remove_worktree(&bare_path, &worktree_path).unwrap();
        assert!(!worktree_path.exists());
        WorktreeManager::prune_worktrees(&bare_path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn test_git_cli_paths_preserve_drive_unc_and_unicode() {
        for (canonical, ordinary) in [
            (r"\\?\C:\cache\worktree with spaces", r"C:\cache\worktree with spaces"),
            (r"\\?\UNC\server\share\cache\chärt", r"\\server\share\cache\chärt"),
            (r"C:\cache\worktree", r"C:\cache\worktree"),
            (r"\\server\share\cache", r"\\server\share\cache"),
        ] {
            assert_eq!(git_cli_path(Path::new(canonical)), PathBuf::from(ordinary));
        }
    }
}
