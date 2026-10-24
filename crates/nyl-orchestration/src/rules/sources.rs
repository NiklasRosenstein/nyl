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
//!   same rule ([`run_source_refs`]). A locked commit (a unit source's or the
//!   Environment source's `revision` + `commit`) must be reachable from
//!   `protectedRefs` or its own revision, and `allowUnprotectedSource` never
//!   waives that ([`pin_refs`]).
//! - A unit executes from one worktree: its effective commit
//!   ([`effective_commit`]).
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

/// The refs the run source S must be reachable from, or `None` when
/// `allowUnprotectedSource` waives the check. `entry` is the defaults-applied
/// entry Environment ([`nyl_core::resources::environment::effective_environment`]);
/// `default_branch` is the repository's default branch, the default of
/// `protectedRefs`. The waiver covers S only (including a `--source`
/// override), never pinned unit sources ([`pin_refs`]).
pub fn run_source_refs(entry: &Environment, default_branch: &str) -> Option<Vec<String>> {
    let _ = (entry, default_branch);
    todo!("run-model")
}

/// The refs a pinned commit must be reachable from, whatever
/// `allowUnprotectedSource` says ("every commit Nyl pins must stay
/// fetchable"): the effective `protectedRefs` (default the repository's
/// default branch) plus `own_revision`, the revision a lock or an
/// Environment `source.revision`+`commit` names.
pub fn pin_refs(entry: &Environment, default_branch: &str, own_revision: Option<&str>) -> Vec<String> {
    let _ = (entry, default_branch, own_revision);
    todo!("run-model")
}

/// The finding for a commit that none of `refs` reaches: it names the
/// commit and the refs searched
/// ([`Code::SourceUnreachable`](nyl_core::orchestration::codes::Code::SourceUnreachable)).
pub fn unreachable(commit: &CommitId, refs: &[String]) -> Finding {
    let _ = (commit, refs);
    todo!("run-model")
}

/// A Git source field naming a revision, which the shell resolves (or, with
/// `commit`, checks for reachability) before [`pin_source_fields`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLock {
    /// The field's JSON Pointer, such as `/source`.
    pub field: String,
    pub repository: SourceRepository,
    pub revision: String,
    /// A locked commit (`revision` + `commit`): used as is once the shell has
    /// checked it is reachable from [`pin_refs`] with this revision.
    pub commit: Option<CommitId>,
}

/// The revisions and locks the Git source fields of `spec` name. `fields`
/// are the driver's `source_fields`. A field in another repository
/// (`repositoryRef`/`repository`) is an error in part 3.
pub fn source_revisions(spec: &Value, fields: &[String]) -> Result<Vec<SourceLock>, String> {
    let _ = (spec, fields);
    todo!("run-model")
}

/// Write each Git source field's resolved `commit` and its worktree-rooted
/// `path`, resolved against `project_dir` by the local path rule, into
/// `resolved_spec`. The commit is S without `revision`, the lock's `commit`
/// when one is set, else the revision's tip from `revisions`.
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

/// The unit's effective commit: the one commit its pinned Git source fields
/// name, or S when it has none. The shell checks out this commit as the
/// worktree of key inputs and executions; it is recorded in the resolved
/// source fields, while `sourceRevision` stays S. Fields naming different
/// commits are an error in part 3 (one worktree per execution).
pub fn effective_commit(
    resolved_spec: &Value,
    fields: &[String],
    source_commit: &CommitId,
) -> Result<CommitId, String> {
    let _ = (resolved_spec, fields, source_commit);
    todo!("run-model")
}
