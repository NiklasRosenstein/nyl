//! What the integration tests that drive the orchestration shell's API
//! share: the walkthroughs' project in a [`World`], one state store, the fake
//! kinds, fresh clones of `main` per invocation, report helpers, and a
//! memory store that fails on request.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, CommitIdentity, Committed, Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry,
    SourceRenderer as _, StateError, StateResult, StateSide, StateStore, StateTips, Transition,
};
use nyl_core::orchestration::state::{Condition, DesiredUnit, Lease, ObservedUnit, Run, Signal, StateLocation};
use nyl_core::orchestration::time::Duration;
use nyl_drivers::fake::Script;
use nyl_drivers::process::ProcessRunner;
use nyl_git::GitAuth;
use nyl_orchestration::fault::{FaultInjector, NoFaults};
use nyl_orchestration::run_report::{EnvironmentRunReport, RunReport};
use nyl_orchestration::shell::run::{self, Cancellation, RunContext, RunRequest};
use nyl_orchestration::shell::state::{self as state_ops, Requested};
use nyl_orchestration::shell::{Context, Settings};
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_render::gitops::units::UnitSourceRenderer;
use nyl_state::git::{CommitAuthor, GitStateStore};
use nyl_state::memory::MemoryStateStore;
use nyl_state::source::GitSourceResolver;
use tempfile::TempDir;

use super::runner::{AUTHOR, START};
use super::world::{git_output, World};

pub fn start() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(START).unwrap().to_utc()
}

pub fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

pub fn unit(name: &str) -> UnitName {
    UnitName::new(name).unwrap()
}

/// The walkthroughs' shared project, with `nyl.toml` in `nyl/`.
pub fn shared_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/walkthroughs/_project")
}

/// The process environment tools see: this process's `PATH`, as the tier-1
/// runner passes it.
pub fn process_env() -> BTreeMap<String, String> {
    std::env::var("PATH")
        .map(|path| BTreeMap::from([("PATH".to_owned(), path)]))
        .unwrap_or_default()
}

/// `dev`'s state location in the world's `project.git`.
pub fn location(world: &World) -> StateLocation {
    StateLocation {
        repository: world.project_url(),
        desired_ref: "nyl/dev/desired".to_owned(),
        observed_ref: "nyl/dev/observed".to_owned(),
        path: String::new(),
        coordination_ref_prefix: "refs/nyl/".to_owned(),
    }
}

/// A Git state store working in `clone`, committing as [`AUTHOR`].
pub fn git_store(clone: &Path, clock: &Arc<FakeClock>) -> GitStateStore {
    GitStateStore::new(
        clone.join(".git"),
        GitAuth::none(),
        CommitAuthor {
            identity: AUTHOR.parse().unwrap(),
            clock: clock.clone(),
        },
    )
}

