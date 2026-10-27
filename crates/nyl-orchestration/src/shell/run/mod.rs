//! `plan` and `reconcile` of one environment.
//!
//! Contract: [Execution](../../../../../design/orchestration-core.md#execution),
//! [Planning](../../../../../design/orchestration-core.md#planning),
//! [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases),
//! and [Transition commits](../../../../../design/orchestration-core.md#transition-commits).
//!
//! A reconcile takes the lease, chooses S (`rules::sources`), checks its
//! reachability, checks out a pinned worktree of S, renders the units
//! (`SourceRenderer`), builds desired documents (`rules::desired`), the graph
//! (`rules::graph`), and resolves references in dependency order
//! (`rules::references`), asking each ready unit's driver for its key inputs
//! (`rules::execution_key`). Then waves: select ready units
//! (`rules::selection`), execute them in parallel up to the concurrency
//! limit on scoped threads, each in its concurrency slot's worktree
//! ([`slots`]) and its own temp dir ([`execute`]), write a
//! checkpoint per finished unit on the run ref, and re-resolve at the same S.
//! When nothing is ready it writes one fenced transition commit per changed
//! state ref, rendered from the report, releases the lease, and deletes the
//! run ref. A reconcile that changes nothing writes nothing. A plan does the
//! resolution and calls driver `plan` for ready units; it never takes the
//! lease and writes nothing.
//!
//! Driver calls run on threads that are not inside an async runtime, so a
//! driver may block on its own runtime (as `FakePublication` does to render):
//! the whole operation runs on a scoped thread of its own, and executions on
//! further scoped threads.
//!
//! A run whose lease was lost, or that hit a store failure (a lease update,
//! run record, or transition commit) after a unit's checkpoint was written,
//! keeps its run ref, marked finished, so executed units' results are not
//! lost; every other run deletes it and releases the lease, including runs
//! that change nothing and runs whose units failed.
//!
//! Takeover ([`crate::rules::takeover`]): after the snapshot the run reads
//! the run refs left by runs before it, overlays the desired documents of
//! units they created on the snapshot it renders against (so their uids are
//! reused), imports their checkpoints after each resolution until nothing
//! new is imported, records units it creates on its run ref before starting
//! them, and, once its transition commit landed (or it released without
//! changes), deletes the leftover run refs or retires them to evidence refs.
//!
//! Cancellation ([`Cancellation`], "Runs and leases" item 7): once
//! cancelled, the run starts no unit, checkpoints the units that finish
//! (units still running become `uncertain`), keeps its run ref marked
//! finished, releases the lease, and writes no transition commit
//! (`NYL-RUN-CANCELLED`, exit 4).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{
    DriverOperation, DriverSet, ExecutionContext, KeyInputs, Outcome, ResolutionContext, Supported, ToolRunner,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, RunId, UnitAddress, UnitName};
use nyl_core::orchestration::ports::{
    ArgoObserver, Checkpoint, CommitIdentity, EnvironmentSnapshot, OperatorPrompt, RenderedSource, SourceRenderer,
    SourceRepository, SourceResolver, StateError, TransitionChanges,
};
use nyl_core::orchestration::state::{
    ApprovalRecord, ArtifactDigest, ArtifactDocument, ArtifactUnit, Condition, DesiredUnit, Lifecycle, ObservedUnit,
    Operation, Readiness, Receipt, Run, RunKind, StateLocation, StateV1,
};
use nyl_core::orchestration::time::{Duration, Timestamp};
use nyl_core::resources::environment::Environment;
use nyl_core::resources::units::OutputDeclaration;
use serde_json::{json, Map, Value};

use super::lease::{self, HeldLease};
use super::{store_error, Context};
use crate::fault::{FaultAction, FaultPoint};
use crate::rules::desired::{
    adopt_tombstone, desire, hold_pending, same_pending, DesireInputs, Desired, LifecycleOutcome, LifecycleWrite,
};
use crate::rules::drift;
use crate::rules::execution_key::{execution_key, stored_fingerprint, KeyMaterial};
use crate::rules::freshness::{receipt_is_current, same_incarnation};
use crate::rules::graph::{extract, topological_order};
use crate::rules::references::{resolve_unit, ReferenceView};
use crate::rules::selection::{batches, local_approve, select, Candidates, Skipped};
use crate::rules::sources::{
    effective_commit, pin_refs, pin_source_fields, root_local_paths, run_source_refs, source_request, source_revisions,
    unreachable, unsupported_source, SourceRequest,
};
use crate::rules::takeover::{self, ImportInputs, Leftovers};
use crate::rules::teardown::TeardownOptions;
use crate::rules::unit_state::{receipt_state, unit_state, NO_CURRENT_RECEIPT};
use crate::run_report::{BlockedBy, EnvironmentRunReport, ReportKind, RunReport, UnitReport, UnitResult, UnitState};
use crate::transition::Trailers;
pub use cancel::{Cancellation, SIGINT, SIGTERM};
use execute::{execute_batch, Execution};
use slots::Slots;

mod cancel;
mod execute;
pub(crate) mod slots;
mod teardown;
pub(crate) mod wait;

/// Default number of units a wave runs at once.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// What a run uses beyond the state store, clock, and identifiers.
pub struct RunContext<'a> {
    pub base: &'a Context<'a>,
    pub sources: &'a dyn SourceResolver,
    pub renderer: &'a dyn SourceRenderer,
    pub drivers: &'a DriverSet,
    pub tools: &'a dyn ToolRunner,
    /// A directory this invocation owns: pinned worktrees and per-execution
    /// temp dirs are created beneath it.
    pub workspace: &'a Path,
    /// A snapshot of the whole process environment, read once by the command
    /// line, never by the shell. The shell admits from it only `PATH` and
    /// each unit's declared `env.passthrough` into that unit's
    /// `ExecutionContext.env`/`ResolutionContext.env`; nothing else reaches a
    /// driver. Units declaring `env.secrets` are rejected in M3
    /// (`NYL-UNIT-SECRETS-UNSUPPORTED`) because no secrets provider is wired.
    pub process_env: &'a BTreeMap<String, String>,
    /// Who authors commits drivers write, such as publication commits, with the
    /// clock's time: the identity the command line resolved for state
    /// commits. `None` for `plan`, which writes nothing.
    pub commit_identity: Option<&'a CommitIdentity>,
    /// Reads Argo CD for a publication's `observe` teardown wait; `None`
    /// when no observer is configured, which makes such a wait uncertain
    /// with `observation-failed`.
    pub observer: Option<&'a dyn ArgoObserver>,
    /// The terminal of a run waiting on a teardown, when stdin is one: Enter
    /// confirms the removal.
    pub prompt: Option<&'a dyn OperatorPrompt>,
}

/// Which operation runs on the shared run path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RunMode {
    /// `nyl reconcile` (and `nyl plan`).
    #[default]
    Reconcile,
    /// `nyl recover -e <env>` importing a broken lease's run: operation
    /// `recover`, nothing executes, then the optional `--unit` action
    /// applies in the same commit. It writes no run record: its one
    /// transition commit is all it writes, and if it crashes the broken
    /// run's refs stay for the next import. Contract:
    /// [Recovery](../../../../../design/orchestration-core.md#recovery), "Importing a broken run".
    Recover { unit: Option<super::recover::RecoverUnit> },
    /// `nyl teardown` (and `nyl plan --teardown`): operation `teardown`,
    /// acting on the stored desired documents. Contract:
    /// [Deletion](../../../../../design/orchestration-core.md#deletion), "Explicit teardown".
    Teardown { target: TeardownTarget },
}

impl RunMode {
    /// The operation the run records in its lease, run ref, and commit.
    pub fn operation(&self) -> Operation {
        match self {
            Self::Reconcile => Operation::Reconcile,
            Self::Recover { .. } => Operation::Recover,
            Self::Teardown { .. } => Operation::Teardown,
        }
    }

    /// Whether the run executes or tears down units; a recover import does not.
    pub fn executes(&self) -> bool {
        !matches!(self, Self::Recover { .. })
    }

    /// Whether the run writes a run record (run ref and checkpoints): every
    /// mode that executes; a recover import writes only its transition commit.
    pub fn writes_run_record(&self) -> bool {
        self.executes()
    }

    /// Whether the run writes lifecycle changes (omission, return,
    /// retention, release, kind change, teardown progress): reconcile and
    /// teardown do; a recover import writes only what the import itself
    /// changes (receipts, conditions, adopted documents and uids).
    pub fn writes_lifecycle(&self) -> bool {
        !matches!(self, Self::Recover { .. })
    }

    /// The report kind of a run in this mode.
    pub fn report_kind(&self) -> ReportKind {
        match self {
            Self::Teardown { .. } => ReportKind::TeardownReport,
            Self::Reconcile | Self::Recover { .. } => ReportKind::ReconcileReport,
        }
    }
}

/// What `nyl teardown` tears down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeardownTarget {
    /// `--unit <u>…`.
    Units(BTreeSet<UnitName>),
    /// `--all`: every unit, dependents first; kinds that cannot tear down,
    /// `deletionPolicy: Retain` units, and retained tombstones are released
    /// (resources kept), and units still in the ownership set are held
    /// afterwards, without an incarnation. Held units only record the
    /// request.
    All,
}

/// The lifecycle options of a run. Contract:
/// [Deletion](../../../../../design/orchestration-core.md#deletion),
/// [Execution](../../../../../design/orchestration-core.md#execution)
/// (`--leaving`), and [Kubernetes publication
/// unit](../../../../../design/orchestration-core.md#kubernetes-publication-unit)
/// (`--confirm-removed`, `--allow-incomplete`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct LifecycleOptions {
    /// `--allow-teardown`: omission and kind-change teardowns proceed.
    pub allow_teardown: bool,
    /// `--allow-dependents`: tear down units retained tombstones cite.
    pub allow_dependents: bool,
    /// `--allow-incomplete`: proceed despite unmet readiness requirements
    /// or an unfinished teardown wait, recorded with a condition.
    pub allow_incomplete: bool,
    /// `reconcile --leaving`: act only on units outside the ownership set.
    pub leaving: bool,
    /// `teardown --confirm-removed`: the operator confirms a publication's removal.
    pub confirm_removed: bool,
    /// `teardown --hold`: hold the units after their teardown, in the same
    /// operation ("tear down, then hold").
    pub hold: bool,
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
    /// `--local`: the run continues from the clone's local state branches
    /// and commits there with `Nyl-Local: true`; the command line passes a
    /// [`super::local::LocalStore`] as the context's store. `None` for an
    /// ordinary run. Contract: [Local runs](../../../../../design/orchestration-core.md#local-runs).
    pub local: Option<super::local::LocalRun>,
    /// Set by the command line's SIGINT/SIGTERM handler; never set by
    /// default. Contract: [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases), item 7.
    pub cancellation: Cancellation,
    /// The operation on the run path.
    pub mode: RunMode,
    pub lifecycle: LifecycleOptions,
    /// `reconcile --unit <u>… --reapply`: the named units execute again
    /// although their receipts are current; requires `units` and `reason`.
    /// Contract: [Drift](../../../../../design/orchestration-core.md#drift).
    pub reapply: bool,
    /// `--fail-fast` (`reconcile`, `teardown`): after the first failure,
    /// uncertain outcome, or approval wait, start no further unit; the run
    /// still checkpoints what finished and writes its transition commit.
    /// By default the run continues with every unit that does not depend
    /// on one that stopped. Contract:
    /// [Execution](../../../../../design/orchestration-core.md#execution).
    pub fail_fast: bool,
    /// `--reason` of an operator action (`--reapply`, `teardown`, `recover`),
    /// recorded as `Nyl-Reason` next to `Nyl-Requested-By`.
    pub reason: Option<String>,
}

impl RunRequest {
    /// Whether the run is an operator action that records `Nyl-Requested-By`
    /// and `Nyl-Reason`: a teardown, a recover import, or a reapply.
    pub fn requested(&self) -> bool {
        !matches!(self.mode, RunMode::Reconcile) || self.reapply
    }

    /// The units `--reapply` forces: the `--unit` names, or none.
    pub fn reapply_units(&self) -> BTreeSet<UnitName> {
        match (&self.units, self.reapply) {
            (Some(units), true) => units.clone(),
            _ => BTreeSet::new(),
        }
    }
}

/// `nyl plan -e <env>`.
pub fn plan(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let environment = off_runtime(|| {
        let session = Session::new(context, request);
        let mut report = EnvironmentRunReport::new(request.environment.clone());
        plan_into(&session, &mut report);
        session.clean();
        report.finish(ReportKind::PlanReport);
        report
    });
    let mut report = RunReport::new(ReportKind::PlanReport);
    report.environments.push(environment);
    report
}

/// `nyl reconcile -e <env>`.
pub fn reconcile(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let environment = off_runtime(|| {
        let session = Session::new(context, request);
        let mut report = reconcile_environment(&session);
        session.clean();
        if request.local.is_some() {
            super::local::finish_local_report(&mut report, request);
        }
        report
    });
    let mut report = RunReport::new(request.mode.report_kind());
    report.environments.push(environment);
    report
}

/// Run `body` on a scoped thread of its own, outside any async runtime the
/// caller may be in.
pub(crate) fn off_runtime<T: Send>(body: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| join(scope.spawn(body)))
}

