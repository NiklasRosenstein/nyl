//! `plan`, `reconcile`, `status`, and `get -e` of the orchestration shell,
//! through its API, in the scenario test world: real Git source resolution
//! and unit rendering, the built-in Command kind and the fake kinds, and
//! the in-memory or Git state store.

mod scenario;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::driver::{DriverOperation, DriverSet};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, CommitIdentity, Committed, Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry,
    SourceRenderer, SourceRepository, SourceResolver, StateError, StateResult, StateSide, StateStore, StateTips,
    Transition,
};
use nyl_core::orchestration::state::{DesiredUnit, FailureCategory, Lease, Run, Signal, StateLocation};
use nyl_drivers::fake::{Script, Scripted};
use nyl_drivers::process::ProcessRunner;
use nyl_git::GitAuth;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::run_report::{EnvironmentRunReport, RunReport, UnitResult};
use nyl_orchestration::shell::inspect::{self, GetValue};
use nyl_orchestration::shell::run::{self, RunContext, RunRequest};
use nyl_orchestration::shell::state::{self as state_ops, Requested};
use nyl_orchestration::shell::{Context, Settings};
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_render::gitops::units::UnitSourceRenderer;
use nyl_state::git::{CommitAuthor, GitStateStore};
use nyl_state::memory::MemoryStateStore;
use nyl_state::source::GitSourceResolver;
use scenario::format::{BranchStep, CommitStep};
use scenario::world::{git_output, World};
use tempfile::TempDir;

const DESIRED_REF: &str = "nyl/dev/desired";
const OBSERVED_REF: &str = "nyl/dev/observed";

fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

fn unit(name: &str) -> UnitName {
    UnitName::new(name).unwrap()
}

fn start() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
        .unwrap()
        .to_utc()
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

const NYL_TOML: (&str, &str) = ("nyl/nyl.toml", "[project]\n");

