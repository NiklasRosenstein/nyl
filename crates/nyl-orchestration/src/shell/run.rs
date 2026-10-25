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
//! driver may block on its own runtime (as `FakePublication` does to render):
//! the whole operation runs on a scoped thread of its own, and executions on
//! further scoped threads.
//!
//! A run whose lease was lost, or that hit a store failure (a lease update,
//! run record, or transition commit) after a unit's checkpoint was written,
//! keeps its run ref, marked finished, so executed units' results are not
//! lost; every other run deletes it and releases the lease, including runs
//! that change nothing and runs whose units failed.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{
    DriverOperation, DriverSet, ExecutionContext, KeyInputs, Outcome, ResolutionContext, Supported, ToolRunner,
};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, RunId, UnitAddress, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, CommitIdentity, EnvironmentSnapshot, RenderedSource, SourceRenderer, SourceRepository, SourceResolver,
    StateError, Transition, TransitionChanges,
};
use nyl_core::orchestration::state::{
    ArtifactDigest, ArtifactDocument, ArtifactUnit, Condition, DesiredUnit, Lifecycle, ObservedUnit, ObservedUnitKind,
    Operation, Readiness, Receipt, Run, RunKind, StateLocation, StateV1,
};
use nyl_core::orchestration::time::{Duration, Timestamp};
use nyl_core::resources::environment::Environment;
use nyl_core::resources::units::OutputDeclaration;
use serde_json::{json, Map, Value};

use super::lease::{self, HeldLease, LeaseError};
use super::state::store_error;
use super::Context;
use crate::fault::{FaultAction, FaultInjector, FaultPoint};
use crate::rules::desired::{desire, DesireInputs, Desired};
use crate::rules::execution_key::{execution_key, stored_fingerprint, KeyMaterial};
use crate::rules::freshness::receipt_is_current;
use crate::rules::graph::{extract, topological_order};
use crate::rules::references::{resolve_unit, ReferenceView};
use crate::rules::selection::{batches, select, Candidates, Skipped};
use crate::rules::sources::{
    effective_commit, pin_refs, pin_source_fields, root_local_paths, run_source_refs, source_request, source_revisions,
    unreachable, unsupported_source, SourceRequest,
};
use crate::rules::state_location::check_record;
use crate::rules::unit_state::{receipt_state, unit_state, NO_CURRENT_RECEIPT};
use crate::run_report::{BlockedBy, EnvironmentRunReport, ReportKind, RunReport, UnitReport, UnitResult, UnitState};
use crate::transition::{self, Trailers};

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
    /// A snapshot of the whole process environment, read once by the command
    /// line, never by the shell. The shell admits from it only `PATH` and
    /// each unit's declared `env.passthrough` into that unit's
    /// `ExecutionContext.env`/`ResolutionContext.env`; nothing else reaches a
    /// driver. Units declaring `env.secrets` are rejected in M3
    /// (`NYL-UNIT-SECRETS-UNSUPPORTED`) because no secrets provider is wired.
    pub process_env: &'a BTreeMap<String, String>,
    /// Who signs commits drivers write, such as publication commits, with the
    /// clock's time: the identity the command line resolved for state
    /// commits. `None` for `plan`, which writes nothing.
    pub commit_identity: Option<&'a CommitIdentity>,
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
        let report = reconcile_environment(&session);
        session.clean();
        report
    });
    let mut report = RunReport::new(ReportKind::ReconcileReport);
    report.environments.push(environment);
    report
}