/// The value of a scoped thread, resuming its panic.
pub(crate) fn join<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    match handle.join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// A new directory `<prefix>-<n>` beneath the workspace, for one operation.
pub(crate) fn fresh_directory(workspace: &Path, prefix: &str) -> PathBuf {
    let mut index = 0usize;
    loop {
        let candidate = workspace.join(format!("{prefix}-{index}"));
        if !candidate.exists() {
            return candidate;
        }
        index += 1;
    }
}

impl RunContext<'_> {
    /// The workspace's render cache, shared by the invocation's runs.
    pub(crate) fn cache_dir(&self) -> PathBuf {
        self.workspace.join("cache")
    }
}

// ---------------------------------------------------------------------------
// Session: one operation's caches and workspace

/// One plan or reconcile: its workspace and the Git reads it caches, so a
/// run resolves each revision and checks each pin once, and never reads a
/// revision's tip twice.
struct Session<'r, 'a> {
    context: &'r RunContext<'a>,
    request: &'r RunRequest,
    /// This operation's own directory beneath the workspace.
    directory: PathBuf,
    default_branch: OnceCell<Result<String, Finding>>,
    revisions: RefCell<BTreeMap<String, CommitId>>,
    pins: RefCell<BTreeSet<(CommitId, String)>>,
    /// Read-only worktrees of effective commits, for key inputs.
    worktrees: RefCell<BTreeMap<CommitId, PathBuf>>,
    /// Key inputs per unit, by the resolved spec and commit they were read for.
    keys: RefCell<BTreeMap<UnitName, (Value, CommitId, KeyInputs)>>,
    executions: RefCell<usize>,
    /// One reusable worktree per concurrency slot.
    slots: Slots,
    /// The deadline of the lease a reconcile holds, which bounds every
    /// resolution deadline; `None` for a plan, which holds no lease.
    lease_deadline: Cell<Option<Timestamp>>,
}

/// What preparing a run produced: S, its rendering, and the desired
/// documents before references are resolved.
struct Prepared {
    source_commit: CommitId,
    rendered: RenderedSource,
    desired: Desired,
    order: Vec<UnitName>,
}

/// One resolution pass over the ownership set.
struct Resolved {
    /// The units the run may execute: held units are left out.
    units: BTreeMap<UnitName, DesiredUnit>,
    /// The effective commit of every unit with an execution key.
    commits: BTreeMap<UnitName, CommitId>,
    /// Resolution errors, per unit.
    errors: BTreeMap<UnitName, Vec<Finding>>,
    /// Held units of the ownership set as source would resolve them, for
    /// the `hold.pending` their documents record; never executed or written.
    shadow: BTreeMap<UnitName, DesiredUnit>,
}

/// What resolution reads besides the desired documents.
struct Evidence<'e> {
    observed: &'e BTreeMap<UnitName, ObservedUnit>,
    artifacts: &'e BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    observed_commit: Option<&'e CommitId>,
    run: Option<&'e RunId>,
    produced_in_run: &'e BTreeSet<UnitName>,
    previous: &'e BTreeMap<UnitName, DesiredUnit>,
}

impl<'r, 'a> Session<'r, 'a> {
    fn new(context: &'r RunContext<'a>, request: &'r RunRequest) -> Self {
        let directory = fresh_directory(context.workspace, "session");
        let slots = Slots::new(&directory);
        Self {
            context,
            request,
            directory,
            default_branch: OnceCell::new(),
            revisions: RefCell::new(BTreeMap::new()),
            pins: RefCell::new(BTreeSet::new()),
            worktrees: RefCell::new(BTreeMap::new()),
            keys: RefCell::new(BTreeMap::new()),
            executions: RefCell::new(0),
            slots,
            lease_deadline: Cell::new(None),
        }
    }

    fn environment(&self) -> &EnvironmentName {
        &self.request.environment
    }

    fn base(&self) -> &Context<'a> {
        self.context.base
    }

    /// Remove every worktree and temp dir of this operation.
    fn clean(&self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }

    /// A new directory for one execution or plan of `unit`.
    fn execution_directory(&self, unit: &UnitName) -> PathBuf {
        let mut count = self.executions.borrow_mut();
        *count += 1;
        self.directory.join(format!("exec-{count}-{unit}"))
    }

    /// The repository's default branch, read only when `protectedRefs` is
    /// not set, and once.
    fn default_branch(&self) -> Result<String, Finding> {
        if self
            .request
            .entry
            .spec
            .protected_refs
            .as_ref()
            .is_some_and(|refs| !refs.is_empty())
        {
            // Unused: the effective protected refs are the declared ones.
            return Ok(String::new());
        }
        self.default_branch
            .get_or_init(|| {
                self.context
                    .sources
                    .default_branch(&SourceRepository::This)
                    .map_err(|error| {
                        Finding::new(
                            Code::SourceUnavailable,
                            format!("cannot read the default branch of this repository: {error}"),
                            "set protectedRefs on the Environment, or fetch the repository's origin/HEAD",
                        )
                    })
            })
            .clone()
    }

    /// Check that `commit` is reachable from `refs`.
    fn check_reachable(&self, commit: &CommitId, refs: &[String]) -> Result<(), Finding> {
        match self
            .context
            .sources
            .reachable_from(&SourceRepository::This, commit, refs)
        {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(unreachable(commit, refs)),
            Err(error) => Err(source_unavailable(&format!(
                "cannot check whether commit {commit} is reachable: {error}"
            ))),
        }
    }

    /// Check a locked commit against [`pin_refs`], which
    /// `allowUnprotectedSource` never waives; once per commit and revision.
    fn check_pin(&self, commit: &CommitId, revision: &str) -> Result<(), Finding> {
        let key = (commit.clone(), revision.to_owned());
        if self.pins.borrow().contains(&key) {
            return Ok(());
        }
        let default = self.default_branch()?;
        let refs = pin_refs(&self.request.entry, &default, Some(revision));
        self.check_reachable(commit, &refs)?;
        self.pins.borrow_mut().insert(key);
        Ok(())
    }

    /// The commit a revision of this repository names, read once per run.
    fn resolve_revision(&self, revision: &str) -> Result<CommitId, Finding> {
        if let Some(commit) = self.revisions.borrow().get(revision) {
            return Ok(commit.clone());
        }
        let commit = self
            .context
            .sources
            .resolve_revision(&SourceRepository::This, revision)
            .map_err(|error| source_unavailable(&format!("cannot resolve revision {revision:?}: {error}")))?;
        self.revisions.borrow_mut().insert(revision.to_owned(), commit.clone());
        Ok(commit)
    }

    /// A read-only worktree of `commit` for key inputs, created once.
    fn resolution_worktree(&self, commit: &CommitId) -> Result<PathBuf, Finding> {
        if let Some(path) = self.worktrees.borrow().get(commit) {
            return Ok(path.clone());
        }
        let path = self.directory.join(format!("source-{commit}"));
        self.checkout(commit, &path)?;
        self.worktrees.borrow_mut().insert(commit.clone(), path.clone());
        Ok(path)
    }

    fn checkout(&self, commit: &CommitId, path: &Path) -> Result<(), Finding> {
        std::fs::create_dir_all(path)
            .map_err(|error| source_unavailable(&format!("cannot create the worktree {}: {error}", path.display())))?;
        self.context
            .sources
            .worktree(&SourceRepository::This, commit, path)
            .map_err(|error| source_unavailable(&format!("cannot check out commit {commit}: {error}")))
    }

    /// S: chosen by the source rules, resolved, and checked for
    /// reachability. An Environment `source.revision`+`commit` lock is also
    /// checked against [`pin_refs`].
    fn source_commit(&self) -> Result<(SourceRequest, CommitId), Finding> {
        let entry = &self.request.entry;
        if let Some(finding) = unsupported_source(entry) {
            return Err(finding);
        }
        let request = source_request(
            entry,
            &self.request.entry_commit,
            self.request.source_override.as_deref(),
        );
        let commit = match &request {
            SourceRequest::Entry(commit) => commit.clone(),
            SourceRequest::Revision(revision) | SourceRequest::Override(revision) => self.resolve_revision(revision)?,
            SourceRequest::Commit { revision, commit } => {
                self.check_pin(commit, revision)?;
                commit.clone()
            }
        };
        if entry.spec.allow_unprotected_source != Some(true) {
            let default = self.default_branch()?;
            if let Some(refs) = run_source_refs(entry, &default) {
                self.check_reachable(&commit, &refs)?;
            }
        }
        Ok((request, commit))
    }

    /// Choose S, render it, and build the desired documents and graph.
    fn prepare(&self, snapshot: &EnvironmentSnapshot) -> Result<Prepared, Vec<Finding>> {
        let (request, source_commit) = self.source_commit().map_err(|finding| vec![finding])?;
        let worktree = self
            .resolution_worktree(&source_commit)
            .map_err(|finding| vec![finding])?;
        let rendered = self
            .context
            .renderer
            .render(&worktree, self.environment(), &source_commit)
            .map_err(|error| vec![source_unavailable(&error.to_string())])?;
        let environment_spec = environment_spec(&rendered.environment, &self.request.entry);
        let desired = desire(
            &DesireInputs {
                source: &rendered,
                environment_spec: &environment_spec,
                source_choice: request.choice(),
                snapshot,
                drivers: self.context.drivers,
            },
            &mut || self.base().ids.uid(),
        );
        let mut errors = desired.errors.clone();
        if let Some(named) = &self.request.units {
            let known = |unit: &UnitName| desired.units.contains_key(unit) || desired.held.contains_key(unit);
            for unit in named.iter().filter(|unit| !known(unit)) {
                errors.push(Finding::new(
                    Code::UnitUnknown,
                    format!(
                        "--unit {unit} names no unit of the ownership set of {}",
                        self.environment()
                    ),
                    format!(
                        "list the environment's units with `nyl get units -e {}`",
                        self.environment()
                    ),
                ));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        // Held units of the ownership set are nodes too, as source renders
        // them, so their consumers' references are checked and ordered.
        let mut nodes = desired.units.clone();
        for (name, held) in &desired.held {
            if let Some(rendered) = &held.rendered {
                nodes.insert(name.clone(), rendered.clone());
            }
        }
        let graph = extract(self.environment(), &source_commit, &nodes)?;
        let order = topological_order(&graph);
        Ok(Prepared {
            source_commit,
            rendered,
            desired,
            order,
        })
    }

    /// Resolve every unit in dependency order against `evidence`. Held
    /// units resolve as `held` stores them: consumers read their receipts,
    /// and their rendering is resolved only in the shadow, for
    /// `hold.pending`. Fenced units stay blocked without an execution key.
    fn resolve(
        &self,
        prepared: &Prepared,
        evidence: &Evidence<'_>,
        held: &BTreeMap<UnitName, DesiredUnit>,
    ) -> Resolved {
        let mut units = prepared.desired.units.clone();
        units.extend(held.iter().map(|(name, unit)| {
            // A held unit without an incarnation (a marker included) declares
            // what source renders for it but has no receipt, so its
            // consumers' references are valid and stay blocked.
            let declared = match prepared.desired.held.get(name).and_then(|held| held.rendered.as_ref()) {
                Some(rendered) if unit.unit.uid.is_none() => {
                    let mut rendered = rendered.clone();
                    rendered.unit.uid = None;
                    rendered
                }
                _ => unit.clone(),
            };
            (name.clone(), declared)
        }));
        let mut commits = BTreeMap::new();
        let mut errors: BTreeMap<UnitName, Vec<Finding>> = BTreeMap::new();
        let mut shadow = BTreeMap::new();
        for name in &prepared.order {
            let (mut unit, in_shadow) = if held.contains_key(name) {
                match prepared.desired.held.get(name).and_then(|held| held.rendered.as_ref()) {
                    Some(rendered) => (rendered.clone(), true),
                    None => continue,
                }
            } else {
                match prepared.desired.units.get(name) {
                    Some(unit) => (unit.clone(), false),
                    None => continue,
                }
            };
            let resolution = {
                let view = ReferenceView {
                    environment: self.environment(),
                    desired: &units,
                    observed: evidence.observed,
                    artifacts: evidence.artifacts,
                    observed_commit: evidence.observed_commit,
                    run: evidence.run,
                    produced_in_run: evidence.produced_in_run,
                    previous: evidence.previous,
                };
                resolve_unit(&view, &unit)
            };
            unit.provenance = resolution.provenance;
            unit.readiness = resolution.readiness;
            if !resolution.errors.is_empty() && !in_shadow {
                errors.insert(
                    name.clone(),
                    resolution
                        .errors
                        .iter()
                        .map(|error| {
                            Finding::new(
                                Code::ReferenceInvalid,
                                format!("unit {name}: {error}"),
                                "correct the reference so that it names a declared, non-sensitive output or a listed artifact of a unit in the ownership set",
                            )
                        })
                        .collect(),
                );
            }
            let fenced = prepared.desired.fenced.contains(name);
            if let (Some(mut spec), true, false) = (resolution.resolved_spec, resolution.errors.is_empty(), fenced) {
                match self.key(
                    name,
                    &unit,
                    &mut spec,
                    &prepared.source_commit,
                    &prepared.rendered.project_dir,
                ) {
                    Ok(keyed) => {
                        unit.attestations = keyed.attestations;
                        unit.execution_key = Some(keyed.key);
                        unit.fingerprint = keyed.fingerprint;
                        unit.resolved_spec = Some(spec);
                        if !in_shadow {
                            commits.insert(name.clone(), keyed.commit);
                        }
                    }
                    Err(finding) if !in_shadow => errors.entry(name.clone()).or_default().push(finding),
                    Err(_) => {}
                }
            }
            if fenced {
                // It waits for the old incarnation, whatever its references say.
                unit.readiness = prepared.desired.units[name].readiness.clone();
            }
            if in_shadow {
                shadow.insert(name.clone(), unit);
            } else {
                units.insert(name.clone(), unit);
            }
        }
        for name in held.keys() {
            units.remove(name);
        }
        Resolved {
            units,
            commits,
            errors,
            shadow,
        }
    }

    /// Pin the unit's Git source fields in `spec`, root its other local path
    /// fields at the worktree, check them out, and compute its execution key.
    fn key(
        &self,
        name: &UnitName,
        unit: &DesiredUnit,
        spec: &mut Value,
        source_commit: &CommitId,
        project_dir: &str,
    ) -> Result<Keyed, Finding> {
        let driver = self
            .context
            .drivers
            .get(&unit.unit.api_version, &unit.unit.kind)
            .ok_or_else(|| {
                Finding::new(
                    Code::UnitKindUnknown,
                    format!("unit {name} has kind {} which no driver serves", unit.unit.kind),
                    "use a unit kind Nyl provides",
                )
            })?;
        let invalid = |message: String| {
            Finding::new(
                Code::UnitInvalid,
                format!("unit {name}: {message}"),
                "correct the unit's Git source fields",
            )
        };
        let fields = driver.source_fields(spec);
        let locks = source_revisions(spec, &fields).map_err(invalid)?;
        let mut revisions = BTreeMap::new();
        for lock in &locks {
            if let Some(commit) = &lock.commit {
                self.check_pin(commit, &lock.revision)?;
            } else {
                let commit = self.resolve_revision(&lock.revision)?;
                revisions.insert((lock.repository.clone(), lock.revision.clone()), commit);
            }
        }
        pin_source_fields(spec, &fields, project_dir, source_commit, &revisions).map_err(invalid)?;
        root_local_paths(spec, &driver.local_path_fields(spec), project_dir).map_err(invalid)?;
        let commit = effective_commit(spec, &fields, source_commit).map_err(invalid)?;

        let cached = self
            .keys
            .borrow()
            .get(name)
            .filter(|(cached_spec, cached_commit, _)| cached_spec == spec && *cached_commit == commit)
            .map(|(_, _, inputs)| inputs.clone());
        let inputs = if let Some(inputs) = cached {
            inputs
        } else {
            let worktree = self.resolution_worktree(&commit)?;
            let now = self.base().clock.now();
            let context = ResolutionContext {
                environment: self.environment().clone(),
                project_dir: project_path(&worktree, project_dir),
                worktree,
                env: admitted_env(self.context.process_env, &unit.spec),
                deadline: resolution_deadline(&self.base().settings, now, self.lease_deadline.get()),
                now,
                tools: self.context.tools,
                cache_dir: self.context.cache_dir(),
            };
            let inputs = driver.execution_key_inputs(&context, spec).map_err(|error| {
                Finding::new(
                    Code::DriverError,
                    format!("unit {name}: cannot read its execution key inputs: {error}"),
                    "check the unit's source files and tools, then run again",
                )
            })?;
            self.keys
                .borrow_mut()
                .insert(name.clone(), (spec.clone(), commit.clone(), inputs.clone()));
            inputs
        };
        let key = execution_key(&KeyMaterial {
            api_version: &unit.unit.api_version,
            kind: &unit.unit.kind,
            behavior_version: driver.behavior_version(),
            resolved_spec: spec,
            source_fields: &fields,
            inputs: &inputs,
        })
        .map_err(invalid)?;
        Ok(Keyed {
            key,
            fingerprint: inputs.fingerprint.as_deref().map(stored_fingerprint),
            commit,
            attestations: driver.attestations(spec),
        })
    }
}

/// The deadline of one resolution call: the resolution timeout from now,
/// but never past the held lease's deadline less its grace period, so no
/// fingerprint runs once another runner may break the lease.
fn resolution_deadline(settings: &super::Settings, now: Timestamp, lease: Option<Timestamp>) -> Timestamp {
    let own = settings.resolution_timeout.after(now);
    match lease {
        Some(lease) => own.min(lease::bound(settings, lease)),
        None => own,
    }
}

/// A resolved unit's key and what it was computed from.
struct Keyed {
    key: ExecutionKey,
    fingerprint: Option<String>,
    commit: CommitId,
    attestations: Vec<String>,
}

fn source_unavailable(message: &str) -> Finding {
    Finding::new(
        Code::SourceUnavailable,
        message,
        "check that the revision exists and the repository is reachable, then run again",
    )
}

/// The Environment spec the environment record keeps: S's effective spec,
/// with `source`, `state`, `protectedRefs`, `allowUnprotectedSource`, and
/// `admission` as the entry worktree declares them.
fn environment_spec(at_source: &Environment, entry: &Environment) -> Value {
    let mut spec = serde_json::to_value(&at_source.spec).unwrap_or_else(|_| Value::Object(Map::new()));
    let entry = serde_json::to_value(&entry.spec).unwrap_or_else(|_| Value::Object(Map::new()));
    if let Value::Object(spec) = &mut spec {
        for field in [
            "source",
            "state",
            "protectedRefs",
            "allowUnprotectedSource",
            "admission",
        ] {
            match entry.get(field) {
                Some(value) => {
                    spec.insert(field.to_owned(), value.clone());
                }
                None => {
                    spec.remove(field);
                }
            }
        }
    }
    spec
}

/// A unit's admitted variables: `PATH` and its declared `env.passthrough`,
/// from the process environment snapshot.
pub(crate) fn admitted_env(process_env: &BTreeMap<String, String>, spec: &Value) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(path) = process_env.get("PATH") {
        env.insert("PATH".to_owned(), path.clone());
    }
    let passthrough = spec
        .pointer("/env/passthrough")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    for name in passthrough {
        if let Some(value) = process_env.get(name) {
            env.insert(name.to_owned(), value.clone());
        }
    }
    env
}