/// Write `files` into a new project directory.
pub fn project(files: &[(&str, &str)]) -> TempDir {
    let directory = TempDir::new().unwrap();
    for (path, content) in files {
        let target = directory.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
    directory
}

pub const NYL_TOML: (&str, &str) = ("nyl/nyl.toml", "[project]\n");

/// An Environment `dev` selecting units labelled `dev: 'true'`, from any source.
pub const ENVIRONMENT: &str = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector:\n    matchLabels: {dev: 'true'}\n  allowUnprotectedSource: true\n";

/// A Command unit of `dev` running `script`.
pub fn command(name: &str, script: &str, extra: &str) -> String {
    format!(
        "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: {name}\n  labels: {{dev: 'true'}}\nspec:\n  command: [sh, -c, '{script}']\n{extra}"
    )
}

/// The units of a report whose outcome is `name`.
pub fn outcome(report: &EnvironmentRunReport, name: &str) -> BTreeSet<String> {
    report
        .units
        .iter()
        .filter(|(_, unit)| unit.outcome_name() == Some(name))
        .map(|(unit, _)| unit.to_string())
        .collect()
}

pub fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

pub fn names(units: &[UnitName]) -> BTreeSet<String> {
    units.iter().map(ToString::to_string).collect()
}

pub fn codes(findings: &[Finding]) -> Vec<Code> {
    findings.iter().map(|finding| finding.code).collect()
}

/// The run an uncertain condition names.
pub fn uncertain_run(observed: Option<&ObservedUnit>) -> Option<RunId> {
    match observed?.condition.as_ref()? {
        Condition::Uncertain { run, .. } => Some(run.clone()),
        Condition::Failed { .. } => None,
    }
}

/// A test world with one state store, run against fresh clones of `main`.
pub struct Harness {
    pub world: World,
    pub store: Box<dyn StateStore>,
    pub location: StateLocation,
    pub clock: Arc<FakeClock>,
    pub ids: SequentialIds,
    pub script: Script,
    pub drivers: DriverSet,
    pub faults: Box<dyn FaultInjector>,
    pub identity: CommitIdentity,
    pub process_env: BTreeMap<String, String>,
    /// The clone the Git store works in, whose `nyl/dev/*` branches are its
    /// local state.
    pub store_clone: Option<PathBuf>,
}

/// One invocation: a fresh clone of `main` and its request.
pub struct Job {
    pub sources: GitSourceResolver,
    pub renderer: UnitSourceRenderer,
    pub request: RunRequest,
    pub workspace: TempDir,
}

impl Harness {
    /// The world of `project`, with a Git store in a clone of its own or an
    /// in-memory store.
    pub fn new(project: &Path, git_store: bool) -> Self {
        let world = World::new(project);
        let clock = Arc::new(FakeClock::new(start()));
        let (store, store_clone): (Box<dyn StateStore>, _) = if git_store {
            let clone = world.fresh_clone("main");
            (Box::new(self::git_store(&clone, &clock)), Some(clone))
        } else {
            (Box::new(MemoryStateStore::new()), None)
        };
        let script = Script::new();
        let drivers = nyl_drivers::registry::with_test_kinds(&script);
        Self {
            location: location(&world),
            world,
            store,
            clock,
            ids: SequentialIds::new(),
            script,
            drivers,
            faults: Box::new(NoFaults),
            identity: AUTHOR.parse().unwrap(),
            process_env: process_env(),
            store_clone,
        }
    }

    pub fn context(&self) -> Context<'_> {
        self.context_on(self.store.as_ref())
    }

    /// The shell context working through `store`, such as a `LocalStore`.
    pub fn context_on<'a>(&'a self, store: &'a dyn StateStore) -> Context<'a> {
        Context {
            store,
            clock: self.clock.as_ref(),
            ids: &self.ids,
            faults: self.faults.as_ref(),
            runner: "test-runner".to_owned(),
            requested_by: "test".to_owned(),
            version: "0.0.0-test".to_owned(),
            settings: Settings::default(),
        }
    }

    pub fn init_at(&self, location: &StateLocation) {
        let report = state_ops::init(&self.context(), &dev(), location, false, &Requested::default());
        assert_eq!(report.exit, 0, "{report:?}");
    }

    pub fn init(&self) {
        self.init_at(&self.location);
    }

    /// A fresh clone of `main`, with the entry Environment as it renders there.
    pub fn job(&self, units: &[&str], cancellation: Cancellation) -> Job {
        let clone = self.world.fresh_clone("main");
        let entry_commit = CommitId::new(git_output(&clone, &["rev-parse", "HEAD"])).unwrap();
        let sources = GitSourceResolver::new(clone.join(".git"), GitAuth::none());
        let renderer = UnitSourceRenderer::new("/nyl");
        let entry = renderer.render(&clone, &dev(), &entry_commit).unwrap().environment;
        Job {
            sources,
            renderer,
            request: RunRequest {
                environment: dev(),
                entry,
                location: self.location.clone(),
                entry_commit,
                entry_project_dir: "/nyl".to_owned(),
                units: (!units.is_empty()).then(|| units.iter().map(|name| unit(name)).collect()),
                concurrency: run::DEFAULT_CONCURRENCY,
                source_override: None,
                wait: None,
                local: None,
                cancellation,
                mode: run::RunMode::default(),
                lifecycle: run::LifecycleOptions::default(),
                reapply: std::collections::BTreeSet::new(),
                reason: None,
            },
            workspace: TempDir::new().unwrap(),
        }
    }

    /// Run `operation` on `job` through `store`, with the commit identity
    /// when `identity` is set.
    pub fn run_on(
        &self,
        store: &dyn StateStore,
        job: &Job,
        identity: bool,
        operation: fn(&RunContext<'_>, &RunRequest) -> RunReport,
    ) -> RunReport {
        let base = self.context_on(store);
        // Running tools receive the run's cancelling signal.
        let tools = ProcessRunner::new().cancelled_by(job.request.cancellation.shared());
        let context = RunContext {
            base: &base,
            sources: &job.sources,
            renderer: &job.renderer,
            drivers: &self.drivers,
            tools: &tools,
            workspace: job.workspace.path(),
            process_env: &self.process_env,
            commit_identity: identity.then_some(&self.identity),
            observer: None,
            prompt: None,
        };
        operation(&context, &job.request)
    }

    pub fn run_with(&self, job: &Job, operation: fn(&RunContext<'_>, &RunRequest) -> RunReport) -> RunReport {
        self.run_on(self.store.as_ref(), job, true, operation)
    }

    pub fn run(&self, job: &Job) -> RunReport {
        self.run_with(job, run::reconcile)
    }

    pub fn reconcile_with(&self, units: &[&str], cancellation: Cancellation) -> EnvironmentRunReport {
        let job = self.job(units, cancellation);
        self.run(&job).environments.remove(0)
    }

    pub fn reconcile(&self, units: &[&str]) -> EnvironmentRunReport {
        self.reconcile_with(units, Cancellation::default())
    }

    /// A plan, which needs no commit identity.
    pub fn plan(&self, units: &[&str]) -> EnvironmentRunReport {
        let job = self.job(units, Cancellation::default());
        self.run_on(self.store.as_ref(), &job, false, run::plan)
            .environments
            .remove(0)
    }

    pub fn verify(&self) -> EnvironmentRunReport {
        let job = self.job(&[], Cancellation::default());
        self.run_with(&job, nyl_orchestration::shell::verify::verify)
            .environments
            .remove(0)
    }

    pub fn status(&self) -> EnvironmentRunReport {
        nyl_orchestration::shell::inspect::status(&self.context(), &dev(), &self.location)
            .environments
            .remove(0)
    }

    pub fn snapshot(&self) -> EnvironmentSnapshot {
        self.store.snapshot(&self.location).unwrap()
    }

    /// The commits on each state ref, read from the store.
    pub fn tips(&self) -> StateTips {
        self.snapshot().tips
    }

    pub fn coordination(&self) -> Coordination {
        self.store.coordination(&self.location, &dev()).unwrap()
    }

    pub fn lease_released(&self) -> bool {
        let coordination = self.coordination();
        coordination.lease.is_none() && coordination.runs.is_empty()
    }

    pub fn advance(&self, minutes: u64) {
        self.clock.advance(Duration::from_mins(minutes));
    }
}

/// Something another process does to the state repository.
pub type Interleaved = Box<dyn FnOnce(&MemoryStateStore) + Send>;

/// A memory store that fails like an unreachable remote on request:
/// `update_lease` from its `fail_lease_updates_from`-th call on, the next
/// `delete_run_failures` run ref deletions and `checkpoint_failures`
/// checkpoint writes; it runs `before_commit` once right before the next
/// transition commit, as a concurrent process could. Everything else is
/// delegated.
#[derive(Default)]
pub struct FaultyStore {
    pub inner: MemoryStateStore,
    pub lease_updates: Arc<AtomicUsize>,
    pub fail_lease_updates_from: Option<usize>,
    pub delete_run_failures: Arc<AtomicUsize>,
    pub checkpoint_failures: Arc<AtomicUsize>,
    pub before_commit: Arc<Mutex<Option<Interleaved>>>,
}

fn unavailable() -> StateError {
    StateError::Unavailable("push failed: connection reset".to_owned())
}

/// Take one of the failures `left` counts, if any are left.
fn take_failure(left: &AtomicUsize) -> bool {
    left.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
        .is_ok()
}

impl StateStore for FaultyStore {
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
        self.inner.snapshot(location)
    }
    fn coordination(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<Coordination> {
        self.inner.coordination(location, environment)
    }
    fn take_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: Option<&str>,
    ) -> StateResult<String> {
        self.inner.take_lease(location, environment, lease, expected)
    }
    fn update_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: &str,
    ) -> StateResult<String> {
        let call = self.lease_updates.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_lease_updates_from.is_some_and(|from| call >= from) {
            return Err(unavailable());
        }
        self.inner.update_lease(location, environment, lease, expected)
    }
    fn release_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        expected: &str,
    ) -> StateResult<()> {
        self.inner.release_lease(location, environment, expected)
    }
    fn delete_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()> {
        self.inner.delete_lease(location, environment)
    }
    fn write_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        checkpoint: Option<&Checkpoint>,
    ) -> StateResult<()> {
        if checkpoint.is_some() && take_failure(&self.checkpoint_failures) {
            return Err(unavailable());
        }
        self.inner.write_run(location, environment, run, checkpoint)
    }
    fn delete_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        if take_failure(&self.delete_run_failures) {
            return Err(unavailable());
        }
        self.inner.delete_run(location, environment, run, expected)
    }
    fn retire_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        self.inner.retire_run(location, environment, run, expected)
    }
    fn delete_evidence(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        self.inner.delete_evidence(location, environment, run)
    }
    fn signals(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
    ) -> StateResult<Vec<Signal>> {
        self.inner.signals(location, environment, run)
    }
    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()> {
        self.inner.send_signal(location, environment, signal)
    }
    fn delete_signals(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        self.inner.delete_signals(location, environment, run)
    }
    fn commit(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_version: &str,
    ) -> StateResult<Committed> {
        let interleaved = self.before_commit.lock().unwrap().take();
        if let Some(interleaved) = interleaved {
            interleaved(&self.inner);
        }
        self.inner
            .commit(location, environment, base, transition, lease_version)
    }
    fn import_history(&self, from: &StateLocation, to: &StateLocation) -> StateResult<()> {
        self.inner.import_history(from, to)
    }
    fn write_run_created(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        self.inner.write_run_created(location, environment, run, created)
    }
    fn log(&self, location: &StateLocation, side: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
        self.inner.log(location, side, limit)
    }
    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        self.inner.local_divergence(location)
    }
    fn reset_local(&self, location: &StateLocation, expected: &StateTips) -> StateResult<StateTips> {
        self.inner.reset_local(location, expected)
    }
    fn commit_local(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_location: &StateLocation,
        lease_version: &str,
    ) -> StateResult<Committed> {
        self.inner
            .commit_local(location, environment, base, transition, lease_location, lease_version)
    }
}