/// Run `body` on a scoped thread of its own, outside any async runtime the
/// caller may be in.
fn off_runtime<T: Send>(body: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| match scope.spawn(body).join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    })
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
    units: BTreeMap<UnitName, DesiredUnit>,
    /// The effective commit of every unit with an execution key.
    commits: BTreeMap<UnitName, CommitId>,
    /// Resolution errors, per unit.
    errors: BTreeMap<UnitName, Vec<Finding>>,
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
        let mut index = 0usize;
        let directory = loop {
            let candidate = context.workspace.join(format!("session-{index}"));
            if !candidate.exists() {
                break candidate;
            }
            index += 1;
        };
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

    /// The workspace's render cache, shared by the invocation's runs.
    fn cache_dir(&self) -> PathBuf {
        self.context.workspace.join("cache")
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
            for unit in named.iter().filter(|unit| !desired.units.contains_key(*unit)) {
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
        let graph = extract(self.environment(), &source_commit, &desired.units)?;
        let order = topological_order(&graph);
        Ok(Prepared {
            source_commit,
            rendered,
            desired,
            order,
        })
    }

    /// Resolve every unit in dependency order against `evidence`.
    fn resolve(&self, prepared: &Prepared, evidence: &Evidence<'_>) -> Resolved {
        let mut units = prepared.desired.units.clone();
        let mut commits = BTreeMap::new();
        let mut errors: BTreeMap<UnitName, Vec<Finding>> = BTreeMap::new();
        for name in &prepared.order {
            let mut unit = prepared.desired.units[name].clone();
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
            if !resolution.errors.is_empty() {
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
            if let (Some(mut spec), true) = (resolution.resolved_spec, resolution.errors.is_empty()) {
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
                        commits.insert(name.clone(), keyed.commit);
                    }
                    Err(finding) => errors.entry(name.clone()).or_default().push(finding),
                }
            }
            units.insert(name.clone(), unit);
        }
        Resolved { units, commits, errors }
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
                cache_dir: self.cache_dir(),
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
        Some(lease) => {
            let bound = lease
                .checked_sub_signed(settings.lease_grace.to_chrono())
                .unwrap_or(lease);
            own.min(bound)
        }
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
fn admitted_env(process_env: &BTreeMap<String, String>, spec: &Value) -> BTreeMap<String, String> {
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
fn project_path(worktree: &Path, project_dir: &str) -> PathBuf {
    let relative = project_dir.trim_matches('/');
    if relative.is_empty() {
        worktree.to_path_buf()
    } else {
        worktree.join(relative)
    }
}

/// A unit's `timeout`, else its kind's default from the contract's Defaults.
fn unit_timeout(unit: &DesiredUnit) -> Duration {
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

/// The observed document of the unit's current incarnation.
fn same_incarnation<'o>(unit: &DesiredUnit, observed: Option<&'o ObservedUnit>) -> Option<&'o ObservedUnit> {
    observed.filter(|observed| observed.unit.uid == unit.unit.uid)
}

/// The run of a unit's latest condition or receipt.
fn last_run(observed: Option<&ObservedUnit>) -> Option<RunId> {
    let observed = observed?;
    match &observed.condition {
        Some(Condition::Failed { run, .. } | Condition::Uncertain { run, .. }) => Some(run.clone()),
        None => observed.receipt.as_ref().map(|receipt| receipt.run.clone()),
    }
}

fn condition_message(observed: Option<&ObservedUnit>) -> Option<String> {
    match observed?.condition.as_ref()? {
        Condition::Failed { message, .. } => Some(message.clone()),
        Condition::Uncertain { reason, .. } => Some(reason.clone()),
    }
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
    match check_record(snapshot.record.as_ref(), environment, &request.location) {
        Ok(()) => {}
        // A plan of an environment without state shows what its first run does.
        Err(finding) if finding.code == Code::StateMissing => report.warnings.push(finding),
        Err(finding) => {
            report.errors.push(finding);
            return;
        }
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
    let resolved = session.resolve(
        &prepared,
        &Evidence {
            observed: &snapshot.observed,
            artifacts: &snapshot.artifacts,
            observed_commit: snapshot.tips.observed.as_ref(),
            run: None,
            produced_in_run: &produced,
            previous: &snapshot.desired,
        },
    );
    report.errors.extend(resolved.errors.values().flatten().cloned());
    let attempted = BTreeSet::new();
    let (ready, _) = select(&Candidates {
        desired: &resolved.units,
        observed: &snapshot.observed,
        drivers: context.drivers,
        named: None,
        attempted: &attempted,
    });
    let ready: BTreeSet<UnitName> = ready.into_iter().collect();
    for (name, unit) in &resolved.units {
        if request.units.as_ref().is_some_and(|named| !named.contains(name)) {
            continue;
        }
        let observed = snapshot.observed.get(name);
        let mut entry = UnitReport::new(unit.unit.kind.clone());
        entry.receipt = Some(receipt_state(Some(unit), observed));
        entry.last_run = last_run(observed);
        entry.message = condition_message(same_incarnation(unit, observed));
        entry.blocked_by = blocked_by(unit);
        entry.state = Some(if ready.contains(name) {
            UnitState::Ready
        } else if matches!(unit.readiness, Readiness::Blocked { .. }) || unit.execution_key.is_none() {
            UnitState::Blocked
        } else {
            unit_state(unit, same_incarnation(unit, observed), false)
        });
        if ready.contains(name) {
            if let Some(commit) = resolved.commits.get(name) {
                plan_unit(session, &prepared, unit, commit, observed, &mut entry);
            }
        }
        report.units.insert(name.clone(), entry);
    }
}

/// Ask a ready unit's driver for its plan, in a worktree of its own.
fn plan_unit(
    session: &Session<'_, '_>,
    prepared: &Prepared,
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
    let worktree = directory.join("worktree");
    let temp_dir = directory.join("tmp");
    if let Err(finding) = session.checkout(commit, &worktree) {
        entry.plan_error = Some(finding.message);
        return;
    }
    if let Err(error) = std::fs::create_dir_all(&temp_dir) {
        entry.plan_error = Some(format!("cannot create {}: {error}", temp_dir.display()));
        return;
    }
    let now = context.base.clock.now();
    let execution = ExecutionContext {
        unit: UnitAddress::new(session.environment().clone(), name.clone()),
        operation: DriverOperation::Plan,
        project_dir: project_path(&worktree, &prepared.rendered.project_dir),
        worktree,
        temp_dir,
        env: admitted_env(context.process_env, &unit.spec),
        approved_digest: None,
        deadline: unit_timeout(unit).after(now),
        now,
        observed,
        tools: context.tools,
        commit_identity: context.commit_identity,
        cache_dir: session.cache_dir(),
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
    /// release the lease when it is still this run's.
    KeepRun { release: bool },
    /// A fault killed the run: write nothing more.
    Killed,
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
}

fn reconcile_environment(session: &Session<'_, '_>) -> EnvironmentRunReport {
    let context = session.context;
    let request = session.request;
    let base = context.base;
    let environment = session.environment();
    let mut report = EnvironmentRunReport::new(environment.clone());
    let run = base.ids.run_id();
    report.run = Some(run.clone());
    let held = match lease::acquire(
        base,
        &request.location,
        environment,
        &run,
        Operation::Reconcile,
        base.settings.resolution_timeout,
        request.wait,
    ) {
        Ok(held) => held,
        Err(LeaseError::Held(finding)) => {
            report.errors.push(finding);
            report.finish(ReportKind::ReconcileReport);
            return report;
        }
        Err(LeaseError::Store(error)) => {
            report.errors.push(store_error(environment, &error));
            report.finish(ReportKind::ReconcileReport);
            return report;
        }
    };
    let record = Run {
        api_version: StateV1,
        kind: RunKind,
        run: run.clone(),
        runner: base.runner.clone(),
        operation: Operation::Reconcile,
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
    };
    session.lease_deadline.set(Some(state.held.lease.deadline));
    let ending = state.holding();
    state.end(&ending);
    let mut report = state.report;
    report.finish(ReportKind::ReconcileReport);
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
        self.context().faults.at(point, unit) == FaultAction::Kill
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
        self.context()
            .base
            .store
            .write_run(self.location(), self.environment(), &self.record, checkpoint)
    }

    /// Update the lease and the run record for a phase.
    fn phase(&mut self, units: Vec<UnitName>, deadline: Timestamp) -> Result<(), Ending> {
        if self.fault(FaultPoint::BeforeLeaseUpdate, None) {
            return Err(self.killed(FaultPoint::BeforeLeaseUpdate));
        }
        let mut lease = self.held.lease.clone();
        lease.units.clone_from(&units);
        lease.deadline = deadline;
        let store = self.context().base.store;
        match store.update_lease(self.location(), self.environment(), &lease, &self.held.version) {
            Ok(version) => {
                self.held.lease = lease;
                self.held.version = version;
                self.session.lease_deadline.set(Some(deadline));
            }
            Err(StateError::LeaseLost) => {
                self.report.errors.push(lease::lost(self.environment()));
                return Err(Ending::KeepRun { release: false });
            }
            Err(error) => {
                let finding = store_error(self.environment(), &error);
                self.report.errors.push(finding);
                return Err(self.failed());
            }
        }
        self.record.units = units;
        self.record.deadline = deadline;
        if let Err(error) = self.write_run(None) {
            let finding = store_error(self.environment(), &error);
            self.report.errors.push(finding);
            return Err(self.failed());
        }
        Ok(())
    }

    /// How a run that holds the lease ends on a store failure: it keeps its
    /// run ref once a checkpoint is on it, else it deletes it.
    fn failed(&self) -> Ending {
        if self.checkpointed {
            Ending::KeepRun { release: true }
        } else {
            Ending::Release
        }
    }

    /// Everything a reconcile does while it holds the lease.
    #[allow(clippy::too_many_lines)] // The contract's steps in order: snapshot, resolve, waves, report, commit.
    fn holding(&mut self) -> Ending {
        let session = self.session;
        let context = session.context;
        let request = session.request;
        let environment = session.environment().clone();
        let snapshot = match context.base.store.snapshot(&request.location) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.report.errors.push(store_error(&environment, &error));
                return Ending::Release;
            }
        };
        if let Err(finding) = check_record(snapshot.record.as_ref(), &environment, &request.location) {
            self.report.errors.push(finding);
            return Ending::Release;
        }
        self.record.read_desired.clone_from(&snapshot.tips.desired);
        self.record.read_observed.clone_from(&snapshot.tips.observed);
        if let Err(error) = self.write_run(None) {
            self.report.errors.push(store_error(&environment, &error));
            return Ending::Release;
        }
        let prepared = match session.prepare(&snapshot) {
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

        let mut observed = snapshot.observed.clone();
        let mut artifacts = snapshot.artifacts.clone();
        let mut produced = BTreeSet::new();
        let mut attempted = BTreeSet::new();
        let mut executions: BTreeMap<UnitName, Execution> = BTreeMap::new();
        let mut first = true;
        let (resolved, skipped) = loop {
            // Resolution and its fingerprints run under a lease renewed for
            // the resolution timeout, never one sized for an earlier phase.
            let deadline = lease::deadline(
                context.base,
                context.base.clock.now(),
                context.base.settings.resolution_timeout,
            );
            if let Err(ending) = self.phase(Vec::new(), deadline) {
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
            );
            if first && !resolved.errors.is_empty() {
                // Resolution errors before anything ran stop the run, as configuration errors do.
                self.report.errors.extend(resolved.errors.values().flatten().cloned());
                return Ending::Release;
            }
            first = false;
            let (ready, skipped) = select(&Candidates {
                desired: &resolved.units,
                observed: &observed,
                drivers: context.drivers,
                named: request.units.as_ref(),
                attempted: &attempted,
            });
            if ready.is_empty() {
                break (resolved, skipped);
            }
            for batch in batches(&ready, request.concurrency) {
                let now = context.base.clock.now();
                let longest = batch
                    .iter()
                    .map(|unit| unit_timeout(&resolved.units[unit]))
                    .max()
                    .unwrap_or_default();
                if let Err(ending) = self.phase(batch.clone(), lease::deadline(context.base, now, longest)) {
                    return ending;
                }
                let results = execute_batch(
                    session, &prepared, &resolved, &observed, &artifacts, &batch, &self.run, &snapshot,
                );
                for execution in results {
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
                            return Ending::KeepRun { release: true };
                        }
                    }
                    self.report.errors.extend(execution.errors.iter().cloned());
                    executions.insert(name, execution);
                }
            }
        };
        self.report.errors.extend(resolved.errors.values().flatten().cloned());

        // The report: one result per unit, only the named ones with --unit.
        for (name, unit) in &resolved.units {
            if request.units.as_ref().is_some_and(|named| !named.contains(name)) {
                continue;
            }
            let current = observed.get(name);
            let mut entry = UnitReport::new(unit.unit.kind.clone());
            entry.receipt = Some(receipt_state(Some(unit), current));
            entry.last_run = last_run(current);
            entry.blocked_by = blocked_by(unit);
            if let Some(execution) = executions.get(name) {
                entry.result = Some(execution.result.clone());
                entry.message.clone_from(&execution.message);
                entry.warnings.clone_from(&execution.warnings);
            } else {
                entry.result = Some(unexecuted_result(
                    unit,
                    same_incarnation(unit, current),
                    skipped.get(name),
                    resolved.errors.get(name),
                ));
                entry.message = condition_message(same_incarnation(unit, current));
            }
            self.report.units.insert(name.clone(), entry);
        }

        // The transition: only the documents that differ from the snapshot.
        let mut changes = TransitionChanges::default();
        if snapshot.environment.as_ref() != Some(&prepared.desired.environment) {
            changes.environment = Some(prepared.desired.environment.clone());
        }
        for (name, unit) in &resolved.units {
            if snapshot.desired.get(name) != Some(unit) {
                changes.desired.insert(name.clone(), Some(unit.clone()));
            }
        }
        for (name, document) in &observed {
            if snapshot.observed.get(name) != Some(document) {
                changes.observed.insert(name.clone(), Some(document.clone()));
            }
        }
        let names: BTreeSet<&UnitName> = artifacts.keys().chain(snapshot.artifacts.keys()).collect();
        for name in names {
            let after = artifacts.get(name);
            if snapshot.artifacts.get(name) != after {
                changes.artifacts.insert(name.clone(), after.cloned());
            }
        }
        if changes.is_empty() {
            return Ending::Release;
        }
        if self.fault(FaultPoint::BeforePush, None) {
            return self.killed(FaultPoint::BeforePush);
        }
        let mut summary = self.report.transition_summary(Operation::Reconcile);
        summary.lease_broken.clone_from(&self.held.broken);
        if let Some(revision) = &request.source_override {
            summary.details.insert("sourceOverride".to_owned(), json!(revision));
        }
        let trailers = Trailers {
            run: self.run.clone(),
            source_commit: Some(source_commit),
            read_desired: snapshot.tips.desired.clone(),
            read_observed: snapshot.tips.observed.clone(),
            runner: context.base.runner.clone(),
            evaluated_at: self.held.lease.taken_at,
            version: context.base.version.clone(),
            requested_by: None,
            reason: None,
            local: false,
        };
        let transition = Transition {
            message: transition::message(&summary, &trailers),
            changes,
        };
        match context.base.store.commit(
            &request.location,
            &environment,
            &snapshot.tips,
            &transition,
            &self.held.version,
        ) {
            Ok(committed) => {
                self.held.version = committed.lease_version;
                if let Some(commit) = committed.desired {
                    self.report.commits.insert("desired", commit);
                }
                if let Some(commit) = committed.observed {
                    self.report.commits.insert("observed", commit);
                }
                Ending::Release
            }
            Err(StateError::LeaseLost) => {
                self.report.errors.push(lease::lost(&environment));
                Ending::KeepRun { release: false }
            }
            Err(error) => {
                self.report.errors.push(store_error(&environment, &error));
                self.failed()
            }
        }
    }

    /// Release the lease and delete or keep the run ref, as `ending` says.
    fn end(&mut self, ending: &Ending) {
        let base = self.context().base;
        let (release, delete) = match ending {
            Ending::Killed => return,
            Ending::Release => (true, true),
            Ending::KeepRun { release } => {
                self.record.finished = true;
                self.record.units.clear();
                if let Err(error) = self.write_run(None) {
                    let finding = store_error(self.environment(), &error);
                    self.report.warnings.push(finding);
                }
                (*release, false)
            }
        };
        if delete {
            if let Err(error) = base.store.delete_run(self.location(), self.environment(), &self.run) {
                let finding = store_error(self.environment(), &error);
                self.report.warnings.push(finding);
            }
        }
        if release {
            if let Err(error) = lease::release(base, &self.held) {
                let finding = store_error(self.environment(), &error);
                self.report.warnings.push(finding);
            }
        }
    }
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
        (Some(Skipped::AwaitingApproval), _) => UnitResult::AwaitingApproval,
        _ if !matches!(unit.lifecycle, Lifecycle::Active) => UnitResult::Blocked {
            reason: "its lifecycle is not active".to_owned(),
        },
        _ => UnitResult::Blocked {
            reason: "it cites receipts that are no longer current".to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// execution

/// One finished execution.
struct Execution {
    name: UnitName,
    result: UnitResult,
    message: Option<String>,
    warnings: Vec<Finding>,
    errors: Vec<Finding>,
    /// What the run ref records; `None` when nothing is recorded, such as
    /// for a driver error before any effect.
    checkpoint: Option<Checkpoint>,
    /// The new artifact set of a success.
    artifacts: Option<BTreeMap<String, ArtifactDocument>>,
    /// A fault killed the run after the effect.
    killed: bool,
}

/// Everything one execution thread needs, prepared before it starts.
struct Job<'j> {
    unit: &'j DesiredUnit,
    commit: &'j CommitId,
    observed: Option<&'j ObservedUnit>,
    previous_artifacts: BTreeMap<String, ArtifactDocument>,
    directory: PathBuf,
}