/// The directory of `nyl.toml` inside a worktree, from its worktree-rooted form.
pub(crate) fn project_path(worktree: &Path, project_dir: &str) -> PathBuf {
    let relative = project_dir.trim_matches('/');
    if relative.is_empty() {
        worktree.to_path_buf()
    } else {
        worktree.join(relative)
    }
}

/// The longest timeout of `units`, which a phase running them needs.
pub(crate) fn longest_timeout<'u>(units: impl Iterator<Item = &'u DesiredUnit>) -> Duration {
    units.map(unit_timeout).max().unwrap_or_default()
}

/// A unit's `timeout`, else its kind's default from the contract's Defaults.
pub(crate) fn unit_timeout(unit: &DesiredUnit) -> Duration {
    let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
    if let Some(timeout) = spec
        .get("timeout")
        .and_then(|value| serde_json::from_value::<Duration>(value.clone()).ok())
    {
        return timeout;
    }
    match unit.unit.kind.as_str() {
        "Terraform" | "OpenTofu" | "OciImage" => Duration::from_mins(60),
        "KubernetesPublication" => Duration::from_mins(10),
        _ => Duration::from_mins(30),
    }
}

/// The declared outputs of a unit's spec.
fn declared_outputs(spec: &Value) -> BTreeMap<String, OutputDeclaration> {
    spec.get("outputs")
        .and_then(|outputs| serde_json::from_value(outputs.clone()).ok())
        .unwrap_or_default()
}

/// A stored blocked reason in the report's form.
fn blocked_by(unit: &DesiredUnit) -> Vec<BlockedBy> {
    let Readiness::Blocked { reasons } = &unit.readiness else {
        return Vec::new();
    };
    reasons
        .iter()
        .map(|reason| {
            let producer = reason
                .reason
                .strip_suffix(" has no current receipt")
                .and_then(|producer| UnitName::new(producer).ok());
            match producer {
                Some(producer) => BlockedBy {
                    pointer: reason.pointer.clone(),
                    unit: Some(producer),
                    reason: NO_CURRENT_RECEIPT.to_owned(),
                },
                None => BlockedBy {
                    pointer: reason.pointer.clone(),
                    unit: None,
                    reason: reason.reason.clone(),
                },
            }
        })
        .collect()
}

/// The failure or uncertainty message of the unit's current incarnation.
fn condition_message(unit: &DesiredUnit, observed: Option<&ObservedUnit>) -> Option<String> {
    let condition = same_incarnation(unit, observed)?.condition.as_ref()?;
    Some(condition.message().to_owned())
}

/// The warning listing the units that leave the ownership set.
fn leaving_warning(environment: &EnvironmentName, leaving: &[crate::run_report::LeavingUnit]) -> Finding {
    Finding::new(
        Code::PlanUnitsLeaving,
        format!(
            "{} unit(s) leave the ownership set of {environment}: {}",
            leaving.len(),
            leaving
                .iter()
                .map(|unit| format!("{} ({})", unit.unit, unit.reason))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        "restore the units in source or in the unitSelector's labels if they should stay; pass --fail-on-leaving to plan to make removals fail a pipeline",
    )
}

// ---------------------------------------------------------------------------
// plan

#[allow(clippy::too_many_lines)] // The plan's steps in order: snapshot, resolve, units, held units, the leaving section.
fn plan_into(session: &Session<'_, '_>, report: &mut EnvironmentRunReport) {
    let context = session.context;
    let request = session.request;
    let environment = session.environment();
    let snapshot = match context.base.store.snapshot(&request.location) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report.errors.push(store_error(environment, &error));
            return;
        }
    };
    match super::local::check_record(
        context.base.store,
        snapshot.record.as_ref(),
        environment,
        &request.location,
    ) {
        Ok(()) => {}
        // A plan of an environment without state shows what its first run does.
        Err(finding) if finding.code == Code::StateMissing => report.warnings.push(finding),
        Err(finding) => {
            report.errors.push(finding);
            return;
        }
    }
    if let RunMode::Teardown { target } = &request.mode {
        plan_teardown(session, report, &snapshot, target);
        return;
    }
    let prepared = match session.prepare(&snapshot) {
        Ok(prepared) => prepared,
        Err(findings) => {
            report.errors.extend(findings);
            return;
        }
    };
    report.source_commit = Some(prepared.source_commit.clone());
    report.leaving.clone_from(&prepared.desired.leaving);
    if !report.leaving.is_empty() {
        report.warnings.push(leaving_warning(environment, &report.leaving));
    }
    let produced = BTreeSet::new();
    let held_view: BTreeMap<UnitName, DesiredUnit> = prepared
        .desired
        .held
        .iter()
        .map(|(name, held)| (name.clone(), held.stored.clone()))
        .collect();
    // Returning tombstones whose key is unchanged are current at once.
    let mut observed = snapshot.observed.clone();
    let mut adoptions = BTreeSet::new();
    let resolved = loop {
        let resolved = session.resolve(
            &prepared,
            &Evidence {
                observed: &observed,
                artifacts: &snapshot.artifacts,
                observed_commit: snapshot.tips.observed.as_ref(),
                run: None,
                produced_in_run: &produced,
                previous: &snapshot.desired,
            },
            &held_view,
        );
        if !adopt_tombstones(&prepared, &resolved, &mut adoptions, &mut observed) {
            break resolved;
        }
    };
    report.errors.extend(resolved.errors.values().flatten().cloned());
    let attempted = BTreeSet::new();
    let selectable = selection_view(&resolved, &held_view);
    let candidates = Candidates {
        desired: &selectable,
        observed: &observed,
        drivers: context.drivers,
        named: None,
        attempted: &attempted,
        local: request.local.as_ref().map(|local| &local.approve),
        observations: &snapshot.observations,
        drift_repair: drift::drift_repair(&prepared.rendered.environment),
        // `nyl plan` has no `--reapply`.
        reapply: &BTreeSet::new(),
    };
    let (ready, skipped) = select(&candidates);
    // `drifted` is a unit state in plans too: the drifted units selection
    // would repair, and those that await an approval or a forced run.
    let drifted: BTreeSet<UnitName> = drift::repairs(&candidates, &ready)
        .into_iter()
        .filter(|(_, repair)| *repair == drift::Repair::Drift)
        .map(|(name, _)| name)
        .chain(
            skipped
                .into_iter()
                .filter(|(_, skip)| matches!(skip, Skipped::Drifted | Skipped::RepairApproval))
                .map(|(name, _)| name),
        )
        .collect();
    let ready: BTreeSet<UnitName> = ready.into_iter().collect();
    for (name, unit) in &resolved.units {
        if request.units.as_ref().is_some_and(|named| !named.contains(name)) {
            continue;
        }
        let current = observed.get(name);
        let mut entry = UnitReport::new(unit.unit.kind.clone());
        entry.receipt = Some(receipt_state(Some(unit), current));
        entry.last_run = current.and_then(ObservedUnit::last_run).cloned();
        entry.message = condition_message(unit, current);
        entry.blocked_by = blocked_by(unit);
        entry.state = Some(if drifted.contains(name) {
            UnitState::Drifted
        } else if ready.contains(name) {
            UnitState::Ready
        } else if matches!(unit.readiness, Readiness::Blocked { .. }) || unit.execution_key.is_none() {
            UnitState::Blocked
        } else {
            unit_state(unit, same_incarnation(unit, current), false)
        });
        if ready.contains(name) {
            if let Some(commit) = resolved.commits.get(name) {
                plan_unit(
                    session,
                    &prepared.rendered.project_dir,
                    unit,
                    commit,
                    current,
                    &mut entry,
                );
            }
        }
        report.units.insert(name.clone(), entry);
    }
    for (name, held) in &prepared.desired.held {
        if request.units.as_ref().is_some_and(|named| !named.contains(name)) {
            continue;
        }
        let pending = hold_pending(held, &prepared.source_commit, resolved.shadow.get(name));
        let mut stored = held.stored.clone();
        if let Lifecycle::Held { hold, .. } = &mut stored.lifecycle {
            hold.pending = pending;
        }
        let mut entry = teardown::held_report(environment, name, &stored, false);
        entry.result = None;
        entry.state = Some(UnitState::Held);
        report.units.insert(name.clone(), entry);
    }

    // The leaving section: what a reconcile with the same flags would do to
    // the units leaving the ownership set and the deleting ones.
    if request.units.is_some() {
        return;
    }
    let mut life = teardown::Life::new(&snapshot.desired);
    let mut artifacts = snapshot.artifacts.clone();
    for (name, write) in &prepared.desired.lifecycle {
        match (&write.desired, prepared.desired.units.contains_key(name)) {
            (Some(desired), true) if !prepared.desired.fenced.contains(name) => {
                life.desired.insert(name.clone(), desired.clone());
            }
            _ => life.apply(name, write.clone(), &mut observed, &mut artifacts),
        }
        if write.outcome == LifecycleOutcome::KindChange {
            let warning = kind_change_warning(
                environment,
                name,
                &snapshot.desired[name],
                &prepared.desired.units[name],
            );
            report.warnings.push(warning);
        }
    }
    for name in &prepared.desired.fenced {
        if matches!(
            snapshot.desired.get(name).map(|stored| &stored.lifecycle),
            Some(Lifecycle::Retained { .. })
        ) {
            life.releases.insert(name.clone());
        }
    }
    let options = TeardownOptions {
        allow_teardown: request.lifecycle.allow_teardown,
        allow_dependents: request.lifecycle.allow_dependents,
        allow_incomplete: request.lifecycle.allow_incomplete,
        named: None,
        explicit: false,
    };
    plan_leaving(
        session,
        report,
        &prepared.rendered.project_dir,
        &mut life,
        &observed,
        options,
        None,
    );
}

