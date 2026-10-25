//! Run takeover, lost leases, recovery by policy, and cancellation in the
//! orchestration shell, through its API, in the scenario test world: real
//! Git source resolution and unit rendering, the built-in Command kind and
//! the fake kinds, and the in-memory or Git state store.
//!
//! Contract: "Runs and leases" items 3 to 8 and "Recovery" in
//! design/orchestration-core.md, and the walkthroughs "Effects without a
//! receipt" and "Competing runners".

mod scenario;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{DriverOperation, DriverSet, RecoveryPolicy};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Clock, CommitIdentity, Committed, Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry,
    SourceRenderer, StateError, StateResult, StateSide, StateStore, StateTips, Transition,
};
use nyl_core::orchestration::state::{
    ApprovalIdentity, ApprovalRecord, Condition, DesiredUnit, FailureCategory, Lease, ObservedUnit, Run, Signal,
    StateLocation,
};
use nyl_core::orchestration::time::Duration;
use nyl_drivers::fake::{Script, Scripted};
use nyl_drivers::process::ProcessRunner;
use nyl_git::GitAuth;
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint, NoFaults};
use nyl_orchestration::run_report::{EnvironmentRunReport, RunReport};
use nyl_orchestration::shell::local::{LeaseScope, LocalRun};
use nyl_orchestration::shell::run::{self, Cancellation, RunContext, RunRequest};
use nyl_orchestration::shell::state::{self as state_ops, Requested};
use nyl_orchestration::shell::verify;
use nyl_orchestration::shell::{Context, Settings};
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_render::gitops::units::UnitSourceRenderer;
use nyl_state::git::{CommitAuthor, GitStateStore};
use nyl_state::memory::MemoryStateStore;
use nyl_state::source::GitSourceResolver;
use scenario::format::CommitStep;
use scenario::world::{git, git_output, World};
use serde_json::json;
use tempfile::TempDir;

fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

fn unit(name: &str) -> UnitName {
    UnitName::new(name).unwrap()
}

/// The walkthroughs' shared project, with `nyl.toml` in `nyl/`.
fn shared_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/walkthroughs/_project")
}

/// Write `files` into a new project directory.
fn project(files: &[(&str, &str)]) -> TempDir {
    let directory = TempDir::new().unwrap();
    for (path, content) in files {
        let target = directory.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
    directory
}

const ENVIRONMENT: &str = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\nspec:\n  unitSelector:\n    matchLabels: {dev: 'true'}\n  allowUnprotectedSource: true\n";

/// A Command unit running `script`.
fn command(name: &str, script: &str, extra: &str) -> String {
    format!(
        "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: {name}\n  labels: {{dev: 'true'}}\nspec:\n  command: [sh, -c, '{script}']\n{extra}"
    )
}

/// A fault injector that kills the run once at `point` for `unit`.
struct KillOnce {
    point: FaultPoint,
    unit: UnitName,
    armed: AtomicBool,
}

impl KillOnce {
    fn new(point: FaultPoint, unit: &str) -> Self {
        Self {
            point,
            unit: UnitName::new(unit).unwrap(),
            armed: AtomicBool::new(true),
        }
    }
}

impl FaultInjector for KillOnce {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        if point == self.point && unit == Some(&self.unit) && self.armed.swap(false, Ordering::SeqCst) {
            FaultAction::Kill
        } else {
            FaultAction::Continue
        }
    }
}

/// A fault injector that deletes the lease ref in the state repository
/// once, while `unit` executes, as an operator with push access could.
struct DeleteLeaseWhileExecuting {
    bare: PathBuf,
    unit: UnitName,
    armed: AtomicBool,
}

impl FaultInjector for DeleteLeaseWhileExecuting {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        if point == FaultPoint::Executing && unit == Some(&self.unit) && self.armed.swap(false, Ordering::SeqCst) {
            git(&self.bare, &["update-ref", "-d", "refs/nyl/dev/lease"]);
        }
        FaultAction::Continue
    }
}