#[allow(clippy::too_many_arguments)] // One wave's inputs: the run, its resolution, its evidence, and the batch.
fn execute_batch(
    session: &Session<'_, '_>,
    prepared: &Prepared,
    resolved: &Resolved,
    observed: &BTreeMap<UnitName, ObservedUnit>,
    artifacts: &BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    batch: &[UnitName],
    run: &RunId,
    snapshot: &EnvironmentSnapshot,
) -> Vec<Execution> {
    let jobs: Vec<Job<'_>> = batch
        .iter()
        .map(|name| Job {
            unit: &resolved.units[name],
            commit: &resolved.commits[name],
            observed: observed.get(name),
            previous_artifacts: artifacts.get(name).cloned().unwrap_or_default(),
            directory: session.execution_directory(name),
        })
        .collect();
    let shared = Shared {
        context: session.context,
        environment: session.environment(),
        project_dir: &prepared.rendered.project_dir,
        cache_dir: session.cache_dir(),
        run,
        read_desired: snapshot.tips.desired.as_ref(),
    };
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let shared = &shared;
                scope.spawn(move || execute(shared, job))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(execution) => execution,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    })
}

/// What every execution of a wave shares.
struct Shared<'s> {
    context: &'s RunContext<'s>,
    environment: &'s EnvironmentName,
    project_dir: &'s str,
    cache_dir: PathBuf,
    run: &'s RunId,
    read_desired: Option<&'s CommitId>,
}