/// `nyl plan --teardown`: what `nyl teardown` would remove, from the
/// stored documents only.
fn plan_teardown(
    session: &Session<'_, '_>,
    report: &mut EnvironmentRunReport,
    snapshot: &EnvironmentSnapshot,
    target: &TeardownTarget,
) {
    let context = session.context;
    let request = session.request;
    let source = match &request.source_override {
        Some(revision) => match session.resolve_revision(revision) {
            Ok(commit) => Some(commit),
            Err(finding) => {
                report.errors.push(finding);
                return;
            }
        },
        None => None,
    };
    let ownership: BTreeSet<UnitName> = snapshot
        .environment
        .as_ref()
        .map(|record| record.ownership.keys().cloned().collect())
        .unwrap_or_default();
    let mut life = teardown::Life::new(&snapshot.desired);
    life.carry_decommission_holds(&ownership);
    let mut observed = snapshot.observed.clone();
    let mut artifacts = snapshot.artifacts.clone();
    let now = context.base.clock.now();
    if let Err(findings) = teardown::mark_teardowns(
        context,
        request,
        &mut life,
        target,
        &ownership,
        now,
        &mut observed,
        &mut artifacts,
    ) {
        report.errors.extend(findings);
        return;
    }
    let named = match target {
        TeardownTarget::Units(names) => Some(names.clone()),
        TeardownTarget::All => None,
    };
    let options = TeardownOptions {
        allow_teardown: true,
        allow_dependents: request.lifecycle.allow_dependents,
        allow_incomplete: request.lifecycle.allow_incomplete,
        named: named.as_ref(),
        explicit: true,
    };
    for (name, mut entry) in std::mem::take(&mut life.results) {
        entry.state = Some(UnitState::Held);
        entry.result = None;
        report.units.insert(name, entry);
    }
    plan_leaving(
        session,
        report,
        &request.entry_project_dir,
        &mut life,
        &observed,
        options,
        source.as_ref(),
    );
}

/// Report what the leaving waves would do: destroy plans of the teardowns
/// that would start, the retentions and releases, and why the others wait.
#[allow(clippy::too_many_lines)] // Readiness, the plan, then one entry per ready, pending, and retained unit.
fn plan_leaving(
    session: &Session<'_, '_>,
    report: &mut EnvironmentRunReport,
    project_dir: &str,
    life: &mut teardown::Life,
    observed: &BTreeMap<UnitName, ObservedUnit>,
    options: TeardownOptions<'_>,
    source: Option<&CommitId>,
) {
    let context = session.context;
    let environment = session.environment();
    let mut readiness = BTreeMap::new();
    let mut readiness_state = BTreeMap::new();
    for (name, unit) in &life.desired {
        let teardown_unit = matches!(&unit.lifecycle, Lifecycle::Deleting { deletion }
            if deletion.intent == nyl_core::orchestration::state::DeletionIntent::Teardown);
        if !teardown_unit {
            continue;
        }
        if let Some((blocking, _)) = teardown::readiness(context.drivers, environment, name, unit) {
            readiness_state.insert(
                name.clone(),
                if blocking.is_empty() {
                    crate::run_report::TeardownReadiness::Ready
                } else {
                    crate::run_report::TeardownReadiness::Incomplete {
                        reasons: blocking.iter().map(|finding| finding.message.clone()).collect(),
                    }
                },
            );
            if !blocking.is_empty() {
                readiness.insert(name.clone(), blocking);
            }
        }
    }
    let attempted = BTreeSet::new();
    let waiting = BTreeSet::new();
    let plan = crate::rules::teardown::plan(&crate::rules::teardown::TeardownInputs {
        environment,
        desired: &life.desired,
        observed,
        drivers: context.drivers,
        options,
        attempted: &attempted,
        waiting: &waiting,
        readiness: &readiness,
        drops: &life.releases,
    });
    for name in &plan.ready {
        let unit = &life.desired[name];
        let mut entry = UnitReport::new(unit.unit.kind.clone());
        entry.state = Some(unit_state(unit, same_incarnation(unit, observed.get(name)), false));
        entry.teardown_readiness = readiness_state.get(name).cloned();
        let retains = life.releases.contains(name)
            || matches!(&unit.lifecycle, Lifecycle::Deleting { deletion }
                if deletion.intent == nyl_core::orchestration::state::DeletionIntent::Retain);
        if retains {
            let retained = !life.releases.contains(name)
                && crate::rules::desired::declares_retain(unit)
                && !matches!(&unit.lifecycle, Lifecycle::Deleting { deletion }
                    if deletion.reason == nyl_core::orchestration::state::DeletionReason::KindChange);
            entry.summary = Some(if retained {
                "would be retained: its resources stay, and its state becomes a tombstone".to_owned()
            } else {
                "would be released: its resources stay in place, and its state is removed".to_owned()
            });
        } else if let Some(commit) = source.or(unit.source_revision.as_ref()) {
            plan_unit(
                session,
                project_dir,
                unit,
                commit,
                same_incarnation(unit, observed.get(name)),
                &mut entry,
            );
        }
        if let Some(tombstones) = plan.overrides.get(name) {
            entry.warnings.push(Finding::new(
                Code::TeardownHasDependents,
                format!(
                    "unit {name} is cited by the retained tombstones {}; --allow-dependents tears it down anyway",
                    tombstones.iter().map(UnitName::as_str).collect::<Vec<_>>().join(", ")
                ),
                "check that the tombstones' resources no longer need it",
            ));
        }
        report.units.insert(name.clone(), entry);
    }
    for (name, pending) in &plan.pending {
        if matches!(
            pending,
            crate::rules::teardown::Pending::NotNamed | crate::rules::teardown::Pending::Held
        ) || report
            .units
            .get(name)
            .is_some_and(|entry| entry.state == Some(UnitState::Held))
        {
            continue;
        }
        let unit = &life.desired[name];
        let mut entry = UnitReport::new(unit.unit.kind.clone());
        entry.state = Some(unit_state(unit, same_incarnation(unit, observed.get(name)), false));
        entry.teardown_readiness = readiness_state.get(name).cloned();
        // What `--allow-teardown` would destroy.
        if *pending == crate::rules::teardown::Pending::NeedsAllowTeardown {
            if let Some(commit) = source.or(unit.source_revision.as_ref()) {
                plan_unit(
                    session,
                    project_dir,
                    unit,
                    commit,
                    same_incarnation(unit, observed.get(name)),
                    &mut entry,
                );
            }
        }
        entry
            .warnings
            .extend(crate::rules::teardown::finding(environment, name, pending));
        if let crate::rules::teardown::Pending::Readiness { findings } = pending {
            entry.warnings.extend(findings.iter().cloned());
        }
        report.units.insert(name.clone(), entry);
    }
    // Retained tombstones that stay.
    for (name, unit) in &life.desired {
        if matches!(unit.lifecycle, Lifecycle::Retained { .. }) && !report.units.contains_key(name) {
            let mut entry = UnitReport::new(unit.unit.kind.clone());
            entry.state = Some(UnitState::Retained);
            report.units.insert(name.clone(), entry);
        }
    }
}

/// Ask a unit's driver for its plan, in a worktree of its own: a ready
/// unit's changes, or a deleting unit's destroy plan.
fn plan_unit(
    session: &Session<'_, '_>,
    project_dir: &str,
    unit: &DesiredUnit,
    commit: &CommitId,
    observed: Option<&ObservedUnit>,
    entry: &mut UnitReport,
) {
    let context = session.context;
    let name = &unit.unit.name;
    let Some(driver) = context.drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
        return;
    };
    let directory = session.execution_directory(name);
    let temp_dir = directory.join("tmp");
    // Planning is sequential, so it always uses the first slot.
    let worktree = match session.slots.checkout(context.sources, 0, commit) {
        Ok(worktree) => worktree,
        Err(message) => {
            entry.plan_error = Some(message);
            return;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&temp_dir) {
        entry.plan_error = Some(format!("cannot create {}: {error}", temp_dir.display()));
        return;
    }
    let now = context.base.clock.now();
    let execution = ExecutionContext {
        unit: UnitAddress::new(session.environment().clone(), name.clone()),
        operation: DriverOperation::Plan,
        project_dir: project_path(&worktree, project_dir),
        worktree,
        temp_dir,
        env: admitted_env(context.process_env, &unit.spec),
        approved_digest: None,
        deadline: unit_timeout(unit).after(now),
        now,
        observed,
        tools: context.tools,
        commit_identity: context.commit_identity,
        cache_dir: session.context.cache_dir(),
        teardown_wait: None,
    };
    match driver.plan(&execution, unit) {
        Ok(Supported::Supported(plan)) => {
            entry.changes = Some(plan.changes);
            entry.summary = Some(plan.summary);
            entry.change_digest = plan.change_digest;
            entry
                .blocked_by
                .extend(plan.blocked_by.into_iter().map(|pointer| BlockedBy {
                    pointer,
                    unit: None,
                    reason: "needed-to-plan".to_owned(),
                }));
        }
        Ok(Supported::Unsupported) => {
            entry.summary = Some(format!("kind {} cannot plan; reconcile executes it", unit.unit.kind));
        }
        Err(error) => entry.plan_error = Some(error.to_string()),
    }
    let _ = std::fs::remove_dir_all(&directory);
}

// ---------------------------------------------------------------------------
// reconcile

/// How a run that held the lease ended.
enum Ending {
    /// Release the lease and delete the run ref.
    Release,
    /// Keep the run ref, marked finished, for the next run to import;
    /// release the lease when it is still this run's. `unrecorded` names
    /// the units that ran but whose checkpoint did not reach the run ref:
    /// the record keeps them as started, so the run taking over gives them
    /// an `uncertain` condition instead of losing their effects.
    KeepRun { release: bool, unrecorded: Vec<UnitName> },
    /// A fault killed the run: write nothing more.
    Killed,
    /// SIGINT or SIGTERM: keep the run ref, marked finished, once a
    /// checkpoint is on it (else delete it), and release the lease.
    Cancelled,
}

/// The mutable state of one reconcile holding the lease.
struct Reconcile<'s, 'r, 'a> {
    session: &'s Session<'r, 'a>,
    run: RunId,
    held: HeldLease,
    record: Run,
    report: EnvironmentRunReport,
    /// Whether a unit's checkpoint is on the run ref: from then on the run
    /// ref is kept whenever the run cannot finish, so executed units'
    /// results are not lost.
    checkpointed: bool,
    /// Whether the run's transition commit landed, or it ended without
    /// changes after its import: a run that took a broken lease over and
    /// ends otherwise restores the break when it releases.
    landed: bool,
    /// Units of the ownership set fenced by a deleting incarnation of their name.
    fenced: BTreeSet<UnitName>,
    /// Whether the run read signals sent to it: its signal ref goes when it ends.
    signalled: bool,
}

/// What a run read before it renders: the snapshot, the runs before it,
/// and the desired documents of units they created.
struct Begun {
    snapshot: EnvironmentSnapshot,
    coordination: nyl_core::orchestration::ports::Coordination,
    replaced: Option<nyl_core::orchestration::state::Lease>,
    adopted: BTreeMap<UnitName, DesiredUnit>,
    overlay: EnvironmentSnapshot,
}