/// A test world with one state store, run against fresh clones of `main`.
struct Harness {
    world: World,
    store: Box<dyn StateStore>,
    location: StateLocation,
    clock: Arc<FakeClock>,
    ids: SequentialIds,
    script: Script,
    drivers: DriverSet,
    faults: Box<dyn FaultInjector>,
    identity: CommitIdentity,
    process_env: BTreeMap<String, String>,
    /// Keeps the Git store's clone alive.
    _store_clone: Option<PathBuf>,
}

/// One invocation: a fresh clone of `main` and its request.
struct Job {
    sources: GitSourceResolver,
    renderer: UnitSourceRenderer,
    request: RunRequest,
    workspace: TempDir,
}

impl Harness {
    fn new(project: &Path, git_store: bool) -> Self {
        let world = World::new(project);
        let clock = Arc::new(FakeClock::new(
            chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
                .unwrap()
                .to_utc(),
        ));
        let identity: CommitIdentity = "Scenario <scenario@example.invalid>".parse().unwrap();
        let (store, store_clone): (Box<dyn StateStore>, _) = if git_store {
            let clone = world.fresh_clone("main");
            let store = GitStateStore::new(
                clone.join(".git"),
                GitAuth::none(),
                CommitAuthor {
                    identity: identity.clone(),
                    clock: clock.clone(),
                },
            );
            (Box::new(store), Some(clone))
        } else {
            (Box::new(MemoryStateStore::new()), None)
        };
        let location = StateLocation {
            repository: world.project_url(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        };
        let script = Script::new();
        let drivers = nyl_drivers::registry::with_test_kinds(&script);
        Self {
            world,
            store,
            location,
            clock,
            ids: SequentialIds::new(),
            script,
            drivers,
            faults: Box::new(NoFaults),
            identity,
            process_env: BTreeMap::from([("PATH".to_owned(), "/usr/local/bin:/usr/bin:/bin".to_owned())]),
            _store_clone: store_clone,
        }
    }

    fn context(&self) -> Context<'_> {
        Context {
            store: self.store.as_ref(),
            clock: self.clock.as_ref(),
            ids: &self.ids,
            faults: self.faults.as_ref(),
            runner: "test-runner".to_owned(),
            requested_by: "test".to_owned(),
            version: "0.0.0-test".to_owned(),
            settings: Settings::default(),
        }
    }

    fn init(&self) {
        let report = state_ops::init(&self.context(), &dev(), &self.location, false, &Requested::default());
        assert_eq!(report.exit, 0, "{report:?}");
    }

    /// A fresh clone of `main`, with the entry Environment as it renders there.
    fn job(&self, units: &[&str], cancellation: Cancellation) -> Job {
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
            },
            workspace: TempDir::new().unwrap(),
        }
    }

    fn run(&self, job: &Job) -> RunReport {
        self.run_with(job, run::reconcile)
    }

    fn run_with(&self, job: &Job, operation: fn(&RunContext<'_>, &RunRequest) -> RunReport) -> RunReport {
        let base = self.context();
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
            commit_identity: Some(&self.identity),
        };
        operation(&context, &job.request)
    }

    fn verify(&self) -> EnvironmentRunReport {
        let job = self.job(&[], Cancellation::default());
        self.run_with(&job, verify::verify).environments.remove(0)
    }

    fn reconcile_with(&self, units: &[&str], cancellation: Cancellation) -> EnvironmentRunReport {
        let job = self.job(units, cancellation);
        self.run(&job).environments.into_iter().next().unwrap()
    }

    fn reconcile(&self, units: &[&str]) -> EnvironmentRunReport {
        self.reconcile_with(units, Cancellation::default())
    }

    fn snapshot(&self) -> EnvironmentSnapshot {
        self.store.snapshot(&self.location).unwrap()
    }

    fn coordination(&self) -> Coordination {
        self.store.coordination(&self.location, &dev()).unwrap()
    }

    fn advance(&self, minutes: u64) {
        self.clock.advance(Duration::from_mins(minutes));
    }
}