/// Execute one unit in a worktree of its effective commit and map its outcome.
#[allow(clippy::too_many_lines)] // One execution in order: worktree, driver call, fault point, and each outcome's record.
fn execute(shared: &Shared<'_>, job: Job<'_>) -> Execution {
    let context = shared.context;
    let unit = job.unit;
    let name = unit.unit.name.clone();
    let mut execution = Execution {
        name: name.clone(),
        result: UnitResult::Current,
        message: None,
        warnings: Vec::new(),
        errors: Vec::new(),
        checkpoint: None,
        artifacts: None,
        killed: false,
    };
    let before_effects = |execution: &mut Execution, message: String| {
        execution.errors.push(Finding::new(
            Code::DriverError,
            format!("unit {name}: {message}"),
            "fix the cause, such as a missing tool or an unreadable worktree, then run nyl reconcile again",
        ));
        execution.result = UnitResult::Blocked {
            reason: format!("driver error: {message}"),
        };
        execution.message = Some(message);
    };
    let Some(driver) = context.drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
        before_effects(&mut execution, format!("no driver serves kind {}", unit.unit.kind));
        return execution;
    };
    let worktree = job.directory.join("worktree");
    let temp_dir = job.directory.join("tmp");
    let prepared = std::fs::create_dir_all(&worktree)
        .and_then(|()| std::fs::create_dir_all(&temp_dir))
        .map_err(|error| format!("cannot create {}: {error}", job.directory.display()))
        .and_then(|()| {
            context
                .sources
                .worktree(&SourceRepository::This, job.commit, &worktree)
                .map_err(|error| format!("cannot check out commit {}: {error}", job.commit))
        });
    if let Err(message) = prepared {
        before_effects(&mut execution, message);
        let _ = std::fs::remove_dir_all(&job.directory);
        return execution;
    }
    let started = context.base.clock.now();
    let ctx = ExecutionContext {
        unit: UnitAddress::new(shared.environment.clone(), name.clone()),
        operation: DriverOperation::Reconcile,
        project_dir: project_path(&worktree, shared.project_dir),
        worktree,
        temp_dir,
        env: admitted_env(context.process_env, &unit.spec),
        approved_digest: None,
        deadline: unit_timeout(unit).after(started),
        now: started,
        observed: job.observed,
        tools: context.tools,
        commit_identity: context.commit_identity,
        cache_dir: shared.cache_dir.clone(),
    };
    let outcome = driver.reconcile(&ctx, unit);
    drop(ctx);
    execution.killed = context.faults.at(FaultPoint::AfterEffect, Some(&name)) == FaultAction::Kill;
    let finished = context.base.clock.now();
    let _ = std::fs::remove_dir_all(&job.directory);
    let outcome = match outcome {
        Ok(Supported::Supported(outcome)) => outcome,
        Ok(Supported::Unsupported) => {
            before_effects(&mut execution, format!("kind {} cannot reconcile", unit.unit.kind));
            return execution;
        }
        Err(error) if !error.effects_possible() => {
            before_effects(&mut execution, error.to_string());
            return execution;
        }
        Err(error) => Outcome::Uncertain {
            reason: error.to_string(),
        },
    };
    let previous_receipt = same_incarnation(unit, job.observed).and_then(|observed| observed.receipt.clone());
    let condition = |condition: Condition| ObservedUnit {
        api_version: StateV1,
        kind: ObservedUnitKind,
        unit: unit.unit.clone(),
        receipt: previous_receipt.clone(),
        condition: Some(condition),
    };
    let observed = match outcome {
        Outcome::Succeeded { outputs, artifacts, .. } => {
            let key = unit
                .execution_key
                .clone()
                .expect("an executed unit has an execution key");
            let (outputs, warnings) = recorded_outputs(unit, outputs);
            execution.warnings = warnings;
            let (set, digests) = recorded_artifacts(shared.environment, unit, &key, artifacts);
            execution.result = UnitResult::Executed {
                execution_key: key.clone(),
            };
            execution.artifacts = Some(set);
            ObservedUnit {
                api_version: StateV1,
                kind: ObservedUnitKind,
                unit: unit.unit.clone(),
                receipt: Some(Receipt {
                    execution_key: key,
                    behavior_version: driver.behavior_version(),
                    run: shared.run.clone(),
                    read_desired: shared.read_desired.cloned(),
                    started_at: started,
                    finished_at: finished,
                    versions: BTreeMap::from([("nyl".to_owned(), context.base.version.clone())]),
                    approval: None,
                    provenance: unit.provenance.clone(),
                    outputs,
                    artifacts: digests,
                }),
                condition: None,
            }
        }
        Outcome::AwaitingApproval { .. } => {
            execution.result = UnitResult::AwaitingApproval;
            return execution;
        }
        Outcome::Failed {
            category,
            retryable,
            message,
        } => {
            execution.result = UnitResult::Failed { category, retryable };
            execution.message = Some(message.clone());
            condition(Condition::Failed {
                since: finished,
                run: shared.run.clone(),
                category,
                retryable,
                message,
            })
        }
        Outcome::Uncertain { reason } | Outcome::Waiting { reason, .. } => {
            execution.result = UnitResult::Uncertain { reason: reason.clone() };
            execution.message = Some(reason.clone());
            condition(Condition::Uncertain {
                since: finished,
                run: shared.run.clone(),
                reason,
            })
        }
    };
    execution.checkpoint = Some(Checkpoint {
        desired: unit.clone(),
        observed,
        artifacts: execution.artifacts.clone().unwrap_or(job.previous_artifacts),
    });
    execution
}