/// The hold signals a run acted on.
#[derive(Debug, Default)]
pub(super) struct Signals {
    /// Signals already read, by unit and time.
    seen: BTreeSet<(UnitName, Timestamp)>,
    /// Units a signal held during the run.
    pub(super) held: BTreeSet<UnitName>,
}

/// Restore the receipts of returning tombstones whose execution key is
/// unchanged ([`adopt_tombstone`]); whether any receipt was restored.
fn adopt_tombstones(
    prepared: &Prepared,
    resolved: &Resolved,
    decided: &mut BTreeSet<UnitName>,
    observed: &mut BTreeMap<UnitName, ObservedUnit>,
) -> bool {
    let mut restored = false;
    for (name, tombstone) in &prepared.desired.tombstones {
        let Some(unit) = resolved.units.get(name) else {
            continue;
        };
        if decided.contains(name) || unit.execution_key.is_none() {
            continue;
        }
        decided.insert(name.clone());
        if let Some(receipt) = adopt_tombstone(unit, tombstone) {
            observed.insert(
                name.clone(),
                ObservedUnit {
                    api_version: StateV1,
                    kind: nyl_core::orchestration::state::ObservedUnitKind,
                    unit: unit.unit.clone(),
                    receipt: Some(receipt),
                    condition: None,
                    desired: Some(Box::new(unit.clone())),
                    waiting: None,
                },
            );
            restored = true;
        }
    }
    restored
}

/// What selection reads: the resolved units, and held units as stored,
/// whose receipts their consumers' citations name. A held unit is never
/// selected: its lifecycle is not active.
fn selection_view(resolved: &Resolved, held: &BTreeMap<UnitName, DesiredUnit>) -> BTreeMap<UnitName, DesiredUnit> {
    let mut view = resolved.units.clone();
    view.extend(held.iter().map(|(name, unit)| (name.clone(), unit.clone())));
    view
}

/// The documents a takeover imports checkpoints against: the resolved
/// units, and held units as stored (Q28); fenced names import against
/// their stored, deleting incarnation instead.
fn import_targets(
    resolved: &Resolved,
    held: &BTreeMap<UnitName, DesiredUnit>,
    fenced: &BTreeSet<UnitName>,
) -> BTreeMap<UnitName, DesiredUnit> {
    resolved
        .units
        .iter()
        .chain(held.iter().filter(|(_, unit)| unit.unit.uid.is_some()))
        .filter(|(name, _)| !fenced.contains(*name))
        .map(|(name, unit)| (name.clone(), unit.clone()))
        .collect()
}

/// `NYL-UNIT-KIND-CHANGE`: the old incarnation is deleting, the new kind waits.
fn kind_change_warning(
    environment: &EnvironmentName,
    name: &UnitName,
    stored: &DesiredUnit,
    new: &DesiredUnit,
) -> Finding {
    Finding::new(
        Code::UnitKindChange,
        format!(
            "unit {name} of {environment} changed its kind from {} to {}: its old incarnation is deleting, and the new \
             kind starts once it is removed",
            stored.unit.kind, new.unit.kind
        ),
        format!(
            "pass --allow-teardown to nyl reconcile to tear the old incarnation down, restore the old kind in source, \
             or drop the old incarnation without a teardown with `nyl state forget -e {environment} --unit {name}`"
        ),
    )
}

/// `NYL-RUN-STOPPED`: `--fail-fast` started no unit after `unit`.
fn stopped_warning(environment: &EnvironmentName, unit: &UnitName) -> Finding {
    Finding::new(
        Code::RunStopped,
        format!("--fail-fast stopped the run of {environment} after unit {unit}: it started no further unit"),
        format!("fix {unit}, then run again; without --fail-fast a run continues with every unit that does not depend on it"),
    )
}

/// A `--local` run never imports remote run refs: it refuses while the
/// remote holds leftovers a remote run must take over first.
fn check_local_leftovers(session: &Session<'_, '_>, run: &RunId) -> Result<(), Finding> {
    let base = session.base();
    let environment = session.environment();
    let coordination = base
        .store
        .coordination(&session.request.location, environment)
        .map_err(|error| store_error(environment, &error))?;
    crate::rules::local::check_leftovers(environment, run, &coordination, base.clock.now())
}

fn reconcile_environment(session: &Session<'_, '_>) -> EnvironmentRunReport {
    let context = session.context;
    let request = session.request;
    let base = context.base;
    let environment = session.environment();
    let mut report = EnvironmentRunReport::new(environment.clone());
    let run = base.ids.run_id();
    report.run = Some(run.clone());
    // A `--local` run checks for remote run refs it would have to take over
    // before it takes the lease, so a refusal leaves the remote lease (and
    // the break it may record) as it was; it checks again once it holds it.
    let checked = if request.local.is_some() {
        check_local_leftovers(session, &run)
    } else {
        Ok(())
    };
    let held = checked.and_then(|()| {
        lease::acquire(
            base,
            &request.location,
            environment,
            &run,
            request.mode.operation(),
            lease::run_takeover(request.local.as_ref()),
            base.settings.resolution_timeout,
            request.wait,
            &request.cancellation,
        )
    });
    let held = match held {
        Ok(held) => held,
        Err(finding) => {
            report.errors.push(finding);
            report.finish(request.mode.report_kind());
            return report;
        }
    };
    let record = Run {
        api_version: StateV1,
        kind: RunKind,
        run: run.clone(),
        runner: base.runner.clone(),
        operation: request.mode.operation(),
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: held.lease.deadline,
        units: Vec::new(),
        finished: false,
    };
    let mut state = Reconcile {
        session,
        run,
        held,
        record,
        report,
        checkpointed: false,
        landed: false,
        fenced: BTreeSet::new(),
        signalled: false,
    };
    session.lease_deadline.set(Some(state.held.lease.deadline));
    let ending = state.holding();
    state.end(&ending);
    let mut report = state.report;
    report.finish(request.mode.report_kind());
    report
}

