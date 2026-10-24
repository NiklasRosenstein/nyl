//! `plan` and `reconcile` of one environment.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution),
//! [Planning](../../../../design/orchestration-core.md#planning),
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
//! and [Transition commits](../../../../design/orchestration-core.md#transition-commits).
//!
//! A reconcile takes the lease, chooses S (`rules::sources`), checks its
//! reachability, checks out a pinned worktree of S, renders the units
//! (`SourceRenderer`), builds desired documents (`rules::desired`), the graph
//! (`rules::graph`), and resolves references in dependency order
//! (`rules::references`), asking each ready unit's driver for its key inputs
//! (`rules::execution_key`). Then waves: select ready units
//! (`rules::selection`), execute them in parallel up to the concurrency
//! limit on scoped threads, each in its own worktree and temp dir, write a
//! checkpoint per finished unit on the run ref, and re-resolve at the same S.
//! When nothing is ready it writes one fenced transition commit per changed
//! state ref, rendered from the report, releases the lease, and deletes the
//! run ref. A reconcile that changes nothing writes nothing. A plan does the
//! resolution and calls driver `plan` for ready units; it never takes the
//! lease and writes nothing.
//!
//! Driver calls run on threads that are not inside an async runtime, so a
//! driver may block on its own runtime (as `FakePublication` does to render).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use nyl_core::orchestration::driver::{DriverSet, ToolRunner};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{SourceRenderer, SourceResolver};
use nyl_core::orchestration::state::StateLocation;
use nyl_core::orchestration::time::Duration;
use nyl_core::resources::environment::Environment;

use super::Context;
use crate::fault::FaultInjector;
use crate::run_report::RunReport;

/// Default number of units a wave runs at once.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// What a run uses beyond the state store, clock, and identifiers.
pub struct RunContext<'a> {
    pub base: &'a Context<'a>,
    pub sources: &'a dyn SourceResolver,
    pub renderer: &'a dyn SourceRenderer,
    pub drivers: &'a DriverSet,
    pub tools: &'a dyn ToolRunner,
    pub faults: &'a dyn FaultInjector,
    /// A directory this invocation owns: pinned worktrees and per-execution
    /// temp dirs are created beneath it.
    pub workspace: &'a Path,
    /// Variables the invocation admits from its process, such as `PATH` and
    /// a unit's `env.passthrough`; the command line reads them, never the shell.
    pub process_env: &'a BTreeMap<String, String>,
}

/// One `plan` or `reconcile` of one environment.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub environment: EnvironmentName,
    /// The Environment as the entry worktree declares it, with group
    /// defaults applied: the source of `source`, `state`, `protectedRefs`,
    /// `allowUnprotectedSource`, and `admission`.
    pub entry: Environment,
    pub location: StateLocation,
    /// The entry worktree's commit, S when `source` is omitted.
    pub entry_commit: CommitId,
    /// Worktree-rooted directory of the entry `nyl.toml`, to locate the
    /// project at S.
    pub entry_project_dir: String,
    /// `--unit`/`--units`; `None` executes every ready unit.
    pub units: Option<BTreeSet<UnitName>>,
    pub concurrency: usize,
    /// `--source <rev>`.
    pub source_override: Option<String>,
    /// `--wait-lease`.
    pub wait: Option<Duration>,
}

/// `nyl plan -e <env>`.
pub fn plan(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let _ = (context, request);
    todo!("cli-integration")
}

/// `nyl reconcile -e <env>`.
pub fn reconcile(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let _ = (context, request);
    todo!("cli-integration")
}