/// The units of a report whose outcome is `name`.
fn outcome(report: &EnvironmentRunReport, name: &str) -> BTreeSet<String> {
    report
        .units
        .iter()
        .filter(|(_, unit)| unit.outcome_name() == Some(name))
        .map(|(unit, _)| unit.to_string())
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn names(units: &[UnitName]) -> BTreeSet<String> {
    units.iter().map(ToString::to_string).collect()
}

fn codes(findings: &[Finding]) -> Vec<Code> {
    findings.iter().map(|finding| finding.code).collect()
}

fn uncertain_run(observed: Option<&ObservedUnit>) -> Option<RunId> {
    match observed?.condition.as_ref()? {
        Condition::Uncertain { run, .. } => Some(run.clone()),
        Condition::Failed { .. } => None,
    }
}

/// A run killed after `database`'s effect, with network and web-image
/// checkpointed, as in the walkthrough "Effects without a receipt".
fn crashed_after_database(harness: &mut Harness) -> RunId {
    harness.faults = Box::new(KillOnce::new(FaultPoint::AfterEffect, "database"));
    harness.init();
    let crashed = harness.reconcile(&[]);
    let run = crashed.run.clone().unwrap();
    // The dead run's lease and run ref block until their deadline passes:
    // database's 60-minute timeout plus the 10-minute grace.
    let held = harness.reconcile(&[]);
    assert_eq!(codes(&held.errors), [Code::LeaseHeld], "{held:#?}");
    harness.advance(71);
    run
}

/// The walkthrough "Effects without a receipt": the run taking over imports
/// the checkpointed units, gives the unit the dead run was executing an
/// uncertain condition naming that run, converges it (`recovered`), runs
/// the rest, writes one commit per ref, and deletes the leftover run ref.
#[test]
fn test_takeover_after_a_crash_imports_recovers_and_deletes_the_run_ref() {
    let mut harness = Harness::new(&shared_project(), false);
    crashed_after_database(&mut harness);

    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    assert_eq!(names(&report.imported), set(&["network", "web-image"]));
    assert_eq!(names(&report.recovered), set(&["database"]));
    assert_eq!(outcome(&report, "executed"), set(&["kubernetes"]));
    assert_eq!(outcome(&report, "recovered"), set(&["database"]));
    assert_eq!(outcome(&report, "imported"), set(&["network", "web-image"]));
    assert_eq!(report.commits.len(), 2, "{report:#?}");
    let snapshot = harness.snapshot();
    let database = &snapshot.observed[&unit("database")];
    assert!(database.condition.is_none() && database.receipt.is_some());
    let coordination = harness.coordination();
    assert!(
        coordination.lease.is_none() && coordination.runs.is_empty(),
        "{coordination:#?}"
    );
    assert!(coordination.evidence.is_empty());
}

/// Something another process does to the state repository.
type Interleaved = Box<dyn FnOnce(&MemoryStateStore) + Send>;

/// A memory store whose `delete_run` fails `failures` times, as a network
/// error would, and that runs `before_commit` once right before the next
/// transition commit, as a concurrent process could; everything else is
/// delegated.
struct FailingDeleteStore {
    inner: MemoryStateStore,
    failures: Arc<AtomicUsize>,
    /// How many of the next checkpoint writes fail.
    checkpoint_failures: Arc<AtomicUsize>,
    before_commit: Arc<Mutex<Option<Interleaved>>>,
}

impl StateStore for FailingDeleteStore {
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
        let failing = checkpoint.is_some()
            && self
                .checkpoint_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
                .is_ok();
        if failing {
            return Err(StateError::Unavailable("push failed: connection reset".to_owned()));
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
        let failing = self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok();
        if failing {
            return Err(StateError::Unavailable("push failed: connection reset".to_owned()));
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

/// A takeover whose run ref deletion fails leaves the run ref behind; the
/// next run reads it again, changes nothing, writes no commit, and does not
/// mark the recovered unit again, then deletes the run ref.
#[test]
fn test_takeover_is_idempotent_when_a_leftover_is_read_again() {
    let mut harness = Harness::new(&shared_project(), false);
    let failures = Arc::new(AtomicUsize::new(0));
    harness.store = Box::new(FailingDeleteStore {
        inner: MemoryStateStore::new(),
        failures: failures.clone(),
        checkpoint_failures: Arc::default(),
        before_commit: Arc::default(),
    });
    let dead = crashed_after_database(&mut harness);
    // The takeover's deletion of the leftover run ref fails once.
    failures.store(1, Ordering::SeqCst);

    let takeover = harness.reconcile(&[]);
    assert_eq!(takeover.exit.code(), 0, "{takeover:#?}");
    assert_eq!(names(&takeover.recovered), set(&["database"]));
    assert_eq!(codes(&takeover.warnings), [Code::StateUnavailable], "{takeover:#?}");
    let left = harness.coordination();
    assert_eq!(
        left.runs.iter().map(|state| &state.run.run).collect::<Vec<_>>(),
        [&dead]
    );
    let tips = harness.snapshot().tips;

    let again = harness.reconcile(&[]);
    assert_eq!(again.exit.code(), 0, "{again:#?}");
    assert!(again.commits.is_empty(), "{again:#?}");
    assert!(again.recovered.is_empty() && again.imported.is_empty(), "{again:#?}");
    assert_eq!(
        outcome(&again, "current"),
        set(&["network", "web-image", "database", "kubernetes"])
    );
    assert_eq!(harness.snapshot().tips, tips);
    assert!(harness.coordination().runs.is_empty(), "the leftover is consumed now");
}

/// A run whose lease was broken may still be alive: here the dead run
/// checkpoints the unit it was executing after the takeover read its run
/// ref and before the takeover's transition commit. The takeover must not
/// delete that run ref, so the late checkpoint survives; the next run
/// reports it as a late checkpoint and retires the ref to an evidence ref
/// that `nyl recover --accept` can adopt.
#[test]
fn test_takeover_keeps_a_run_ref_that_checkpointed_after_it_was_read() {
    let mut harness = Harness::new(&shared_project(), false);
    // database stays uncertain, so the late checkpoint is evidence.
    harness.script.recover_by(RecoveryPolicy::Manual);
    let before_commit: Arc<Mutex<Option<Interleaved>>> = Arc::default();
    harness.store = Box::new(FailingDeleteStore {
        inner: MemoryStateStore::new(),
        failures: Arc::new(AtomicUsize::new(0)),
        checkpoint_failures: Arc::default(),
        before_commit: before_commit.clone(),
    });
    let dead = crashed_after_database(&mut harness);
    let leftover = harness.coordination().runs.remove(0);
    assert_eq!(leftover.run.run, dead);
    let database = leftover
        .created
        .iter()
        .find(|desired| desired.unit.name == unit("database"))
        .cloned()
        .expect("database was recorded before it started");
    // The dead run's result for database, finished after the takeover
    // recorded its condition.
    let mut late = leftover.checkpoints[0].clone();
    late.desired = database.clone();
    late.observed.unit = database.unit.clone();
    let receipt = late.observed.receipt.as_mut().unwrap();
    receipt.started_at = harness.clock.now();
    receipt.finished_at = Duration::from_mins(5).after(harness.clock.now());
    let (location, record) = (harness.location.clone(), leftover.run.clone());
    *before_commit.lock().unwrap() = Some(Box::new(move |store: &MemoryStateStore| {
        store.write_run(&location, &dev(), &record, Some(&late)).unwrap();
    }));

    let takeover = harness.reconcile(&[]);
    assert_eq!(names(&takeover.recovered), set(&["database"]), "{takeover:#?}");
    assert!(before_commit.lock().unwrap().is_none(), "the takeover committed");
    let snapshot = harness.snapshot();
    assert_eq!(
        uncertain_run(snapshot.observed.get(&unit("database"))),
        Some(dead.clone())
    );
    let kept = harness.coordination();
    assert_eq!(
        kept.runs.iter().map(|state| &state.run.run).collect::<Vec<_>>(),
        [&dead],
        "the run ref that changed after the takeover read it is kept"
    );

    harness.advance(10);
    let next = harness.reconcile(&[]);
    assert!(codes(&next.warnings).contains(&Code::RunLateCheckpoint), "{next:#?}");
    let after = harness.coordination();
    assert!(after.runs.is_empty(), "{after:#?}");
    assert_eq!(
        after.evidence.iter().map(|state| &state.run.run).collect::<Vec<_>>(),
        [&dead],
        "the late checkpoint is kept as evidence"
    );
}

/// A unit's checkpoint write fails after its effects ran: the run keeps its
/// run ref, marked finished, with the units of the batch whose checkpoint
/// did not reach it recorded as started, so the next run gives them an
/// `uncertain` condition and recovers them instead of running them again as
/// if nothing had happened.
#[test]
fn test_a_failed_checkpoint_write_leaves_the_unit_uncertain_for_the_next_run() {
    let mut harness = Harness::new(&shared_project(), false);
    let checkpoint_failures = Arc::new(AtomicUsize::new(1));
    harness.store = Box::new(FailingDeleteStore {
        inner: MemoryStateStore::new(),
        failures: Arc::default(),
        checkpoint_failures: checkpoint_failures.clone(),
        before_commit: Arc::default(),
    });
    harness.init();
    let failed = harness.reconcile(&[]);
    assert_eq!(codes(&failed.errors), [Code::StateUnavailable], "{failed:#?}");
    assert!(failed.commits.is_empty(), "{failed:#?}");
    let coordination = harness.coordination();
    assert!(coordination.lease.is_none(), "the lease is released: {coordination:#?}");
    let [kept] = coordination.runs.as_slice() else {
        panic!("the run ref is kept: {coordination:#?}");
    };
    assert!(kept.run.finished);
    let unrecorded = names(&kept.run.units);
    assert!(
        !unrecorded.is_empty(),
        "the unit without a checkpoint is recorded as started"
    );
    for unit in &kept.run.units {
        assert!(
            kept.checkpoints
                .iter()
                .all(|checkpoint| &checkpoint.desired.unit.name != unit),
            "{unit} has no checkpoint"
        );
    }

    let next = harness.reconcile(&[]);
    assert_eq!(next.exit.code(), 0, "{next:#?}");
    assert_eq!(names(&next.recovered), unrecorded, "{next:#?}");
    assert!(harness.coordination().runs.is_empty());
}

/// "Runs and leases" item 8: an operation that takes over a broken lease
/// without consuming the broken run's ref (here `verify`) puts the break
/// back when it releases, so the next reconcile still takes the run over at
/// once, before its deadline, and records the break in its commit.
#[test]
fn test_a_lease_break_survives_an_operation_that_does_not_take_the_run_over() {
    let mut harness = Harness::new(&shared_project(), true);
    harness.init();
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    let steps = TempDir::new().unwrap();
    let patch = steps.path().join("web-page/services/web");
    std::fs::create_dir_all(&patch).unwrap();
    std::fs::write(patch.join("index.html"), "<h1>Changed</h1>\n").unwrap();
    harness.world.commit(
        &CommitStep {
            branch: "main".to_owned(),
            message: "Change the web page".to_owned(),
            apply: Some("web-page".to_owned()),
            delete: Vec::new(),
        },
        steps.path(),
    );
    // The run dies while executing web-image, its run ref live for an hour.
    harness.faults = Box::new(KillOnce::new(FaultPoint::AfterEffect, "web-image"));
    let dead = harness.reconcile(&[]).run.unwrap();
    let broken = state_ops::break_lease(
        &harness.context(),
        &dev(),
        &harness.location,
        Some("runner lost".to_owned()),
    );
    assert_eq!(broken.exit, 0, "{broken:?}");

    let verified = harness.verify();
    assert!(verified.run.is_some(), "verify took the lease: {verified:#?}");
    assert!(!codes(&verified.errors).contains(&Code::LeaseHeld), "{verified:#?}");
    let lease = harness
        .coordination()
        .lease
        .expect("the broken lease is put back")
        .lease;
    assert_eq!(lease.run, dead);
    assert_eq!(
        lease.broken.map(|request| request.reason),
        Some(Some("runner lost".to_owned()))
    );

    let takeover = harness.reconcile(&[]);
    assert_eq!(takeover.exit.code(), 0, "{takeover:#?}");
    assert_eq!(names(&takeover.recovered), set(&["web-image"]), "{takeover:#?}");
    let message = git_output(
        &harness.world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/observed"],
    );
    assert!(
        message.contains("\nleaseBroken: {") && message.contains("runner lost"),
        "{message}"
    );
    assert!(harness.coordination().lease.is_none());
}

/// A crashed run created and started a unit that the next run no longer
/// selects: the takeover adopts its uid, commits its desired document and
/// an uncertain observed document naming the dead run, and only then
/// deletes the run ref, so its resources are never left without state.
#[test]
fn test_takeover_records_state_for_a_deselected_unit_the_dead_run_created() {
    let files = [
        ("nyl/nyl.toml", "[project]\n".to_owned()),
        ("nyl/environment.yaml", ENVIRONMENT.to_owned()),
        ("nyl/alpha.yaml", command("alpha", "true", "")),
        ("nyl/beta.yaml", command("beta", "true", "")),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(path, content)| (*path, content.as_str())).collect();
    let project = project(&files);
    let mut harness = Harness::new(project.path(), false);
    harness.faults = Box::new(KillOnce::new(FaultPoint::AfterEffect, "beta"));
    harness.init();
    let crashed = harness.reconcile(&[]);
    let dead = crashed.run.clone().unwrap();
    let created = harness.coordination().runs[0]
        .created
        .iter()
        .find(|desired| desired.unit.name == unit("beta"))
        .cloned()
        .expect("beta was recorded on the run ref before it started");

    let steps = TempDir::new().unwrap();
    harness.world.commit(
        &CommitStep {
            branch: "main".to_owned(),
            message: "Remove beta".to_owned(),
            apply: None,
            delete: vec!["nyl/beta.yaml".to_owned()],
        },
        steps.path(),
    );
    // The Command timeout of 30 minutes plus the 10-minute grace.
    harness.advance(41);
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    assert_eq!(names(&report.imported), set(&["alpha"]));
    assert_eq!(names(&report.recovered), set(&["beta"]));
    assert_eq!(report.leaving.len(), 1, "{report:#?}");
    let snapshot = harness.snapshot();
    assert_eq!(snapshot.desired[&unit("beta")].unit.uid, created.unit.uid);
    assert_eq!(uncertain_run(snapshot.observed.get(&unit("beta"))), Some(dead));
    assert!(harness.coordination().runs.is_empty());
}

/// The walkthrough "Competing runners", third run, against the Git store:
/// the lease ref is deleted while a unit executes; the run checkpoints it,
/// fails its next lease update, exits 4 with `NYL-LEASE-LOST`, writes no
/// state, and the next run imports its result and runs the rest.
#[test]
fn test_lost_lease_exits_4_and_the_next_run_imports_its_checkpoint() {
    let mut harness = Harness::new(&shared_project(), true);
    harness.init();
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    let steps = TempDir::new().unwrap();
    let patch = steps.path().join("web-page/services/web");
    std::fs::create_dir_all(&patch).unwrap();
    std::fs::write(patch.join("index.html"), "<h1>Changed</h1>\n").unwrap();
    harness.world.commit(
        &CommitStep {
            branch: "main".to_owned(),
            message: "Change the web page".to_owned(),
            apply: Some("web-page".to_owned()),
            delete: Vec::new(),
        },
        steps.path(),
    );
    let tips = harness.snapshot().tips;
    harness.faults = Box::new(DeleteLeaseWhileExecuting {
        bare: harness.world.bare(),
        unit: unit("web-image"),
        armed: AtomicBool::new(true),
    });

    let lost = harness.reconcile(&[]);
    assert_eq!(lost.exit.code(), 4, "{lost:#?}");
    assert_eq!(codes(&lost.errors), [Code::LeaseLost]);
    assert!(lost.commits.is_empty());
    assert_eq!(
        harness.snapshot().tips,
        tips,
        "a run that lost its lease writes no state"
    );
    let kept = harness.coordination();
    assert!(kept.runs.iter().all(|state| state.run.finished), "{kept:#?}");

    let next = harness.reconcile(&[]);
    assert_eq!(next.exit.code(), 0, "{next:#?}");
    assert_eq!(names(&next.imported), set(&["web-image"]));
    assert_eq!(outcome(&next, "executed"), set(&["kubernetes"]));
    assert!(harness.coordination().runs.is_empty());
}

/// An uncertain unit whose recovery policy is `inspect` is inspected, not
/// executed: applied effects record its receipt with the inspected outputs,
/// and the unit reports `recovered`.
#[test]
fn test_inspect_policy_records_the_receipt_of_applied_effects_without_executing() {
    let harness = Harness::new(&shared_project(), false);
    harness.script.recover_by(RecoveryPolicy::Inspect);
    // A second execution would fail: the run must not execute again.
    harness
        .script
        .push(unit("network"), DriverOperation::Reconcile, &Scripted::Uncertain, 1);
    let failure = Scripted::Failed {
        category: FailureCategory::ToolError,
        retryable: true,
    };
    harness
        .script
        .push(unit("network"), DriverOperation::Reconcile, &failure, 1);
    let applied = Scripted::Succeeded {
        outputs: Some(json!({"vpcId": "vpc-inspected"})),
    };
    harness
        .script
        .push(unit("network"), DriverOperation::Inspect, &applied, 1);
    harness.init();
    let uncertain = harness.reconcile(&["network"]);
    assert_eq!(uncertain.exit.code(), 4, "{uncertain:#?}");

    let recovered = harness.reconcile(&["network"]);
    assert_eq!(recovered.exit.code(), 0, "{recovered:#?}");
    assert_eq!(outcome(&recovered, "recovered"), set(&["network"]));
    assert_eq!(names(&recovered.recovered), set(&["network"]));
    let snapshot = harness.snapshot();
    let network = &snapshot.observed[&unit("network")];
    assert!(network.condition.is_none());
    assert_eq!(
        network.receipt.as_ref().unwrap().outputs,
        json!({"vpcId": "vpc-inspected"})
    );
}

/// An uncertain `inspect` unit whose inspection cannot tell keeps its
/// condition and waits for `nyl recover`, without executing.
#[test]
fn test_inspect_policy_unknown_leaves_the_condition() {
    let harness = Harness::new(&shared_project(), false);
    harness.script.recover_by(RecoveryPolicy::Inspect);
    harness
        .script
        .push(unit("network"), DriverOperation::Reconcile, &Scripted::Uncertain, 1);
    // An execution would fail instead of staying uncertain.
    let failure = Scripted::Failed {
        category: FailureCategory::ToolError,
        retryable: true,
    };
    harness
        .script
        .push(unit("network"), DriverOperation::Reconcile, &failure, 1);
    harness
        .script
        .push(unit("network"), DriverOperation::Inspect, &Scripted::Uncertain, 1);
    harness.init();
    let first = harness.reconcile(&["network"]);
    let second = harness.reconcile(&["network"]);
    assert_eq!(second.exit.code(), 4, "{second:#?}");
    assert_eq!(outcome(&second, "uncertain"), set(&["network"]));
    assert!(second.commits.is_empty(), "{second:#?}");
    let snapshot = harness.snapshot();
    assert_eq!(uncertain_run(snapshot.observed.get(&unit("network"))), first.run);
}

/// A run cancelled while a unit runs starts nothing more, keeps the
/// checkpoint of the unit that finished, gives the unit still running an uncertain condition after
/// its tool got the signal, keeps its run ref marked finished, releases the
/// lease, writes no transition commit, and exits 4 with
/// `NYL-RUN-CANCELLED`; the next run imports the run ref as after a crash.
#[test]
fn test_cancelled_run_checkpoints_marks_running_units_uncertain_and_skips_the_commit() {
    let markers = TempDir::new().unwrap();
    let started = markers.path().join("started");
    // Forward slashes: `sh` on Windows reads backslashes as escapes.
    let marker = started.display().to_string().replace('\\', "/");
    let slow = format!("[ -e \"{marker}\" ] && exit 0; touch \"{marker}\"; sleep 120");
    let files = [
        ("nyl/nyl.toml", "[project]\n".to_owned()),
        ("nyl/environment.yaml", ENVIRONMENT.to_owned()),
        ("nyl/quick.yaml", command("quick", "true", "")),
        // After quick, so quick's checkpoint never races the signal.
        (
            "nyl/slow.yaml",
            command("slow", &slow, "  idempotent: true\n  dependsOn: [quick]\n"),
        ),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(path, content)| (*path, content.as_str())).collect();
    let project = project(&files);
    let harness = Harness::new(project.path(), false);
    harness.init();
    let tips = harness.snapshot().tips;

    let cancellation = Cancellation::new();
    let signal = cancellation.clone();
    let watcher = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !started.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        signal.cancel(run::SIGTERM);
    });
    let begun = std::time::Instant::now();
    let cancelled = harness.reconcile_with(&[], cancellation);
    watcher.join().unwrap();
    // Unix forwards the signal to the tool's group; Windows kills the tool
    // once the 30s grace passes.
    let bound = if cfg!(unix) { 25 } else { 60 };
    assert!(
        begun.elapsed() < std::time::Duration::from_secs(bound),
        "the tool was stopped"
    );
    assert_eq!(cancelled.exit.code(), 4, "{cancelled:#?}");
    assert_eq!(codes(&cancelled.errors), [Code::RunCancelled], "{cancelled:#?}");
    assert!(cancelled.commits.is_empty());
    assert_eq!(harness.snapshot().tips, tips, "no transition commit");
    let coordination = harness.coordination();
    assert!(coordination.lease.is_none(), "the lease is released");
    let [kept] = coordination.runs.as_slice() else {
        panic!("the run ref is kept: {coordination:#?}");
    };
    assert!(kept.run.finished && kept.run.units.is_empty());
    let checkpoint = |name: &str| {
        kept.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.desired.unit.name == unit(name))
            .unwrap_or_else(|| panic!("{name} is checkpointed"))
    };
    assert!(checkpoint("quick").observed.receipt.is_some());
    assert_eq!(uncertain_run(Some(&checkpoint("slow").observed)), cancelled.run.clone());

    // The next run imports both; slow converges.
    let next = harness.reconcile(&[]);
    assert_eq!(next.exit.code(), 0, "{next:#?}");
    assert_eq!(names(&next.imported), set(&["quick", "slow"]));
    assert_eq!(outcome(&next, "recovered"), set(&["slow"]));
    assert!(harness.coordination().runs.is_empty());
}

/// Decision 3: a `--local` run executes a unit whose recovery policy is not
/// `converge` only when `--approve` names it; otherwise the unit awaits
/// approval and the run exits 2. The executed unit's receipt and the
/// local transition commit's summary record the `local` approval with the
/// commit identity.
#[test]
fn test_local_run_executes_a_unit_that_does_not_converge_only_when_approved() {
    let files = [
        ("nyl/nyl.toml", "[project]\n".to_owned()),
        ("nyl/environment.yaml", ENVIRONMENT.to_owned()),
        ("nyl/manual.yaml", command("manual", "true", "")),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(path, content)| (*path, content.as_str())).collect();
    let project = project(&files);
    let harness = Harness::new(project.path(), false);
    harness.init();
    let local = |approve: &[&str]| LocalRun {
        lease: LeaseScope::Local,
        approve: approve.iter().map(|name| unit(name)).collect(),
    };

    let mut job = harness.job(&[], Cancellation::default());
    job.request.local = Some(local(&[]));
    let waiting = harness.run(&job).environments.remove(0);
    assert_eq!(waiting.exit.code(), 2, "{waiting:#?}");
    assert_eq!(outcome(&waiting, "awaiting-approval"), set(&["manual"]));

    let mut job = harness.job(&[], Cancellation::default());
    job.request.local = Some(local(&["manual"]));
    let approved = harness.run(&job).environments.remove(0);
    assert_eq!(approved.exit.code(), 0, "{approved:#?}");
    assert_eq!(outcome(&approved, "executed"), set(&["manual"]));
    let expected = ApprovalRecord {
        by: vec!["Scenario <scenario@example.invalid>".to_owned()],
        identity: ApprovalIdentity::Local,
        source: Some("local".to_owned()),
        digest: None,
        reviewed: false,
    };
    let snapshot = harness.snapshot();
    let receipt = snapshot.observed[&unit("manual")].receipt.as_ref().unwrap();
    assert_eq!(receipt.approval.as_ref(), Some(&expected));
    let log = harness.store.log(&harness.location, StateSide::Observed, 1).unwrap();
    let line = log[0]
        .message
        .lines()
        .find(|line| line.trim_start().starts_with("manual:"))
        .expect("the summary lists manual");
    assert!(
        line.contains("approval") && line.contains("Scenario <scenario@example.invalid>") && line.contains("local"),
        "{line}"
    );
}