impl<'r, 'a> Reconcile<'_, 'r, 'a> {
    fn context(&self) -> &'r RunContext<'a> {
        self.session.context
    }

    fn environment(&self) -> &'r EnvironmentName {
        &self.session.request.environment
    }

    fn location(&self) -> &'r StateLocation {
        &self.session.request.location
    }

    fn fault(&self, point: FaultPoint, unit: Option<&UnitName>) -> bool {
        self.context().base.faults.at(point, unit) == FaultAction::Kill
    }

    fn killed(&mut self, point: FaultPoint) -> Ending {
        self.report.errors.push(Finding::new(
            Code::DriverError,
            format!("the run was stopped at fault point {point}"),
            "the next run takes over once the lease expires or is broken",
        ));
        Ending::Killed
    }

    /// Write the run record; a store failure is reported and ends the run.
    fn write_run(&mut self, checkpoint: Option<&Checkpoint>) -> Result<(), StateError> {
        if !self.session.request.mode.writes_run_record() {
            return Ok(());
        }
        self.context()
            .base
            .store
            .write_run(self.location(), self.environment(), &self.record, checkpoint)
    }

    /// Update the lease and the run record for a phase, recording on the
    /// run ref the units of `created` before they start, in one store
    /// write. The `before-lease-update` fault point is consulted without a
    /// unit, then with each unit the phase starts, so a fault can name the
    /// phase that starts a unit's batch.
    fn phase(&mut self, units: Vec<UnitName>, deadline: Timestamp, created: &[DesiredUnit]) -> Result<(), Ending> {
        if self.fault(FaultPoint::BeforeLeaseUpdate, None)
            || units
                .iter()
                .any(|unit| self.fault(FaultPoint::BeforeLeaseUpdate, Some(unit)))
        {
            return Err(self.killed(FaultPoint::BeforeLeaseUpdate));
        }
        let mut lease = self.held.lease.clone();
        lease.units.clone_from(&units);
        lease.deadline = deadline;
        let mut record = self.record.clone();
        record.units = units;
        record.deadline = deadline;
        let mut version = self.held.version.clone();
        let store = self.context().base.store;
        let written = if self.session.request.mode.writes_run_record() {
            store.update_lease_and_run(
                self.location(),
                self.environment(),
                &lease,
                &mut version,
                &record,
                created,
            )
        } else {
            // A recover import renews only its lease.
            store
                .update_lease(self.location(), self.environment(), &lease, &version)
                .map(|new| version = new)
        };
        if version != self.held.version {
            self.held.version = version;
            self.held.lease = lease;
            self.session.lease_deadline.set(Some(deadline));
            self.record = record;
        }
        match written {
            Ok(()) => Ok(()),
            Err(StateError::LeaseLost) => Err(self.lost()),
            Err(error) => {
                let finding = store_error(self.environment(), &error);
                self.report.errors.push(finding);
                Err(self.failed())
            }
        }
    }

    /// How a run ends that lost its lease: `NYL-LEASE-LOST`, and it keeps
    /// its run ref for the run holding the lease to import.
    fn lost(&mut self) -> Ending {
        self.report.errors.push(lease::lost(self.environment()));
        Ending::KeepRun {
            release: false,
            unrecorded: Vec::new(),
        }
    }

    /// How a run that holds the lease ends on a store failure: it keeps its
    /// run ref once a checkpoint is on it, else it deletes it.
    fn failed(&self) -> Ending {
        if self.checkpointed {
            Ending::KeepRun {
                release: true,
                unrecorded: Vec::new(),
            }
        } else {
            Ending::Release
        }
    }

    /// Everything a reconcile or teardown does while it holds the lease.
    #[allow(clippy::too_many_lines)] // The contract's steps in order: snapshot, resolve, waves, leaving waves, report, commit.
    fn holding(&mut self) -> Ending {
        let session = self.session;
        let context = session.context;
        let request = session.request;
        let environment = session.environment().clone();
        let begun = match self.begin() {
            Ok(begun) => begun,
            Err(ending) => return ending,
        };
        if let RunMode::Teardown { target } = &request.mode {
            return self.teardown_holding(&begun, target);
        }
        let Begun {
            snapshot,
            coordination,
            replaced,
            adopted,
            overlay,
        } = &begun;
        let run = self.run.clone();
        let leftovers = takeover::leftovers(&run, replaced.as_ref(), coordination);
        let prepared = match session.prepare(overlay) {
            Ok(prepared) => prepared,
            Err(findings) => {
                self.report.errors.extend(findings);
                return Ending::Release;
            }
        };
        let source_commit = prepared.source_commit.clone();
        self.report.source_commit = Some(source_commit.clone());
        self.record.source_commit = Some(source_commit.clone());
        self.report.leaving.clone_from(&prepared.desired.leaving);
        if !self.report.leaving.is_empty() {
            let warning = leaving_warning(&environment, &self.report.leaving);
            self.report.warnings.push(warning);
        }
        self.fenced.clone_from(&prepared.desired.fenced);
        // `--unit` does no lifecycle work (Q5), and a recover import writes
        // only what it imports (Q56).
        let lifecycle_work = request.mode.writes_lifecycle() && request.units.is_none();

        let mut observed = snapshot.observed.clone();
        let mut artifacts = snapshot.artifacts.clone();
        let mut life = teardown::Life::new(&overlay.desired);
        if lifecycle_work {
            for (name, write) in &prepared.desired.lifecycle {
                match (&write.desired, prepared.desired.units.contains_key(name)) {
                    // A returning unit's document is the one resolution writes.
                    (Some(desired), true) if !prepared.desired.fenced.contains(name) => {
                        life.desired.insert(name.clone(), desired.clone());
                    }
                    _ => life.apply(name, write.clone(), &mut observed, &mut artifacts),
                }
                if write.outcome == LifecycleOutcome::KindChange {
                    let stored = &overlay.desired[name];
                    let new = &prepared.desired.units[name];
                    self.report
                        .warnings
                        .push(kind_change_warning(&environment, name, stored, new));
                }
            }
            // Tombstones of another kind are dropped once nothing deleting depends on them.
            for name in &prepared.desired.fenced {
                if matches!(
                    overlay.desired.get(name).map(|stored| &stored.lifecycle),
                    Some(Lifecycle::Retained { .. })
                ) {
                    life.releases.insert(name.clone());
                }
            }
            let ownership: BTreeSet<UnitName> = prepared
                .desired
                .units
                .keys()
                .chain(&prepared.desired.fenced)
                .cloned()
                .collect();
            life.carry_decommission_holds(&ownership);
        }
        let mut held_view: BTreeMap<UnitName, DesiredUnit> = prepared
            .desired
            .held
            .iter()
            .map(|(name, held)| (name.clone(), held.stored.clone()))
            .collect();
        let mut produced = BTreeSet::new();
        let mut attempted = BTreeSet::new();
        let mut executions: BTreeMap<UnitName, Execution> = BTreeMap::new();
        let mut created: BTreeSet<UnitName> = BTreeSet::new();
        let mut taking = Takeover::new(&leftovers, overlay, &prepared.desired.units);
        let runs: Vec<_> = leftovers.runs.iter().map(|state| (*state).clone()).collect();
        let mut adoptions: BTreeSet<UnitName> = BTreeSet::new();
        let reapply = request.reapply_units();
        let mut first = true;
        let (resolved, skipped) = loop {
            if request.cancellation.is_cancelled() {
                return self.cancelled();
            }
            // Resolution and its fingerprints run under a lease renewed for
            // the resolution timeout, never one sized for an earlier phase.
            let deadline = lease::deadline(
                context.base,
                context.base.clock.now(),
                context.base.settings.resolution_timeout,
            );
            if let Err(ending) = self.phase(Vec::new(), deadline, &[]) {
                return ending;
            }
            let resolved = session.resolve(
                &prepared,
                &Evidence {
                    observed: &observed,
                    artifacts: &artifacts,
                    observed_commit: snapshot.tips.observed.as_ref(),
                    run: Some(&self.run),
                    produced_in_run: &produced,
                    previous: &snapshot.desired,
                },
                &held_view,
            );
            if first && !resolved.errors.is_empty() {
                // Resolution errors before anything ran stop the run, as configuration errors do.
                self.report.errors.extend(resolved.errors.values().flatten().cloned());
                return Ending::Release;
            }
            first = false;
            // A returning tombstone adopts its receipt while its execution
            // key is unchanged (Q3); otherwise it runs as an ordinary change.
            if adopt_tombstones(&prepared, &resolved, &mut adoptions, &mut observed) {
                continue;
            }
            if taking.active {
                let import_targets = import_targets(&resolved, &held_view, &prepared.desired.fenced);
                let import = takeover::import(&ImportInputs {
                    leftovers: &leftovers,
                    environment: &environment,
                    now: context.base.clock.now(),
                    resolved: &import_targets,
                    adopted: &taking.unresolved,
                    snapshot: &snapshot.observed,
                    observed: &observed,
                    handled: &taking.handled,
                });
                if taking.apply(import, &mut observed, &mut artifacts, &mut produced, &mut self.report) {
                    // Imported receipts may change downstream keys: resolve again.
                    continue;
                }
            }
            if !request.mode.executes() || request.lifecycle.leaving || life.stopped.is_some() {
                // A recover import resolves and imports; `--leaving` acts
                // only on units outside the ownership set.
                break (resolved, BTreeMap::new());
            }
            // Holds sent to this run take effect before it starts a unit (Q28).
            if self.apply_hold_signals(&mut life, &runs, &mut observed, &mut artifacts, Some(&mut held_view)) {
                continue;
            }
            let selectable = selection_view(&resolved, &held_view);
            let candidates = Candidates {
                desired: &selectable,
                observed: &observed,
                drivers: context.drivers,
                named: request.units.as_ref(),
                attempted: &attempted,
                local: request
                    .local
                    .as_ref()
                    .and_then(|local| local_approve(&local.approve, context.commit_identity)),
                observations: &snapshot.observations,
                drift_repair: drift::drift_repair(&prepared.rendered.environment),
                reapply: &reapply,
            };
            let (ready, skipped) = select(&candidates);
            for (name, repair) in drift::repairs(&candidates, &ready) {
                let list = match repair {
                    drift::Repair::Drift => &mut self.report.drift_repaired,
                    drift::Repair::Reapply => &mut self.report.reapplied,
                };
                if !list.contains(&name) {
                    list.push(name);
                }
            }
            if ready.is_empty() {
                break (resolved, skipped);
            }
            for batch in batches(&ready, request.concurrency) {
                if request.cancellation.is_cancelled() {
                    return self.cancelled();
                }
                if life.stopped.is_some() {
                    break;
                }
                // A hold that arrived since the wave began keeps its unit from starting.
                if self.apply_hold_signals(&mut life, &runs, &mut observed, &mut artifacts, Some(&mut held_view)) {
                    break;
                }
                let now = context.base.clock.now();
                let longest = longest_timeout(batch.iter().map(|unit| &resolved.units[unit]));
                // Units whose uid this run generated go on the run ref before
                // they start, so a run taking over adopts them.
                let new: Vec<DesiredUnit> = batch
                    .iter()
                    .map(|name| &resolved.units[name])
                    .filter(|unit| {
                        !created.contains(&unit.unit.name)
                            && !adopted.contains_key(&unit.unit.name)
                            && snapshot.desired.get(&unit.unit.name).map(|stored| &stored.unit.uid)
                                != Some(&unit.unit.uid)
                    })
                    .cloned()
                    .collect();
                if let Err(ending) = self.phase(batch.clone(), lease::deadline(context.base, now, longest), &new) {
                    return ending;
                }
                created.extend(new.into_iter().map(|unit| unit.unit.name));
                let approvals = local_approvals(session, &resolved, &batch);
                let results = execute_batch(
                    session, &prepared, &resolved, &observed, &artifacts, &batch, &self.run, snapshot, &approvals,
                );
                let mut results = results.into_iter();
                while let Some(execution) = results.next() {
                    let name = execution.name.clone();
                    attempted.insert(name.clone());
                    if execution.killed {
                        return self.killed(FaultPoint::AfterEffect);
                    }
                    if let Some(checkpoint) = &execution.checkpoint {
                        if self.fault(FaultPoint::BeforeCheckpoint, Some(&name)) {
                            return self.killed(FaultPoint::BeforeCheckpoint);
                        }
                        if let Some(set) = &execution.artifacts {
                            if set.is_empty() {
                                artifacts.remove(&name);
                            } else {
                                artifacts.insert(name.clone(), set.clone());
                            }
                            produced.insert(name.clone());
                        }
                        observed.insert(name.clone(), checkpoint.observed.clone());
                        self.checkpointed = true;
                        if let Err(error) = self.write_run(Some(checkpoint)) {
                            let finding = store_error(&environment, &error);
                            self.report.errors.push(finding);
                            // This unit and the later ones of the batch ran,
                            // but no checkpoint of theirs is on the run ref.
                            let unrecorded = std::iter::once(name)
                                .chain(
                                    results
                                        .filter(|execution| execution.checkpoint.is_some())
                                        .map(|execution| execution.name),
                                )
                                .collect();
                            return Ending::KeepRun {
                                release: true,
                                unrecorded,
                            };
                        }
                    }
                    if matches!(execution.result, UnitResult::Recovered { .. }) {
                        taking.recovered.insert(name.clone());
                    }
                    if request.fail_fast
                        && life.stopped.is_none()
                        && matches!(
                            execution.result,
                            UnitResult::Failed { .. } | UnitResult::Uncertain { .. } | UnitResult::AwaitingApproval
                        )
                    {
                        life.stopped = Some(name.clone());
                    }
                    self.report.errors.extend(execution.errors.iter().cloned());
                    executions.insert(name, execution);
                }
                if request.cancellation.is_cancelled() {
                    // The units that finished are checkpointed; units still
                    // running at the signal are uncertain.
                    self.report_cancelled_units(&resolved.units, &executions);
                    return self.cancelled();
                }
            }
        };
        self.report.errors.extend(resolved.errors.values().flatten().cloned());
        if taking.active {
            // Units that never got an execution key import their
            // checkpoints without a key comparison, so none stays behind.
            let mut keyed = import_targets(&resolved, &held_view, &prepared.desired.fenced);
            let mut unkeyed = taking.unresolved.clone();
            keyed.retain(|name, unit| {
                if unit.execution_key.is_some() {
                    return true;
                }
                unkeyed.insert(name.clone(), unit.clone());
                false
            });
            let import = takeover::import(&ImportInputs {
                leftovers: &leftovers,
                environment: &environment,
                now: context.base.clock.now(),
                resolved: &keyed,
                adopted: &unkeyed,
                snapshot: &snapshot.observed,
                observed: &observed,
                handled: &taking.handled,
            });
            taking.apply(import, &mut observed, &mut artifacts, &mut produced, &mut self.report);
        }
        self.report.imported = taking.imported.iter().cloned().collect();
        self.report.recovered = taking.recovered.iter().cloned().collect();
        // `nyl recover --unit` after importing a broken run: the action
        // applies to the imported state, in the same commit.
        let mut recover_entry = None;
        if let RunMode::Recover { unit: Some(recover) } = &request.mode {
            let runs: Vec<_> = leftovers
                .runs
                .iter()
                .chain(leftovers.evidence.iter())
                .map(|state| (*state).clone())
                .collect();
            let desired = resolved
                .units
                .get(&recover.unit)
                .or_else(|| taking.unresolved.get(&recover.unit));
            match super::recover::after_import(&environment, recover, desired, &mut observed, &mut artifacts, &runs) {
                Ok(entry) => recover_entry = Some((recover.unit.clone(), entry)),
                Err(finding) => {
                    self.report.errors.push(finding);
                    return Ending::Release;
                }
            }
        }

        // The leaving waves: teardowns, retentions, releases, and drops,
        // after the units of the ownership set (Q5: not with `--unit`).
        if lifecycle_work && request.mode == RunMode::Reconcile && life.stopped.is_none() {
            let inputs = teardown::LeaveInputs {
                project_dir: &prepared.rendered.project_dir,
                source: None,
                runs: &runs,
                options: TeardownOptions {
                    allow_teardown: request.lifecycle.allow_teardown,
                    allow_dependents: request.lifecycle.allow_dependents,
                    allow_incomplete: request.lifecycle.allow_incomplete,
                    named: None,
                    explicit: false,
                },
            };
            if let Err(ending) = self.leave(&mut life, &inputs, &mut observed, &mut artifacts) {
                return ending;
            }
        }
        // A hold that arrived while the last units ran is still written by this run.
        if request.mode.executes() {
            self.apply_hold_signals(&mut life, &runs, &mut observed, &mut artifacts, None);
        }
        // Held units record what source would change, only when that changes.
        if lifecycle_work {
            for (name, held) in &prepared.desired.held {
                if life.signals.held.contains(name) {
                    continue;
                }
                let pending = hold_pending(held, &source_commit, resolved.shadow.get(name));
                let Lifecycle::Held { hold, deletion } = &held.stored.lifecycle else {
                    continue;
                };
                if same_pending(hold.pending.as_ref(), pending.as_ref()) {
                    continue;
                }
                let mut hold = hold.clone();
                hold.pending = pending;
                let document = DesiredUnit {
                    lifecycle: Lifecycle::Held {
                        hold,
                        deletion: deletion.clone(),
                    },
                    ..held.stored.clone()
                };
                life.apply(
                    name,
                    LifecycleWrite {
                        desired: Some(document),
                        remove_observed: false,
                        outcome: LifecycleOutcome::PendingTeardown,
                    },
                    &mut observed,
                    &mut artifacts,
                );
            }
        }

        // The report: one result per unit, only the named ones with --unit.
        let named = |name: &UnitName| request.units.as_ref().is_none_or(|named| named.contains(name));
        let not_started = if life.stopped.is_some() {
            let selectable = selection_view(&resolved, &held_view);
            let (ready, _) = select(&Candidates {
                desired: &selectable,
                observed: &observed,
                drivers: context.drivers,
                named: request.units.as_ref(),
                attempted: &attempted,
                local: request
                    .local
                    .as_ref()
                    .and_then(|local| local_approve(&local.approve, context.commit_identity)),
                observations: &snapshot.observations,
                drift_repair: drift::drift_repair(&prepared.rendered.environment),
                reapply: &reapply,
            });
            ready.into_iter().collect()
        } else {
            BTreeSet::new()
        };
        for (name, unit) in &resolved.units {
            // `--leaving` reports only the units outside the ownership set.
            if !named(name) || request.lifecycle.leaving {
                continue;
            }
            let current = observed.get(name);
            let mut entry = UnitReport::new(unit.unit.kind.clone());
            entry.receipt = Some(receipt_state(Some(unit), current));
            entry.last_run = current.and_then(ObservedUnit::last_run).cloned();
            entry.blocked_by = blocked_by(unit);
            if let Some(execution) = executions.get(name) {
                entry.result = Some(execution.result.clone());
                entry.message.clone_from(&execution.message);
                entry.warnings.clone_from(&execution.warnings);
            } else if let Some(result) = imported_result(&taking, unit, current) {
                entry.result = Some(result);
            } else if let (Some(stopped), true) = (&life.stopped, not_started.contains(name)) {
                entry.result = Some(UnitResult::Blocked {
                    reason: format!("--fail-fast stopped the run after {stopped}"),
                });
            } else {
                entry.result = Some(unexecuted_result(
                    unit,
                    same_incarnation(unit, current),
                    skipped.get(name),
                    resolved.errors.get(name),
                ));
                entry.message = condition_message(unit, current);
                if skipped.get(name) == Some(&Skipped::LocalApproval) {
                    entry.message = Some(format!(
                        "a --local run executes {name} only with --approve {name}: its recovery policy is not converge"
                    ));
                }
            }
            self.report.units.insert(name.clone(), entry);
        }
        for (name, held) in &prepared.desired.held {
            if named(name)
                && !life.signals.held.contains(name)
                && (!request.lifecycle.leaving || held.rendered.is_none())
            {
                let document = life.desired.get(name).unwrap_or(&held.stored);
                let entry = teardown::held_report(&environment, name, document, false);
                self.report.units.insert(name.clone(), entry);
            }
        }
        if request.units.is_none() {
            // Retained tombstones stay retained, run after run.
            for (name, stored) in &life.desired {
                if matches!(stored.lifecycle, Lifecycle::Retained { .. })
                    && !self.report.units.contains_key(name)
                    && !life.results.contains_key(name)
                {
                    let mut entry = UnitReport::new(stored.unit.kind.clone());
                    entry.result = Some(UnitResult::Retained);
                    self.report.units.insert(name.clone(), entry);
                }
            }
        }
        for (name, entry) in std::mem::take(&mut life.results) {
            if named(&name) {
                self.report.units.insert(name, entry);
            }
        }
        if let Some(stopped) = &life.stopped {
            self.report.warnings.push(stopped_warning(&environment, stopped));
        }

        // The transition: only the documents that differ from the snapshot.
        let mut changes = TransitionChanges::default();
        if snapshot.environment.as_ref() != Some(&prepared.desired.environment) {
            changes.environment = Some(prepared.desired.environment.clone());
        }
        // Without lifecycle work, adopted units no longer selected keep the
        // desired document of the incarnation a dead run created, so its
        // resources have state; with it, they leave like any other unit.
        let unselected = adopted
            .iter()
            .filter(|(name, _)| !lifecycle_work && !resolved.units.contains_key(*name));
        for (name, unit) in resolved.units.iter().chain(unselected) {
            if prepared.desired.fenced.contains(name) || life.signals.held.contains(name) {
                continue;
            }
            if snapshot.desired.get(name) != Some(unit) {
                changes.desired.insert(name.clone(), Some(unit.clone()));
            }
        }
        for (name, write) in &life.writes {
            if snapshot.desired.get(name) == write.as_ref() {
                changes.desired.remove(name);
            } else {
                changes.desired.insert(name.clone(), write.clone());
            }
        }
        Self::observed_changes(snapshot, &observed, &artifacts, &life, &mut changes);
        if request.cancellation.is_cancelled() {
            return self.cancelled();
        }
        let mut summary = self.report.transition_summary(request.mode.operation());
        if let RunMode::Recover { .. } = &request.mode {
            // A recover executes nothing: its summary lists what it imported
            // and recovered, and its headline gives their counts, then the
            // `--unit` action (Q44).
            let listed: BTreeSet<UnitName> = summary.imported.iter().chain(&summary.recovered).cloned().collect();
            summary.units.retain(|name, _| listed.contains(name));
            super::recover::summarize(&mut summary, recover_entry);
        } else if let Some((unit, entry)) = recover_entry {
            summary.units.insert(unit, entry);
        }
        if !self.report.reapplied.is_empty() {
            summary.reapplied = Some(crate::transition::Reapplied {
                request: nyl_core::orchestration::state::Request {
                    by: context.base.requested_by.clone(),
                    at: self.held.lease.taken_at,
                    reason: request.reason.clone(),
                },
                units: self.report.reapplied.clone(),
            });
        }
        for (name, execution) in &executions {
            let approval = execution
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.observed.receipt.as_ref())
                .filter(|receipt| receipt.run == self.run)
                .and_then(|receipt| receipt.approval.as_ref());
            if let (Some(approval), Some(Value::Object(entry))) = (approval, summary.units.get_mut(name)) {
                entry.insert("approval".to_owned(), json!(approval));
            }
        }
        life.details.insert_into(&mut summary.details);
        self.commit(snapshot, &leftovers, &observed, changes, summary, Some(source_commit))
    }

    /// A teardown run (`nyl teardown`): it acts on the stored desired
    /// documents and renders nothing (Q4).
    #[allow(clippy::too_many_lines)] // The steps in order: import, requests, leaving waves, report, commit.
    fn teardown_holding(&mut self, begun: &Begun, target: &TeardownTarget) -> Ending {
        let session = self.session;
        let context = session.context;
        let request = session.request;
        let environment = session.environment().clone();
        let Begun {
            snapshot,
            coordination,
            replaced,
            adopted: _,
            overlay,
        } = begun;
        let run = self.run.clone();
        let leftovers = takeover::leftovers(&run, replaced.as_ref(), coordination);
        let mut observed = snapshot.observed.clone();
        let mut artifacts = snapshot.artifacts.clone();
        // `--source`: every teardown runs with that revision's files.
        let source = match &request.source_override {
            Some(revision) => match session.resolve_revision(revision) {
                Ok(commit) => Some(commit),
                Err(finding) => {
                    self.report.errors.push(finding);
                    return Ending::Release;
                }
            },
            None => None,
        };
        // A run taking over imports against the stored documents.
        let stored: BTreeMap<UnitName, DesiredUnit> = overlay.desired.clone();
        let mut taking = Takeover::new(&leftovers, overlay, &BTreeMap::new());
        if taking.active {
            let mut produced = BTreeSet::new();
            loop {
                let mut keyed = stored.clone();
                let mut unkeyed = BTreeMap::new();
                keyed.retain(|name, unit| {
                    if unit.execution_key.is_some() && unit.unit.uid.is_some() {
                        return true;
                    }
                    unkeyed.insert(name.clone(), unit.clone());
                    false
                });
                let import = takeover::import(&ImportInputs {
                    leftovers: &leftovers,
                    environment: &environment,
                    now: context.base.clock.now(),
                    resolved: &keyed,
                    adopted: &unkeyed,
                    snapshot: &snapshot.observed,
                    observed: &observed,
                    handled: &taking.handled,
                });
                if !taking.apply(import, &mut observed, &mut artifacts, &mut produced, &mut self.report) {
                    break;
                }
            }
        }
        self.report.imported = taking.imported.iter().cloned().collect();
        self.report.recovered = taking.recovered.iter().cloned().collect();

        let ownership: BTreeSet<UnitName> = snapshot
            .environment
            .as_ref()
            .map(|record| record.ownership.keys().cloned().collect())
            .unwrap_or_default();
        let mut life = teardown::Life::new(&stored);
        life.carry_decommission_holds(&ownership);
        if let Err(findings) = self.request_teardowns(&mut life, target, &ownership, &mut observed, &mut artifacts) {
            self.report.errors.extend(findings);
            return Ending::Release;
        }
        let named: Option<BTreeSet<UnitName>> = match target {
            TeardownTarget::Units(names) => Some(names.clone()),
            TeardownTarget::All => None,
        };
        let runs: Vec<_> = leftovers.runs.iter().map(|state| (*state).clone()).collect();
        let inputs = teardown::LeaveInputs {
            project_dir: &request.entry_project_dir,
            source: source.as_ref(),
            runs: &runs,
            options: TeardownOptions {
                allow_teardown: true,
                allow_dependents: request.lifecycle.allow_dependents,
                allow_incomplete: request.lifecycle.allow_incomplete,
                named: named.as_ref(),
                explicit: true,
            },
        };
        if let Err(ending) = self.leave(&mut life, &inputs, &mut observed, &mut artifacts) {
            return ending;
        }
        // `--all` leaves an unfinished decommission deleting (its hold is
        // carried to the run that completes it); `--hold` holds it now.
        if !matches!(target, TeardownTarget::All) {
            self.hold_unfinished(&mut life, &mut observed, &mut artifacts);
        }
        let in_scope = |name: &UnitName| named.as_ref().is_none_or(|named| named.contains(name));
        for (name, entry) in std::mem::take(&mut life.results) {
            if in_scope(&name) {
                self.report.units.insert(name, entry);
            }
        }
        if let Some(stopped) = &life.stopped {
            self.report.warnings.push(stopped_warning(&environment, stopped));
        }

        let mut changes = TransitionChanges::default();
        for (name, write) in &life.writes {
            if snapshot.desired.get(name) != write.as_ref() {
                changes.desired.insert(name.clone(), write.clone());
            }
        }
        Self::observed_changes(snapshot, &observed, &artifacts, &life, &mut changes);
        if request.cancellation.is_cancelled() {
            return self.cancelled();
        }
        let mut summary = self.report.transition_summary(request.mode.operation());
        life.details.insert_into(&mut summary.details);
        self.commit(snapshot, &leftovers, &observed, changes, summary, None)
    }

    /// Take the snapshot, check it, and read the runs before this one.
    fn begin(&mut self) -> Result<Begun, Ending> {
        let session = self.session;
        let context = session.context;
        let request = session.request;
        let environment = session.environment().clone();
        let snapshot = match context.base.store.snapshot(&request.location) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.report.errors.push(store_error(&environment, &error));
                return Err(Ending::Release);
            }
        };
        if let Err(finding) = super::local::check_record(
            context.base.store,
            snapshot.record.as_ref(),
            &environment,
            &request.location,
        ) {
            self.report.errors.push(finding);
            return Err(Ending::Release);
        }
        if request.local.is_none() {
            if let Err(finding) = super::local::check_remote_tips(context.base.store, &request.location, &environment) {
                self.report.errors.push(finding);
                return Err(Ending::Release);
            }
        } else if let Err(finding) = check_local_leftovers(session, &self.run) {
            // A `--local` run never imports remote run refs, so it waits
            // until remote state took over dead ones.
            self.report.errors.push(finding);
            return Err(Ending::Release);
        }
        // The runs before this one, read after taking the lease. A
        // `--local` run under the remote lease never takes the remote
        // lease's units as its own leftovers (and vice versa).
        let coordination = match context.base.store.coordination(&request.location, &environment) {
            Ok(coordination) => coordination,
            Err(error) => {
                self.report.errors.push(store_error(&environment, &error));
                return Err(Ending::Release);
            }
        };
        let replaced = match &request.local {
            Some(local) if local.lease == super::local::LeaseScope::Remote => None,
            _ => self.held.replaced.clone(),
        };
        let mut adopted = {
            let leftovers = takeover::leftovers(&self.run, replaced.as_ref(), &coordination);
            takeover::adoptions(&leftovers)
        };
        adopted.retain(|name, desired| {
            snapshot.desired.get(name).map(|stored| &stored.unit.uid) != Some(&desired.unit.uid)
        });
        let mut overlay = snapshot.clone();
        overlay.desired.extend(adopted.clone());

        self.record.read_desired.clone_from(&snapshot.tips.desired);
        self.record.read_observed.clone_from(&snapshot.tips.observed);
        if let Err(error) = self.write_run(None) {
            self.report.errors.push(store_error(&environment, &error));
            return Err(Ending::Release);
        }
        Ok(Begun {
            snapshot,
            coordination,
            replaced,
            adopted,
            overlay,
        })
    }

    /// The observed side of the transition: changed and removed observed
    /// documents, artifact sets, and the observations and attestations of
    /// units whose receipt was replaced or whose state was removed.
    fn observed_changes(
        snapshot: &EnvironmentSnapshot,
        observed: &BTreeMap<UnitName, ObservedUnit>,
        artifacts: &BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
        life: &teardown::Life,
        changes: &mut TransitionChanges,
    ) {
        for (name, document) in observed {
            if snapshot.observed.get(name) != Some(document) {
                changes.observed.insert(name.clone(), Some(document.clone()));
            }
        }
        for name in snapshot.observed.keys().filter(|name| !observed.contains_key(*name)) {
            changes.observed.insert(name.clone(), None);
        }
        let names: BTreeSet<&UnitName> = artifacts.keys().chain(snapshot.artifacts.keys()).collect();
        for name in names {
            let after = artifacts.get(name);
            if snapshot.artifacts.get(name) != after {
                changes.artifacts.insert(name.clone(), after.cloned());
            }
        }
        // A new receipt replaces the one a drift observation was about.
        changes
            .observations
            .extend(drift::observation_changes(snapshot, observed));
        for name in &life.cleared {
            if snapshot.observations.contains_key(name) {
                changes.observations.insert(name.clone(), None);
            }
            if snapshot.attestations.contains_key(name) {
                changes.attestations.insert(name.clone(), None);
            }
        }
    }

    /// Write the transition commit, or release without one when nothing
    /// changed, then consume the leftover runs.
    fn commit(
        &mut self,
        snapshot: &EnvironmentSnapshot,
        leftovers: &Leftovers<'_>,
        observed: &BTreeMap<UnitName, ObservedUnit>,
        mut changes: TransitionChanges,
        mut summary: crate::transition::TransitionSummary,
        source_commit: Option<CommitId>,
    ) -> Ending {
        let context = self.context();
        let request = self.session.request;
        let environment = self.environment().clone();
        if changes.is_empty() && lease::records_break(&self.held) {
            // The takeover of a broken lease is recorded once, even when
            // its import changed nothing ("Runs and leases" item 8).
            changes.commit_unchanged = true;
        }
        if changes.is_empty() {
            self.consume(leftovers, observed);
            self.landed = true;
            return Ending::Release;
        }
        if self.fault(FaultPoint::BeforePush, None) {
            return self.killed(FaultPoint::BeforePush);
        }
        if let Some(revision) = &request.source_override {
            summary.details.insert("sourceOverride".to_owned(), json!(revision));
        }
        let trailers = Trailers {
            source_commit,
            local: request.local.is_some(),
            requested_by: request.requested().then(|| context.base.requested_by.clone()),
            reason: request.requested().then(|| request.reason.clone()).flatten(),
            ..super::trailers(context.base, &self.held, &snapshot.tips)
        };
        let committed = super::commit_transition(
            context.base,
            &request.location,
            &mut self.held,
            &snapshot.tips,
            summary,
            changes,
            &trailers,
        );
        match committed {
            Ok(commits) => {
                self.report.commits.extend(commits);
                self.landed = true;
                self.consume(leftovers, observed);
                Ending::Release
            }
            Err(StateError::LeaseLost) => self.lost(),
            Err(error) => {
                self.report.errors.push(store_error(&environment, &error));
                self.failed()
            }
        }
    }

    /// Apply the holds sent to this run since it last looked (Q28): each
    /// held unit's document is decided by [`crate::rules::hold::hold`] and
    /// written in this run's transition commit; the run starts no held unit.
    /// Whether any hold was applied.
    pub(super) fn apply_hold_signals(
        &mut self,
        life: &mut teardown::Life,
        runs: &[nyl_core::orchestration::ports::RunState],
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
        mut held_view: Option<&mut BTreeMap<UnitName, DesiredUnit>>,
    ) -> bool {
        let context = self.context();
        let environment = self.environment().clone();
        // Oldest first; a store failure reads as no signal.
        let received = crate::shell::signal::received(context.base, self.location(), &environment, &self.run);
        if !received.is_empty() {
            self.signalled = true;
        }
        let mut applied = false;
        for signal in received {
            if signal.action != nyl_core::orchestration::state::SignalAction::Hold
                || !life.signals.seen.insert((signal.unit.clone(), signal.at))
                || life.signals.held.contains(&signal.unit)
            {
                continue;
            }
            let name = signal.unit.clone();
            let hold = nyl_core::orchestration::state::Hold {
                since: signal.at,
                by: signal.by.clone(),
                reason: signal.reason.clone(),
                pending: None,
            };
            let stored = life.desired.get(&name);
            match crate::rules::hold::hold(&environment, &name, stored, observed.get(&name), runs, hold) {
                Ok(crate::rules::hold::HoldWrite::Unchanged) => {}
                Ok(crate::rules::hold::HoldWrite::Desired(document)) => {
                    let document = *document;
                    life.apply(
                        &name,
                        LifecycleWrite {
                            desired: Some(document.clone()),
                            remove_observed: false,
                            outcome: LifecycleOutcome::PendingTeardown,
                        },
                        observed,
                        artifacts,
                    );
                    let entry = teardown::held_report(&environment, &name, &document, false);
                    life.results.insert(name.clone(), entry);
                    life.details.held.push(name.clone());
                    if let Some(view) = held_view.as_deref_mut() {
                        view.insert(name.clone(), document);
                    }
                    life.signals.held.insert(name);
                    applied = true;
                }
                Err(finding) => self.report.warnings.push(finding),
            }
        }
        applied
    }

    /// End a cancelled run: `NYL-RUN-CANCELLED`, exit 4.
    fn cancelled(&mut self) -> Ending {
        let signal = self
            .session
            .request
            .cancellation
            .signal()
            .map_or_else(String::new, |signal| format!(" by signal {signal}"));
        self.report.errors.push(Finding::new(
            Code::RunCancelled,
            format!(
                "the run was cancelled{signal}; it checkpointed the units that finished, left the units still running \
                 uncertain, and wrote no transition commit"
            ),
            "run nyl reconcile again: it imports this run's checkpoints, and units left uncertain run again when \
             their recovery policy converges; avoid cancelling runs that apply changes",
        ));
        Ending::Cancelled
    }

    /// The units a cancelled run executed, in the report.
    fn report_cancelled_units(
        &mut self,
        units: &BTreeMap<UnitName, DesiredUnit>,
        executions: &BTreeMap<UnitName, Execution>,
    ) {
        for (name, execution) in executions {
            let kind = units.get(name).map(|unit| unit.unit.kind.clone()).unwrap_or_default();
            let mut entry = UnitReport::new(kind);
            entry.result = Some(execution.result.clone());
            entry.message.clone_from(&execution.message);
            entry.warnings.clone_from(&execution.warnings);
            self.report.units.insert(name.clone(), entry);
        }
    }

    /// Delete the leftover run refs this run consumed and retire those whose
    /// checkpoints are evidence for a unit still uncertain; failures are
    /// warnings, since a re-read leftover imports as a no-op.
    ///
    /// The coordination refs are read again first, and only refs unchanged
    /// since the takeover read them are consumed: a run whose lease was
    /// broken may still be alive and have checkpointed in the meantime.
    fn consume(&mut self, leftovers: &Leftovers<'_>, observed: &BTreeMap<UnitName, ObservedUnit>) {
        let consumption = takeover::consumed(leftovers, observed);
        if consumption == takeover::Consumption::default() {
            return;
        }
        let store = self.context().base.store;
        let (location, environment) = (self.location(), self.environment());
        let consumption = match store.coordination(location, environment) {
            Ok(current) => takeover::unchanged(consumption, leftovers, &current),
            Err(error) => {
                let finding = store_error(environment, &error);
                self.report.warnings.push(finding);
                return;
            }
        };
        // Consume each run ref only at the version the takeover imported; a
        // ref that moved in between is left for the next run.
        let version = |run: &RunId| {
            leftovers
                .runs
                .iter()
                .find(|state| &state.run.run == run)
                .map(|state| state.version.as_str())
        };
        let mut failures = Vec::new();
        for run in &consumption.delete {
            failures.extend(store.delete_run(location, environment, run, version(run)).err());
        }
        for run in &consumption.retire {
            failures.extend(store.retire_run(location, environment, run, version(run)).err());
        }
        failures.retain(|error| !matches!(error, StateError::Conflict { .. }));
        for run in &consumption.delete_evidence {
            failures.extend(store.delete_evidence(location, environment, run).err());
        }
        for error in failures {
            let finding = store_error(environment, &error);
            self.report.warnings.push(finding);
        }
    }

    /// Release the lease and delete or keep the run ref, as `ending` says.
    fn end(&mut self, ending: &Ending) {
        let base = self.context().base;
        let (release, delete) = match ending {
            Ending::Killed => return,
            Ending::Release => (true, true),
            Ending::Cancelled if !self.checkpointed => (true, true),
            Ending::Cancelled | Ending::KeepRun { .. } => {
                let (release, unrecorded) = match ending {
                    Ending::KeepRun { release, unrecorded } => (*release, unrecorded.as_slice()),
                    _ => (true, &[][..]),
                };
                self.record.finished = true;
                self.record.units = unrecorded.to_vec();
                if let Err(error) = self.write_run(None) {
                    let finding = store_error(self.environment(), &error);
                    self.report.warnings.push(finding);
                }
                (release, false)
            }
        };
        if self.signalled && self.session.request.mode.writes_run_record() {
            // The signals sent to this run are consumed with it.
            let _ = base
                .store
                .delete_signals(self.location(), self.environment(), &self.run);
        }
        if delete && self.session.request.mode.writes_run_record() {
            if let Err(error) = base
                .store
                .delete_run(self.location(), self.environment(), &self.run, None)
            {
                let finding = store_error(self.environment(), &error);
                self.report.warnings.push(finding);
            }
        }
        if release {
            if let Err(error) = lease::release(base, &self.held, self.landed) {
                let finding = store_error(self.environment(), &error);
                self.report.warnings.push(finding);
            }
        }
    }
}