fn environment(extra: &str) -> String {
    format!(
        "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: dev\n  labels: {{tier: dev}}\nspec:\n  unitSelector:\n    matchLabels: {{dev: 'true'}}\n{extra}"
    )
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
    tools: ProcessRunner,
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
        let clock = Arc::new(FakeClock::new(start()));
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
            desired_ref: DESIRED_REF.to_owned(),
            observed_ref: OBSERVED_REF.to_owned(),
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
            tools: ProcessRunner::new(),
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
            faults: &NoFaults,
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
    fn job(&self, units: &[&str]) -> Job {
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
            },
            workspace: TempDir::new().unwrap(),
        }
    }

    fn run(&self, job: &Job, identity: bool, operation: fn(&RunContext<'_>, &RunRequest) -> RunReport) -> RunReport {
        let base = self.context();
        let context = RunContext {
            base: &base,
            sources: &job.sources,
            renderer: &job.renderer,
            drivers: &self.drivers,
            tools: &self.tools,
            workspace: job.workspace.path(),
            process_env: &self.process_env,
            commit_identity: identity.then_some(&self.identity),
        };
        operation(&context, &job.request)
    }

    fn reconcile(&self, units: &[&str]) -> EnvironmentRunReport {
        let job = self.job(units);
        let report = self.run(&job, true, run::reconcile);
        report.environments.into_iter().next().unwrap()
    }

    fn plan(&self, units: &[&str]) -> EnvironmentRunReport {
        let job = self.job(units);
        let report = self.run(&job, false, run::plan);
        report.environments.into_iter().next().unwrap()
    }

    /// The commits on each state ref, read from the store.
    fn tips(&self) -> nyl_core::orchestration::ports::StateTips {
        self.store.snapshot(&self.location).unwrap().tips
    }

    fn lease_released(&self) -> bool {
        let coordination = self.store.coordination(&self.location, &dev()).unwrap();
        coordination.lease.is_none() && coordination.runs.is_empty()
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

fn codes(findings: &[nyl_core::orchestration::codes::Finding]) -> Vec<Code> {
    findings.iter().map(|finding| finding.code).collect()
}

/// The dependency wave of the walkthrough against the Git store: a plan
/// shows the blocked units and writes nothing, a reconcile runs every unit
/// in waves in one commit per ref, and the next reconcile, whose
/// provenance cites the first run, changes nothing and writes nothing. Each
/// run releases its lease, so the next one takes it.
#[test]
fn test_dependency_wave_runs_once_then_changes_nothing() {
    let harness = Harness::new(&shared_project(), true);
    harness.init();
    let initial = harness.tips();

    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 2, "{plan:#?}");
    assert_eq!(outcome(&plan, "blocked"), set(&["database", "kubernetes"]));
    assert_eq!(outcome(&plan, "ready"), set(&["network", "web-image"]));
    assert_eq!(harness.tips(), initial, "a plan writes nothing");

    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    assert_eq!(
        outcome(&first, "executed"),
        set(&["network", "web-image", "database", "kubernetes"])
    );
    assert_eq!(first.commits.len(), 2, "{first:#?}");
    assert!(harness.lease_released());
    assert!(harness
        .world
        .files("deploy")
        .iter()
        .any(|file| file == "dev/_nyl/index.json"));
    let after_first = harness.tips();

    let second = harness.reconcile(&[]);
    assert_eq!(second.exit.code(), 0, "{second:#?}");
    assert_eq!(
        outcome(&second, "current"),
        set(&["network", "web-image", "database", "kubernetes"])
    );
    assert!(second.commits.is_empty(), "{second:#?}");
    assert_eq!(harness.tips(), after_first);
    assert!(harness.lease_released());

    // The no-op run released its lease: the next run takes it.
    let third = harness.reconcile(&[]);
    assert_eq!(third.exit.code(), 0, "{third:#?}");

    let status = inspect::status(&harness.context(), &dev(), &harness.location);
    assert_eq!(status.exit().code(), 0, "{status:#?}");

    let snapshot = inspect::snapshot(harness.store.as_ref(), &harness.location, None).unwrap();
    let host: UnitInputSource =
        serde_json::from_value(serde_json::json!({"unit": "database", "output": "host"})).unwrap();
    assert!(matches!(
        inspect::get_value(&dev(), &snapshot, &host, false).unwrap(),
        GetValue::Value(serde_json::Value::String(_))
    ));
    // An earlier state revision reads the state before the run.
    let before = inspect::snapshot(harness.store.as_ref(), &harness.location, Some(&initial)).unwrap();
    assert!(before.desired.is_empty() && before.record.is_some());
}

/// A failed producer fails the run (exit 3) and blocks its consumer; the
/// next reconcile retries it and runs the consumer in a later wave.
#[test]
fn test_retryable_failure_is_retried_and_unblocks_its_dependent() {
    let harness = Harness::new(&shared_project(), false);
    harness.init();
    harness.script.push(
        unit("web-image"),
        DriverOperation::Reconcile,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: true,
        },
        1,
    );
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 3, "{first:#?}");
    assert_eq!(outcome(&first, "executed"), set(&["network", "database"]));
    assert_eq!(outcome(&first, "failed"), set(&["web-image"]));
    assert_eq!(outcome(&first, "blocked"), set(&["kubernetes"]));

    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 2, "{plan:#?}");
    assert_eq!(outcome(&plan, "ready"), set(&["web-image"]));

    let second = harness.reconcile(&[]);
    assert_eq!(second.exit.code(), 0, "{second:#?}");
    assert_eq!(outcome(&second, "executed"), set(&["web-image", "kubernetes"]));
    assert_eq!(outcome(&second, "current"), set(&["network", "database"]));
}

/// `--unit` executes only the named unit, blocked until its dependencies
/// have current receipts, and reports only it; once every unit ran in its
/// own invocation, unit-scoped and whole runs change nothing.
#[test]
fn test_unit_scoped_runs_execute_only_the_named_unit() {
    let harness = Harness::new(&shared_project(), false);
    harness.init();
    let blocked = harness.reconcile(&["database"]);
    assert_eq!(blocked.exit.code(), 2, "{blocked:#?}");
    assert_eq!(outcome(&blocked, "blocked"), set(&["database"]));
    assert_eq!(blocked.units.len(), 1);
    for name in ["network", "web-image", "database", "kubernetes"] {
        let report = harness.reconcile(&[name]);
        assert_eq!(report.exit.code(), 0, "{name}: {report:#?}");
        assert_eq!(outcome(&report, "executed"), set(&[name]));
    }
    let tips = harness.tips();
    let again = harness.reconcile(&["database"]);
    assert_eq!(outcome(&again, "current"), set(&["database"]));
    let whole = harness.reconcile(&[]);
    assert_eq!(whole.exit.code(), 0, "{whole:#?}");
    assert_eq!(
        outcome(&whole, "current"),
        set(&["network", "web-image", "database", "kubernetes"])
    );
    assert_eq!(harness.tips(), tips, "runs that change nothing write nothing");
}

fn command_project() -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment("  allowUnprotectedSource: true\n")),
        (
            "nyl/units.yaml",
            "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: producer\n  labels: {dev: 'true'}\nspec:\n  command: [sh, -c, 'printf ''{\"vpcId\": \"vpc-1\", \"extra\": 1}'' > \"$NYL_OUTPUTS\"']\n  outputs:\n    vpcId: {type: string}\n",
        ),
    ])
}