/// The outputs a receipt records: the declared, non-sensitive ones a
/// driver reported, with a `NYL-OUTPUT-UNDECLARED` warning for each
/// undeclared one.
fn recorded_outputs(unit: &DesiredUnit, outputs: Value) -> (Value, Vec<Finding>) {
    let name = &unit.unit.name;
    let declared = declared_outputs(unit.resolved_spec.as_ref().unwrap_or(&unit.spec));
    let mut recorded = Map::new();
    let mut warnings = Vec::new();
    if let Value::Object(outputs) = outputs {
        for (output, value) in outputs {
            match declared.get(&output) {
                Some(declaration) if !declaration.sensitive => {
                    recorded.insert(output, value);
                }
                Some(_) => {}
                None => warnings.push(Finding::new(
                    Code::OutputUndeclared,
                    format!("unit {name} reported output {output:?}, which it does not declare; it was not recorded"),
                    format!("declare {output} under the unit's spec.outputs to record it, or stop writing it"),
                )),
            }
        }
    }
    (Value::Object(recorded), warnings)
}

/// A success's artifact documents with `metadata.unit` and `executionKey`
/// filled in, and the digests its receipt lists.
fn recorded_artifacts(
    environment: &EnvironmentName,
    unit: &DesiredUnit,
    key: &ExecutionKey,
    artifacts: Vec<ArtifactDocument>,
) -> (BTreeMap<String, ArtifactDocument>, Vec<ArtifactDigest>) {
    let uid = unit.unit.uid.clone().expect("an executed unit has a uid");
    let mut set = BTreeMap::new();
    let mut digests = Vec::new();
    for mut document in artifacts {
        document.metadata.unit = ArtifactUnit {
            environment: environment.clone(),
            name: unit.unit.name.clone(),
            uid: uid.clone(),
        };
        document.execution_key = key.clone();
        digests.push(ArtifactDigest {
            name: document.metadata.name.clone(),
            kind: document.kind.clone(),
            digest: document.digest(),
        });
        set.insert(document.metadata.name.clone(), document);
    }
    (set, digests)
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