/// The takeover state of one reconcile: what it imported, marked, and handled.
struct Takeover {
    /// Whether there is anything to take over.
    active: bool,
    /// Units with a desired document the run does not resolve (adopted
    /// units no longer selected, and leaving units).
    unresolved: BTreeMap<UnitName, DesiredUnit>,
    handled: BTreeSet<(RunId, UnitName)>,
    imported: BTreeSet<UnitName>,
    /// Units the takeover marked uncertain and units the run recovered.
    recovered: BTreeSet<UnitName>,
}

impl Takeover {
    fn new(leftovers: &Leftovers<'_>, overlay: &EnvironmentSnapshot, units: &BTreeMap<UnitName, DesiredUnit>) -> Self {
        Self {
            active: !leftovers.runs.is_empty() || leftovers.replaced.is_some(),
            unresolved: overlay
                .desired
                .iter()
                .filter(|(name, _)| !units.contains_key(*name))
                .map(|(name, unit)| (name.clone(), unit.clone()))
                .collect(),
            handled: BTreeSet::new(),
            imported: BTreeSet::new(),
            recovered: BTreeSet::new(),
        }
    }

    /// Overlay an import on the run's evidence; whether it changed any
    /// observed document.
    fn apply(
        &mut self,
        import: takeover::Import,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
        produced: &mut BTreeSet<UnitName>,
        report: &mut EnvironmentRunReport,
    ) -> bool {
        let changed = !import.observed.is_empty();
        observed.extend(import.observed);
        for (name, set) in import.artifacts {
            if set.is_empty() {
                artifacts.remove(&name);
            } else {
                artifacts.insert(name.clone(), set);
            }
            // Its receipt reaches state with this run's commit.
            produced.insert(name);
        }
        self.imported.extend(import.imported);
        self.recovered.extend(import.recovered);
        self.handled.extend(import.handled);
        report.warnings.extend(import.warnings);
        changed
    }
}

