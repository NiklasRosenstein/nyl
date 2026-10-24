//! The source commit S, pinned Git sources, and reachability.
//!
//! Contract: [Source](../../../../design/orchestration-core.md#source),
//! [Units](../../../../design/orchestration-core.md#units) ("Git source
//! shape"), and [Pinned commits](../../../../design/orchestration-core.md#pinned-commits).
//!
//! - S is the entry worktree's commit when the Environment's `source` is
//!   omitted, the tip of `source.revision`, or `source.commit` when both are
//!   set; `--source` overrides it for one invocation, recorded as `override`.
//!   (`fromPromotion` is M6.)
//! - S must be reachable from `protectedRefs` (default: the repository's
//!   default branch) unless `allowUnprotectedSource`; an override follows the
//!   same rule. A unit source with `revision` must be reachable from
//!   `protectedRefs` or that revision.
//! - Resolution writes the resolved commit and the worktree-rooted path into
//!   every Git source field of a resolved spec; without `revision` a source
//!   is S.
//!
//! The shell performs the Git reads these rules ask for through
//! `SourceResolver`; the rules only decide.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::CommitId;
use nyl_core::orchestration::ports::SourceRepository;
use nyl_core::orchestration::state::SourceChoice;
use nyl_core::resources::environment::Environment;
use serde_json::Value;

/// How to find S.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRequest {
    /// The entry worktree's commit.
    Entry(CommitId),
    /// The tip of a revision of this repository.
    Revision(String),
    /// A commit, with the revision it must be reachable from.
    Commit { revision: String, commit: CommitId },
    /// `--source <rev>`: a revision or commit for this invocation only.
    Override(String),
}

impl SourceRequest {
    /// How the environment record names the choice.
    pub fn choice(&self) -> SourceChoice {
        todo!("run-model")
    }
}

/// The request for S from the entry worktree's Environment, its commit, and
/// an optional `--source` override.
pub fn source_request(entry: &Environment, entry_commit: &CommitId, override_revision: Option<&str>) -> SourceRequest {
    let _ = (entry, entry_commit, override_revision);
    todo!("run-model")
}

/// The refs S must be reachable from, or `None` when
/// `allowUnprotectedSource` waives the check. `default_branch` is the
/// repository's default branch, the default of `protectedRefs`.
pub fn protected_refs(entry: &Environment, default_branch: &str) -> Option<Vec<String>> {
    let _ = (entry, default_branch);
    todo!("run-model")
}

/// The finding for a commit that none of `refs` reaches: it names the
/// commit and the refs searched
/// ([`Code::SourceUnreachable`](nyl_core::orchestration::codes::Code::SourceUnreachable)).
pub fn unreachable(commit: &CommitId, refs: &[String]) -> Finding {
    let _ = (commit, refs);
    todo!("run-model")
}

/// Revisions the Git source fields of `spec` name, which the shell resolves
/// before [`pin_source_fields`]. `fields` are the driver's `source_fields`.
pub fn source_revisions(spec: &Value, fields: &[String]) -> Result<Vec<(SourceRepository, String)>, String> {
    let _ = (spec, fields);
    todo!("run-model")
}

/// Write each Git source field's resolved `commit` (S without `revision`,
/// else the revision's commit from `revisions`) and its worktree-rooted
/// `path`, resolved against `project_dir` by the local path rule, into
/// `resolved_spec`.
pub fn pin_source_fields(
    resolved_spec: &mut Value,
    fields: &[String],
    project_dir: &str,
    source_commit: &CommitId,
    revisions: &BTreeMap<(SourceRepository, String), CommitId>,
) -> Result<(), String> {
    let _ = (resolved_spec, fields, project_dir, source_commit, revisions);
    todo!("run-model")
}