/// A Command unit's undeclared output is reported with
/// `NYL-OUTPUT-UNDECLARED` and left out of its receipt.
#[test]
fn test_undeclared_command_output_is_warned_about_and_not_recorded() {
    let project = command_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    let producer = &report.units[&unit("producer")];
    assert!(matches!(producer.result, Some(UnitResult::Executed { .. })));
    assert_eq!(codes(&producer.warnings), vec![Code::OutputUndeclared]);
    assert!(producer.warnings[0].message.contains("extra"));
    let snapshot = harness.store.snapshot(&harness.location).unwrap();
    let receipt = snapshot.observed[&unit("producer")].receipt.as_ref().unwrap();
    assert_eq!(receipt.outputs, serde_json::json!({"vpcId": "vpc-1"}));
}

/// `--unit` naming no unit of the ownership set is an error, exit 1, and
/// writes nothing; a plan needs no commit identity and writes nothing.
#[test]
fn test_unknown_unit_fails_and_plan_writes_nothing() {
    let project = command_project();
    let harness = Harness::new(project.path(), false);
    harness.init();
    let before = harness.tips();

    let unknown = harness.reconcile(&["missing"]);
    assert_eq!(unknown.exit.code(), 1, "{unknown:#?}");
    assert_eq!(codes(&unknown.errors), vec![Code::UnitUnknown]);
    assert!(harness.lease_released());

    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 0, "{plan:#?}");
    assert_eq!(outcome(&plan, "ready"), set(&["producer"]));
    assert_eq!(harness.tips(), before);
    assert!(harness.lease_released());
}

fn infra_project(source: &str, environment_extra: &str) -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment(environment_extra)),
        (
            "nyl/units.yaml",
            &format!(
                "apiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: network\n  labels: {{dev: 'true'}}\nspec:\n  source: {source}\n  backend: {{path: '%SCENARIO_TMP%/network.tfstate'}}\n"
            ),
        ),
        ("infra/main.tf", "# v1\n"),
    ])
}

/// Copy `files` into a new directory beneath `root`, for a commit step.
fn apply(root: &Path, name: &str, files: &[(&str, &str)]) -> String {
    for (path, content) in files {
        let target = root.join(name).join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
    name.to_owned()
}

/// A unit whose source names a revision executes from that revision's
/// files: a change there alone changes its execution key.
#[test]
fn test_change_on_a_pinned_revision_changes_the_key() {
    let project = infra_project("{path: ../infra, revision: pinned}", "  allowUnprotectedSource: true\n");
    let harness = Harness::new(project.path(), false);
    let steps = TempDir::new().unwrap();
    harness.world.branch(&BranchStep {
        name: "pinned".to_owned(),
        from: "main".to_owned(),
    });
    harness.init();
    let executed_key = |report: &EnvironmentRunReport| match &report.units[&unit("network")].result {
        Some(UnitResult::Executed { execution_key }) => execution_key.clone(),
        other => panic!("network did not execute: {other:?}"),
    };
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 0, "{first:#?}");
    let first_key = executed_key(&first);

    harness.world.commit(
        &CommitStep {
            branch: "pinned".to_owned(),
            message: "Change the pinned source".to_owned(),
            apply: Some(apply(steps.path(), "v2", &[("infra/main.tf", "# v2\n")])),
            delete: Vec::new(),
        },
        steps.path(),
    );
    let second = harness.reconcile(&[]);
    assert_eq!(second.exit.code(), 0, "{second:#?}");
    assert_ne!(executed_key(&second), first_key);
}

/// A unit's locked commit must be reachable from protectedRefs or its own
/// revision; allowUnprotectedSource waives that only for S.
#[test]
fn test_unreachable_unit_lock_fails_despite_allow_unprotected_source() {
    let project = infra_project("{path: ../infra}", "  allowUnprotectedSource: true\n");
    let harness = Harness::new(project.path(), false);
    let steps = TempDir::new().unwrap();
    harness.world.branch(&BranchStep {
        name: "other".to_owned(),
        from: "main".to_owned(),
    });
    harness.world.commit(
        &CommitStep {
            branch: "other".to_owned(),
            message: "Diverge".to_owned(),
            apply: Some(apply(steps.path(), "other", &[("infra/main.tf", "# other\n")])),
            delete: Vec::new(),
        },
        steps.path(),
    );
    let other = harness.world.rev_parse("other");
    let units = format!(
        "apiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: network\n  labels: {{dev: 'true'}}\nspec:\n  source: {{path: ../infra, revision: main, commit: {other}}}\n  backend: {{path: '%SCENARIO_TMP%/network.tfstate'}}\n"
    );
    harness.world.commit(
        &CommitStep {
            branch: "main".to_owned(),
            message: "Lock the source".to_owned(),
            apply: Some(apply(steps.path(), "lock", &[("nyl/units.yaml", &units)])),
            delete: Vec::new(),
        },
        steps.path(),
    );
    harness.init();
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 1, "{report:#?}");
    assert_eq!(codes(&report.errors), vec![Code::SourceUnreachable]);
    assert!(report.errors[0].message.contains(&other));
    assert!(report.commits.is_empty());
    assert!(harness.lease_released());
}

