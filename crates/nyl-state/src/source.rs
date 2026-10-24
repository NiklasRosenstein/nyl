//! The Git implementation of [`SourceResolver`]: revisions, reachability
//! from protected refs, and pinned worktrees of source commits.
//!
//! Contract: [Source](../../../design/orchestration-core.md#source) and
//! [Pinned commits](../../../design/orchestration-core.md#pinned-commits).
//! Every execution gets its own detached checkout of the commit it runs, so
//! no unit reads another commit's files or the entry worktree's uncommitted
//! changes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver, SourceResult};
use nyl_git::GitAuth;

/// Sources in the clone at `git_dir` and, by URL, in other repositories,
/// fetched into it.
pub struct GitSourceResolver {
    git_dir: PathBuf,
    auth: GitAuth,
    /// Read URLs of repositories named by credential-free URL, as for
    /// [`crate::git::Endpoint`].
    endpoints: BTreeMap<String, String>,
}

impl GitSourceResolver {
    /// Resolve sources through the repository at `git_dir`, the entry clone,
    /// which [`SourceRepository::This`] names.
    pub fn new(git_dir: impl Into<PathBuf>, auth: GitAuth) -> Self {
        Self {
            git_dir: git_dir.into(),
            auth,
            endpoints: BTreeMap::new(),
        }
    }

    /// Read the repository that sources name by `identity` from `url`.
    #[must_use]
    pub fn with_endpoint(mut self, identity: impl Into<String>, url: impl Into<String>) -> Self {
        self.endpoints.insert(identity.into(), url.into());
        self
    }
}

impl SourceResolver for GitSourceResolver {
    fn resolve_revision(&self, repository: &SourceRepository, revision: &str) -> SourceResult<CommitId> {
        let _ = (repository, revision, &self.git_dir, &self.auth, &self.endpoints);
        todo!("drivers-sources")
    }

    fn reachable_from(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        refs: &[String],
    ) -> SourceResult<Option<String>> {
        let _ = (repository, commit, refs);
        todo!("drivers-sources")
    }

    fn worktree(&self, repository: &SourceRepository, commit: &CommitId, path: &Path) -> SourceResult<()> {
        let _ = (repository, commit, path);
        todo!("drivers-sources")
    }

    fn default_branch(&self, repository: &SourceRepository) -> SourceResult<String> {
        let _ = repository;
        todo!("drivers-sources")
    }
}
