//! One reusable worktree per concurrency slot.
//!
//! Contract: [Execution](../../../../../design/orchestration-core.md#execution).
//! A run keeps one worktree per slot, beneath its session directory, and
//! resets it to each unit's commit through
//! [`SourceResolver::reset_worktree`], so a unit sees exactly what a fresh
//! checkout holds without paying a full checkout per unit. A slot is used by
//! one execution at a time: the wave executor gives job `i` of a batch slot
//! `i`, and batches never exceed the concurrency limit. Resolution worktrees
//! (`source-<commit>`), which readers share, are separate and never reset.

use std::path::{Path, PathBuf};

use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::{SourceRepository, SourceResolver};

/// The slot worktrees of one plan or reconcile.
#[derive(Debug)]
pub(crate) struct Slots {
    directory: PathBuf,
}

impl Slots {
    /// Slots beneath `session`, the operation's own directory, which the
    /// session removes when it ends.
    pub(crate) fn new(session: &Path) -> Self {
        Self {
            directory: session.join("slots"),
        }
    }

    /// Main and independent source-field worktrees of one unit in a slot.
    pub(crate) fn checkout_unit(
        &self,
        sources: &dyn SourceResolver,
        slot: usize,
        commit: &CommitId,
        driver: &dyn nyl_core::orchestration::driver::Driver,
        spec: &serde_json::Value,
        project_dir: &str,
    ) -> Result<
        (
            PathBuf,
            std::collections::BTreeMap<String, nyl_core::orchestration::driver::SourceRoot>,
        ),
        String,
    > {
        let worktree = self.directory.join(format!("slot-{slot}"));
        let paths = crate::shell::sources::checkout_paths(driver, spec, project_dir, &SourceRepository::This);
        sources
            .reset_worktree_with_paths(&SourceRepository::This, commit, &worktree, paths.as_deref())
            .map_err(|error| format!("cannot check out commit {commit}: {error}"))?;
        let roots = crate::shell::sources::prepare_source_roots(
            sources,
            driver,
            spec,
            commit,
            project_dir,
            &self.directory.join(format!("slot-{slot}-sources")),
        )?;
        Ok((worktree, roots))
    }

    /// The worktree of `slot`, reset to `commit`.
    pub(crate) fn checkout(
        &self,
        sources: &dyn SourceResolver,
        slot: usize,
        commit: &CommitId,
    ) -> Result<PathBuf, String> {
        let worktree = self.directory.join(format!("slot-{slot}"));
        sources
            .reset_worktree(&SourceRepository::This, commit, &worktree)
            .map_err(|error| format!("cannot check out commit {commit}: {error}"))?;
        Ok(worktree)
    }
}