fn grouped_project(group_spec: &str) -> TempDir {
    project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment("")),
        (
            "nyl/group.yaml",
            &format!(
                "apiVersion: gitops.nyl/v1\nkind: EnvironmentGroup\nmetadata:\n  name: development\nspec:\n  environmentSelector:\n    matchLabels: {{tier: dev}}\n{group_spec}"
            ),
        ),
        (
            "nyl/units.yaml",
            "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: producer\n  labels: {dev: 'true'}\nspec:\n  command: [sh, -c, 'true']\n",
        ),
    ])
}

/// An EnvironmentGroup's protectedRefs and allowUnprotectedSource reach the
/// reachability check of S through the defaults-applied entry Environment.
#[test]
fn test_group_protection_defaults_reach_the_source_check() {
    let protected = grouped_project("  protectedRefs: [release]\n");
    let harness = Harness::new(protected.path(), false);
    harness.init();
    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 1, "{plan:#?}");
    assert_eq!(codes(&plan.errors), vec![Code::SourceUnreachable]);
    assert!(plan.errors[0].message.contains("release"));

    let waived = grouped_project("  protectedRefs: [release]\n  allowUnprotectedSource: true\n");
    let harness = Harness::new(waived.path(), false);
    harness.init();
    let plan = harness.plan(&[]);
    assert_eq!(plan.exit.code(), 0, "{plan:#?}");
}

/// Discovery of a project works in a worktree the source resolver checks
/// out, which is a Git repository of its own borrowing the clone's objects.
#[test]
fn test_discovery_works_in_a_source_resolver_worktree() {
    let world = World::new(&shared_project());
    let clone = world.fresh_clone("main");
    let resolver = GitSourceResolver::new(clone.join(".git"), GitAuth::none());
    let head = resolver.resolve_revision(&SourceRepository::This, "main").unwrap();
    let directory = TempDir::new().unwrap();
    let worktree = directory.path().join("worktree");
    resolver.worktree(&SourceRepository::This, &head, &worktree).unwrap();
    let inventory = nyl::gitops::discover_gitops_inventory(&worktree.join("nyl"), None).unwrap();
    let units = nyl::gitops::units::discover_units(&inventory).unwrap();
    assert_eq!(units.len(), 4);
}

/// A memory store whose `update_lease` fails with an unavailable store
/// from its `fail_from`-th call on, as a network error would.
struct FlakyLeaseStore {
    inner: MemoryStateStore,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    fail_from: usize,
}

impl StateStore for FlakyLeaseStore {
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
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if call >= self.fail_from {
            return Err(StateError::Unavailable("push failed: connection reset".to_owned()));
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
        self.inner.write_run(location, environment, run, checkpoint)
    }
    fn delete_run(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        self.inner.delete_run(location, environment, run)
    }
    fn retire_run(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        self.inner.retire_run(location, environment, run)
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

/// A store failure while taking the lease for a later wave, after an
/// earlier wave's units ran and were checkpointed, keeps the run ref with
/// their checkpoints, so the next run imports them instead of executing
/// them again; the lease is released.
#[test]
fn test_store_failure_after_a_checkpoint_keeps_the_run_ref() {
    let mut harness = Harness::new(&shared_project(), false);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    harness.store = Box::new(FlakyLeaseStore {
        inner: MemoryStateStore::new(),
        calls: calls.clone(),
        fail_from: 3,
    });
    harness.init();
    // The renewals before the first resolution and the first wave succeed;
    // the renewal before re-resolving for the second wave fails.
    calls.store(0, std::sync::atomic::Ordering::SeqCst);
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 1, "{report:#?}");
    assert_eq!(codes(&report.errors), vec![Code::StateUnavailable], "{report:#?}");
    let coordination = harness.store.coordination(&harness.location, &dev()).unwrap();
    assert!(coordination.lease.is_none(), "the lease is released");
    let [kept] = coordination.runs.as_slice() else {
        panic!("the run ref is kept: {coordination:#?}");
    };
    assert!(kept.run.finished);
    let checkpointed: BTreeSet<String> = kept
        .checkpoints
        .iter()
        .map(|checkpoint| checkpoint.desired.unit.name.to_string())
        .collect();
    assert_eq!(checkpointed, set(&["network", "web-image"]));
}