/// The result of a unit whose current receipt this run imported.
fn imported_result(taking: &Takeover, unit: &DesiredUnit, observed: Option<&ObservedUnit>) -> Option<UnitResult> {
    if !taking.imported.contains(&unit.unit.name) || !receipt_is_current(unit, observed) {
        return None;
    }
    let receipt = observed?.receipt.as_ref()?;
    Some(UnitResult::Imported {
        execution_key: receipt.execution_key.clone(),
        run: receipt.run.clone(),
    })
}

/// The approvals a `--local` run records for the units of `batch`
/// (`rules::selection::local_approvals`).
fn local_approvals(
    session: &Session<'_, '_>,
    resolved: &Resolved,
    batch: &[UnitName],
) -> BTreeMap<UnitName, ApprovalRecord> {
    let Some(local) = &session.request.local else {
        return BTreeMap::new();
    };
    crate::rules::selection::local_approvals(
        &local.approve,
        session.context.commit_identity,
        session.context.drivers,
        batch.iter().map(|name| &resolved.units[name]),
    )
}

/// The result of a unit this run did not execute.
fn unexecuted_result(
    unit: &DesiredUnit,
    observed: Option<&ObservedUnit>,
    skipped: Option<&Skipped>,
    errors: Option<&Vec<Finding>>,
) -> UnitResult {
    if let Some(error) = errors.and_then(|errors| errors.first()) {
        return UnitResult::Blocked {
            reason: error.message.clone(),
        };
    }
    // A current unit that drift repair or `--reapply` would re-run but
    // this run does not ("Drift").
    match skipped {
        Some(Skipped::Drifted) => return UnitResult::Drifted,
        Some(Skipped::RepairApproval) => return UnitResult::AwaitingApproval,
        _ => {}
    }
    if receipt_is_current(unit, observed) {
        return UnitResult::Current;
    }
    if let Readiness::Blocked { reasons } = &unit.readiness {
        return UnitResult::Blocked {
            reason: reasons
                .iter()
                .map(|reason| reason.reason.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        };
    }
    let condition = observed.and_then(|observed| observed.condition.as_ref());
    match (skipped, condition) {
        (Some(Skipped::Uncertain), Some(Condition::Uncertain { reason, .. })) => {
            UnitResult::Uncertain { reason: reason.clone() }
        }
        (Some(Skipped::FailedNonRetryable), Some(Condition::Failed { category, .. })) => UnitResult::Failed {
            category: *category,
            retryable: false,
        },
        (Some(Skipped::LocalApproval), _) => UnitResult::AwaitingApproval,
        _ if !matches!(unit.lifecycle, Lifecycle::Active) => UnitResult::Blocked {
            reason: "its lifecycle is not active".to_owned(),
        },
        _ => UnitResult::Blocked {
            reason: "it cites receipts that are no longer current".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn environment(spec: &Value) -> Environment {
        serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Environment",
            "metadata": {"name": "dev"},
            "spec": spec
        }))
        .unwrap()
    }

    /// Catches the environment record keeping S's own source or protection
    /// fields, which the entry worktree decides, or dropping S's values.
    #[test]
    fn test_environment_spec_takes_source_and_protection_from_the_entry() {
        let at_source = environment(&json!({
            "values": {"replicas": 2},
            "protectedRefs": ["release"],
            "allowUnprotectedSource": true
        }));
        let entry = environment(&json!({"values": {"replicas": 1}, "protectedRefs": ["main"]}));
        let spec = environment_spec(&at_source, &entry);
        assert_eq!(spec["values"]["replicas"], 2);
        assert_eq!(spec["protectedRefs"], json!(["main"]));
        assert!(spec.get("allowUnprotectedSource").is_none());
    }

    /// Catches a driver receiving process variables the unit never declared.
    #[test]
    fn test_admitted_env_holds_path_and_declared_passthrough_only() {
        let process = BTreeMap::from([
            ("PATH".to_owned(), "/bin".to_owned()),
            ("TOKEN".to_owned(), "secret".to_owned()),
            ("REGION".to_owned(), "eu".to_owned()),
        ]);
        let env = admitted_env(&process, &json!({"env": {"passthrough": ["REGION", "MISSING"]}}));
        assert_eq!(
            env,
            BTreeMap::from([
                ("PATH".to_owned(), "/bin".to_owned()),
                ("REGION".to_owned(), "eu".to_owned()),
            ])
        );
    }

    /// Catches a fingerprint running past the held lease, when another
    /// runner may already have broken it.
    #[test]
    fn test_resolution_deadline_stays_within_the_lease() {
        let settings = crate::shell::Settings::default();
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            resolution_deadline(&settings, now, None),
            now + chrono::Duration::minutes(30)
        );
        let lease = now + chrono::Duration::minutes(15);
        assert_eq!(
            resolution_deadline(&settings, now, Some(lease)),
            now + chrono::Duration::minutes(5)
        );
        let long = now + chrono::Duration::minutes(120);
        assert_eq!(
            resolution_deadline(&settings, now, Some(long)),
            now + chrono::Duration::minutes(30)
        );
    }

    #[test]
    fn test_project_path_maps_worktree_rooted_directories() {
        assert_eq!(project_path(Path::new("/w"), "/"), PathBuf::from("/w"));
        assert_eq!(project_path(Path::new("/w"), "/nyl"), PathBuf::from("/w/nyl"));
    }
}
