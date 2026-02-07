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

        tracing::debug!("Creating git worktree at {} for commit {}", worktree_path.display(), oid_str);

        // Use git worktree add command (git2-rs doesn't have direct worktree support)
        let output = Command::new("git")
            .arg("-C")
            .arg(bare_repo_path)
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(worktree_path)
            .arg(&oid_str)
            .output()?;

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
        tracing::debug!("Force checking out commit {} in worktree {}", oid, worktree_path.display());

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
    #[allow(dead_code)]
    pub fn remove_worktree(bare_repo_path: &Path, worktree_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(bare_repo_path)
            .arg("worktree")
            .arg("remove")
            .arg("--force")
            .arg(worktree_path)
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
            .arg(bare_repo_path)
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_worktree_manager_structure() {
        // Basic structure test - actual functionality requires a real Git repository
        // More comprehensive tests would be integration tests
    }
}
