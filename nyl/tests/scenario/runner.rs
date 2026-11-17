//! The tier-1 runner of `design/reference-scenarios.md#tiers`: runs one
//! scenario in-process, in a test world of its own, with a fake clock,
//! sequential identifiers, the fake kinds driven by the scenario's `driver`
//! steps, a one-shot fault slot, and a fixed commit identity.
//!
//! Each `nyl` step parses its arguments with the real command line and runs
//! [`Cli::execute_in_process`] in a fresh clone, so the walkthroughs exercise
//! the same path as the binary. Fresh clones carry no Git identity, so a run
//! that read the host's would not be reproducible; the identity comes from
//! the wiring instead.
//!
//! A `nyl` step with `pause` runs on a thread of its own whose fault
//! injector blocks the first time the invocation reaches the pause point,
//! so later steps run while it waits; its `resume` step lets it continue,
//! joins it, and checks it like any `nyl` step.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{Scope, ScopedJoinHandle};

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::driver::{DriverOperation, DriverSet};
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::{Clock, CommitIdentity};
use nyl_core::orchestration::state::{DesiredUnit, FailureCategory, ObservedUnit};
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint};
use nyl_orchestration::rules::unit_state::receipt_state;
use nyl_orchestration::run_report::ReceiptState;
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_render::gitops::observation::ReconcilerFacts;
use nyl_units::fake::{Script, Scripted, ScriptedDeployment, ScriptedReading};
use nyl_units::kubernetes_publication::reconciler::{Health, Running};
use nyl_units::process::ProcessRunner;
use serde_json::Value;

use super::format::{
    placeholders, ApplicationHealth, ApplicationReading, ArgocdStep, DeployExpectation,
    DriverOperation as StepOperation, DriverOutcome, EvidenceLevel, Expect, KeepRefsExpectation, Mention, Placeholder,
    PointName, PromotionExpectation, ReceiptExpectation, RecordLocation, Step, StepKind, UidExpectation,
    UnitExpectation,
};
use super::world::{git_output, World};
use super::Loaded;

/// The `runs` of an `argocd` reading that names no publication.
const UNMATCHED: &str = "unmatched";

/// The time every scenario starts at.
pub const START: &str = "2026-09-24T10:00:00Z";

/// The commit identity of every state and publication commit.
pub const AUTHOR: &str = "Scenario <scenario@example.invalid>";

/// How long a paused invocation may take to reach its pause point before
/// the scenario fails instead of hanging the test.
const REACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The outcome keys of `expect` that name sets of units: each a
/// `UnitResult::name` or `UnitState::name`, as `UnitReport::outcome_name`
/// gives them.
pub const OUTCOME_KEYS: [&str; 14] = [
    "executed",
    "current",
    "ready",
    "blocked",
    "failed",
    "uncertain",
    "awaiting-approval",
    "pending-teardown",
    "tearing-down",
    "retained",
    "removed",
    "released",
    "held",
    "drifted",
];

/// A fault the next `nyl` step dies at, consumed when it fires. A `None`
/// unit matches only a point the shell consults without a unit.
#[derive(Default)]
pub struct FaultSlot {
    armed: Mutex<Option<(FaultPoint, Option<UnitName>)>>,
}

impl FaultSlot {
    /// A slot that kills once at `point` for `unit`, or for the unit-less call.
    pub fn kill_once(point: FaultPoint, unit: Option<&str>) -> Self {
        Self {
            armed: Mutex::new(Some((point, unit.map(|unit| UnitName::new(unit).unwrap())))),
        }
    }

    fn is_armed(&self) -> bool {
        self.armed.lock().expect("fault lock").is_some()
    }
}

impl FaultInjector for FaultSlot {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        let mut armed = self.armed.lock().expect("fault lock");
        match armed.as_ref() {
            Some((armed_point, armed_unit)) if *armed_point == point && armed_unit.as_ref() == unit => {
                *armed = None;
                FaultAction::Kill
            }
            _ => FaultAction::Continue,
        }
    }
}

/// The injector of a paused invocation: the first time the invocation
/// reaches the pause point (for the given unit, when one is given), it
/// signals `reached` and blocks until resumed; every point then consults
/// the shared fault slot, so a pause point that is a fault point can also
/// kill. Dropping the resume sender lets a blocked invocation continue.
struct Pausing<'s> {
    shared: &'s FaultSlot,
    point: FaultPoint,
    unit: Option<UnitName>,
    reached: Mutex<Option<mpsc::Sender<()>>>,
    resume: Mutex<mpsc::Receiver<()>>,
}

impl FaultInjector for Pausing<'_> {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        let matches = point == self.point && self.unit.as_ref().is_none_or(|paused| Some(paused) == unit);
        if matches {
            let reached = self.reached.lock().expect("pause lock").take();
            if let Some(reached) = reached {
                // The scenario thread may have given up waiting; continue then.
                let _ = reached.send(());
                drop(reached);
                let _ = self.resume.lock().expect("pause lock").recv();
            }
        }
        self.shared.at(point, unit)
    }
}

/// What every invocation of one scenario shares.
struct Shared {
    world: World,
    clock: Arc<FakeClock>,
    ids: SequentialIds,
    script: Script,
    drivers: DriverSet,
    tools: ProcessRunner,
    faults: FaultSlot,
    author: CommitIdentity,
    process_env: BTreeMap<String, String>,
    tier: u8,
}

impl Shared {
    /// The wiring of one invocation with `faults` as its injector.
    fn wiring<'a>(&'a self, faults: &'a dyn FaultInjector) -> Wiring<'a> {
        Wiring {
            clock: self.clock.clone(),
            ids: &self.ids,
            drivers: &self.drivers,
            tools: &self.tools,
            faults,
            runner: "scenario".to_owned(),
            author: Some(self.author.clone()),
            process_env: self.process_env.clone(),
            cancellation: nyl_orchestration::shell::run::Cancellation::default(),
            prompt: None,
            progress: None,
        }
    }
}

/// Run every step of `loaded`, returning the first failed check.
pub fn run(loaded: &Loaded) -> Result<(), String> {
    run_tier(loaded, 1, Publication::Fake)
}

/// Which publication driver tier 1 runs against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    /// `FakePublication`, which scenarios can script and observe.
    Fake,
    /// The real `KubernetesPublication` driver.
    Real,
}

/// [`run`] with the real `KubernetesPublication` driver.
pub fn run_real_publication(loaded: &Loaded) -> Result<(), String> {
    run_tier(loaded, 1, Publication::Real)
}

/// Run the same scenario and assertions through separate binary processes.
#[cfg(feature = "tier2")]
pub fn run_binary(loaded: &Loaded) -> Result<(), String> {
    run_tier(loaded, 2, Publication::Fake)
}

fn run_tier(loaded: &Loaded, tier: u8, publication: Publication) -> Result<(), String> {
    // The guard sits where scenarios execute, so no runner can run one that
    // the harness has not reached or that cannot pass yet.
    if loaded.scenario.milestone > super::REACHED {
        return Err(format!(
            "{} is above the reached milestone {}",
            loaded.name,
            super::REACHED
        ));
    }
    if let Some((_, reason)) = super::PENDING.iter().find(|(name, _)| *name == loaded.name) {
        return Err(format!("{} is pending and runs in no runner: {reason}", loaded.name));
    }
    let start = chrono::DateTime::parse_from_rfc3339(START)
        .expect("start time")
        .to_utc();
    let script = Script::new();
    #[cfg(feature = "tier2")]
    let registry = if tier == 2 {
        Some(super::native::Registry::new()?)
    } else {
        None
    };
    #[cfg(feature = "tier2")]
    let world = World::with_registry(
        &loaded.directory.join(&loaded.scenario.project),
        registry
            .as_ref()
            .map_or(super::world::FAKE_REGISTRY, |registry| registry.address.as_str()),
    );
    #[cfg(not(feature = "tier2"))]
    let world = World::new(&loaded.directory.join(&loaded.scenario.project));
    // Command units admit `SCENARIO_TMP` through `env.passthrough` to reach
    // the files `file` steps change outside Nyl.
    let mut process_env = super::shell::process_env();
    process_env.insert("SCENARIO_TMP".to_owned(), world.scenario_tmp_for_scripts());
    // A unit that declares `observe: {}` admits `KUBECONFIG` for `nyl attest`, and an
    // admitted variable must exist wherever the unit runs. No scenario reads
    // Argo CD, so the file it names does not.
    process_env.insert(
        "KUBECONFIG".to_owned(),
        format!("{}/no-kubeconfig", world.scenario_tmp_for_scripts()),
    );
    let shared = Shared {
        world,
        clock: Arc::new(FakeClock::new(start)),
        ids: SequentialIds::new(),
        drivers: match publication {
            Publication::Fake => nyl_units::registry::with_test_kinds(&script, nyl::resource_catalogue()),
            Publication::Real => nyl_units::registry::with_real_publication(&script, nyl::resource_catalogue()),
        },
        script,
        tools: ProcessRunner::new(),
        faults: FaultSlot::default(),
        author: AUTHOR.parse().expect("identity"),
        process_env,
        tier,
    };
    std::thread::scope(|scope| {
        let mut state = RunState {
            world: &shared.world,
            shared: &shared,
            uids: BTreeMap::new(),
            written: BTreeMap::new(),
            paused: BTreeMap::new(),
            writes: BTreeMap::new(),
        };
        let mut result = loaded.scenario.steps.iter().enumerate().try_for_each(|(index, step)| {
            state
                .step(scope, loaded, step)
                .map_err(|error| format!("{}: step {}: {error}", loaded.name, index + 1))
        });
        // Let every invocation still paused finish, so none outlives the
        // scenario blocked and a panic in one is reported, not rethrown.
        let unresumed = std::mem::take(&mut state.paused);
        for (id, paused) in unresumed {
            drop(paused.resume);
            let _ = paused.handle.join();
            if result.is_ok() {
                result = Err(format!("{}: {id} was paused and never resumed", loaded.name));
            }
        }
        result
    })
}

/// Commit counts on an environment's desired and observed refs.
type Counts = (usize, usize);

/// What a step's checks compare against, taken before its invocation.
struct Before {
    /// The step's arguments with their placeholders replaced.
    args: Vec<String>,
    /// The environment whose refs `commits` counts: the step's `-e`, else
    /// dev's, and for `promote` the environment its path promotes into.
    environment: String,
    /// For `promote`: the path it names, or why that could not be read.
    promote: Option<Result<PromotionPath, String>>,
    /// For a `promote` step that checks `commits`: every state and project ref
    /// before it ran, and the refs the step may change.
    state_refs: Option<BTreeMap<String, String>>,
    excused_refs: Vec<String>,
    /// The step's `expect` with its placeholders replaced before it ran.
    expect: Option<Expect>,
    /// Notes added to a failed `promotion` check.
    hints: Vec<String>,
    counts: Counts,
    /// The commits other steps had written to the environment's refs.
    written: Counts,
    deploy: Option<BTreeMap<String, String>>,
}

/// A paused invocation awaiting its `resume` step.
struct Paused<'scope, 'l> {
    step: &'l Step,
    handle: ScopedJoinHandle<'scope, Result<CommandOutcome, String>>,
    resume: mpsc::Sender<()>,
    before: Before,
}

struct RunState<'s, 'scope, 'l> {
    world: &'s World,
    shared: &'s Shared,
    /// Each unit's uid as the last `units` check saw it.
    uids: BTreeMap<String, Option<String>>,
    /// Per environment, the state commits checked steps have written, so a
    /// resumed invocation's new commits exclude those of steps that ran
    /// while it was paused.
    written: BTreeMap<String, Counts>,
    paused: BTreeMap<String, Paused<'scope, 'l>>,
    /// What each `nyl` step with an `id` wrote, for the placeholders and the
    /// `promotion` expectation.
    writes: BTreeMap<String, StepWrite>,
}

/// The state commits a `nyl` step with an `id` wrote, and the run that wrote them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepWrite {
    pub desired: Option<String>,
    pub observed: Option<String>,
    /// The `Nyl-Run-Id` trailer of those commits.
    pub run: Option<String>,
    /// The environment whose refs they are on.
    pub environment: String,
}

impl StepWrite {
    /// What `%STATE(<id>)%` is: the desired commit, else the observed one.
    pub fn state(&self) -> Option<&String> {
        self.desired.as_ref().or(self.observed.as_ref())
    }
}

impl<'s: 'scope, 'scope, 'l: 'scope> RunState<'s, 'scope, 'l> {
    fn step(&mut self, scope: &'scope Scope<'scope, '_>, loaded: &'l Loaded, step: &'l Step) -> Result<(), String> {
        let shared = self.shared;
        let kind = step.kind()?;
        if shared.tier == 2 && (step.tier == Some(1) || kind.tier_one_only() || step.pause.is_some()) {
            return Ok(());
        }
        match kind {
            StepKind::Commit => self
                .world
                .commit(step.commit.as_ref().expect("kind"), &loaded.directory),
            StepKind::Branch => self.world.branch(step.branch.as_ref().expect("kind")),
            StepKind::SquashMerge => self.world.squash_merge(step.squash_merge.as_ref().expect("kind")),
            StepKind::Merge => self.world.merge(step.merge.as_ref().expect("kind")),
            StepKind::DeleteBranch => self.world.delete_branch(step.delete_branch.as_ref().expect("kind")),
            StepKind::Ref => {
                let step = step.reference.as_ref().expect("kind");
                match (&step.delete, &step.set, &step.to) {
                    (Some(reference), _, _) => self.world.delete_ref(reference),
                    (_, Some(reference), Some(branch)) => self.world.set_ref(reference, branch),
                    _ => unreachable!("validated: delete, or set with to"),
                }
            }
            StepKind::File => self.world.file(step.file.as_ref().expect("kind"))?,
            StepKind::Clock => {
                let step = step.clock.as_ref().expect("kind");
                if let Some(set) = &step.set {
                    let time = chrono::DateTime::parse_from_rfc3339(set)
                        .map_err(|error| format!("clock set {set}: {error}"))?;
                    shared.clock.set(time.to_utc());
                }
                if let Some(advance) = &step.advance {
                    let duration = advance
                        .parse::<Duration>()
                        .map_err(|error| format!("clock advance {advance}: {error}"))?;
                    shared.clock.advance(duration);
                }
            }
            StepKind::Driver => {
                let step = step.driver.as_ref().expect("kind");
                let unit = UnitName::new(&step.unit).map_err(|error| error.to_string())?;
                let operation = match step.operation {
                    StepOperation::Plan => DriverOperation::Plan,
                    StepOperation::Reconcile => DriverOperation::Reconcile,
                    StepOperation::DetectDrift => DriverOperation::DetectDrift,
                    StepOperation::Teardown => DriverOperation::Teardown,
                    StepOperation::Observe => DriverOperation::Observe,
                    StepOperation::VerifyArtifact => DriverOperation::VerifyArtifact,
                };
                // An observation that could not be read, or an artifact that is gone, has a reason, not a category.
                let uncategorized = matches!(step.operation, StepOperation::Observe | StepOperation::VerifyArtifact);
                let result = match step.outcome {
                    DriverOutcome::Succeeded => Scripted::Succeeded {
                        outputs: step.outputs.clone(),
                    },
                    DriverOutcome::Uncertain => Scripted::Uncertain,
                    DriverOutcome::Drifted => Scripted::Drifted,
                    DriverOutcome::Pending => Scripted::Pending,
                    // An observation that could not be read has a reason, not a category.
                    DriverOutcome::Failed if uncategorized => Scripted::Failed {
                        category: FailureCategory::ToolError,
                        retryable: false,
                    },
                    DriverOutcome::Failed => Scripted::Failed {
                        category: serde_json::from_value::<FailureCategory>(Value::String(
                            step.category.clone().unwrap_or_default(),
                        ))
                        .map_err(|error| format!("category: {error}"))?,
                        retryable: step.retryable.unwrap_or(false),
                    },
                };
                shared.script.push(unit, operation, &result, step.times.unwrap_or(1));
            }
            StepKind::Fault => {
                let step = step.fault.as_ref().expect("kind");
                let point = match step.point {
                    PointName::Executing | PointName::WaitingLease => {
                        return Err(format!("fault {:?} only pauses", step.point));
                    }
                    point => point.fault_point(),
                };
                let unit = step
                    .unit
                    .as_deref()
                    .map(UnitName::new)
                    .transpose()
                    .map_err(|error| error.to_string())?;
                *shared.faults.armed.lock().expect("fault lock") = Some((point, unit));
            }
            StepKind::Nyl if step.pause.is_some() => self.pause(scope, loaded, step)?,
            StepKind::Nyl => {
                let args = step.nyl.as_ref().expect("kind");
                let branch = step.at.as_deref().unwrap_or("main");
                let before = self.before(args, step.expect.as_ref(), branch)?;
                let outcome = self.invoke(&before.args, branch)?;
                self.check(step, step, outcome, &before)?;
            }
            StepKind::Resume => {
                let id = step.resume.as_ref().expect("kind");
                let paused = self
                    .paused
                    .remove(id)
                    .ok_or_else(|| format!("resume {id}: no invocation with that id is paused"))?;
                // The invocation may have ended at a kill; it then needs no signal.
                let _ = paused.resume.send(());
                let outcome = paused
                    .handle
                    .join()
                    .map_err(|_| format!("resume {id}: the invocation panicked"))??;
                self.check(paused.step, step, outcome, &paused.before)?;
            }
            StepKind::Argocd => self.argocd(step.argocd.as_ref().expect("kind"))?,
            kind @ (StepKind::CiAdmit | StepKind::CiWorker | StepKind::CiExport) => {
                return Err(format!("tier 1 does not run {kind:?} steps yet"));
            }
        }
        Ok(())
    }

    /// Start a paused `nyl` step on its own thread and return once it
    /// reached its pause point.
    fn pause(&mut self, scope: &'scope Scope<'scope, '_>, loaded: &'l Loaded, step: &'l Step) -> Result<(), String> {
        let shared = self.shared;
        let args = step.nyl.as_ref().expect("kind");
        let id = step.id.clone().expect("validated: a paused step has an id");
        let at = step.pause.as_ref().expect("paused");
        let point = at.point.fault_point();
        let unit = at
            .unit
            .as_deref()
            .map(UnitName::new)
            .transpose()
            .map_err(|error| error.to_string())?;
        let described = match &unit {
            Some(unit) => format!("{point} for {unit}"),
            None => point.to_string(),
        };
        // The resume step's expectations decide what to record before.
        let resume = loaded
            .scenario
            .steps
            .iter()
            .find(|candidate| candidate.resume.as_deref() == Some(id.as_str()));
        let branch = step.at.as_deref().unwrap_or("main");
        let before = self.before(args, resume.and_then(|step| step.expect.as_ref()), branch)?;
        let args = before.args.clone();
        let (reached_sender, reached) = mpsc::channel();
        let (resume_sender, resume_receiver) = mpsc::channel();
        let world = self.world;
        let handle = scope.spawn(move || {
            let pausing = Pausing {
                shared: &shared.faults,
                point,
                unit,
                reached: Mutex::new(Some(reached_sender)),
                resume: Mutex::new(resume_receiver),
            };
            let wiring = shared.wiring(&pausing);
            invoke(world, &wiring, &args, branch)
        });
        match reached.recv_timeout(REACH_TIMEOUT) {
            Ok(()) => {
                self.paused.insert(
                    id,
                    Paused {
                        step,
                        handle,
                        resume: resume_sender,
                        before,
                    },
                );
                Ok(())
            }
            // The injector is gone: the invocation ended without pausing.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let ended = match handle.join() {
                    Ok(Ok(outcome)) => format!("it exited {}; stderr:\n{}", outcome.exit, outcome.stderr),
                    Ok(Err(error)) => error,
                    Err(_) => "it panicked".to_owned(),
                };
                Err(format!("{id} never reached its pause point {described}: {ended}"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(resume_sender);
                Err(format!(
                    "{id} did not reach its pause point {described} within {REACH_TIMEOUT:?}"
                ))
            }
        }
    }

    /// What `check` compares a step's result against, taken before it runs,
    /// with the placeholders of its arguments replaced.
    fn before(&self, args: &[String], expect: Option<&Expect>, branch: &str) -> Result<Before, String> {
        let args = self.expand_all(args, branch)?;
        let promote = (args.first().map(String::as_str) == Some("promote")).then(|| self.promotion_path(&args, branch));
        // A `promote` step counts the refs of the environment its path
        // promotes into. A path that cannot be read counts nothing, and the
        // step must then leave every ref unchanged.
        let counted = counted_environment(&args, promote.as_ref());
        let environment = counted.clone().unwrap_or_else(|| environment_of(&args));
        let checks_commits = expect.is_some_and(|expect| expect.commits.is_some());
        let state_refs = (promote.is_some() && checks_commits).then(|| self.state_refs());
        // A dry run takes no lease and writes nothing, the target included.
        let dry_run = args.iter().any(|arg| arg == "--dry-run");
        let excused_refs = match (&promote, dry_run) {
            (Some(Ok(path)), false) => excused_refs(&path.to, path.record_source.then_some(branch)),
            _ => Vec::new(),
        };
        let mut hints = Vec::new();
        let expect = expect
            .map(|expect| self.expand_expect(expect, branch, &mut hints))
            .transpose()?;
        Ok(Before {
            counts: self.state_counts(&environment),
            written: self.written.get(&environment).copied().unwrap_or_default(),
            deploy: expect
                .as_ref()
                .and_then(|expect| expect.deploy.as_ref())
                .map(|deploy| self.tree(&deploy.branch)),
            environment,
            args,
            promote,
            state_refs,
            excused_refs,
            expect,
            hints,
        })
    }

    /// `expect` with the placeholders of every string a `nyl get` invocation
    /// or a comparison reads replaced, as the step starts. The revisions a
    /// `promotion` expectation names (`sourceCommit`, `superseded`) are not
    /// placeholders: they resolve after the step, when the commits exist.
    fn expand_expect(&self, expect: &Expect, branch: &str, hints: &mut Vec<String>) -> Result<Expect, String> {
        let mut expect = expect.clone();
        let text = |text: &mut String| -> Result<(), String> {
            *text = self.expand(text, branch)?;
            Ok(())
        };
        if let Some(get) = &mut expect.get {
            get.args = self.expand_all(&get.args, branch)?;
            if let Some(Value::String(value)) = &mut get.value {
                text(value)?;
            }
            for value in get.json.iter_mut().flat_map(|json| json.values_mut()) {
                if let Value::String(value) = value {
                    text(value)?;
                }
            }
        }
        for mention in expect
            .deploy
            .iter_mut()
            .flat_map(|deploy| deploy.mentions.values_mut().flatten())
        {
            if let Mention::Get(args) = mention {
                *args = self.expand_all(args, branch)?;
            }
        }
        if let Some(promotion) = &mut expect.promotion {
            if let Some(commit) = &mut promotion.from.desired_commit {
                // `%STATE%` of a step that wrote only observed commits is that
                // commit, which never equals a desired commit.
                for placeholder in placeholders(commit)? {
                    if let Placeholder::State(id) = placeholder {
                        if self.writes.get(&id).is_some_and(|write| write.desired.is_none()) {
                            hints.push(format!(
                                "%STATE({id})% fell back to the observed commit, because that step wrote no desired commit"
                            ));
                        }
                    }
                }
                text(commit)?;
            }
            if let Some(commit) = &mut promotion.from.observed_commit {
                text(commit)?;
            }
            for value in promotion.values.iter_mut().flat_map(|values| values.values_mut()) {
                if let Mention::Get(args) = &mut value.equals {
                    *args = self.expand_all(args, branch)?;
                }
            }
        }
        Ok(expect)
    }

    /// Check the result of the invocation of `invoked` against the `exit`,
    /// `killed`, and `expect` of `checked`: the same step for a `nyl` step,
    /// the `resume` step for a paused one.
    fn check(
        &mut self,
        invoked: &Step,
        checked: &Step,
        outcome: CommandOutcome,
        before: &Before,
    ) -> Result<(), String> {
        let args = &before.args;
        let branch = invoked.at.as_deref().unwrap_or("main");
        let environment = before.environment.as_str();
        // The invocation's own commits: the new ones less those other steps
        // wrote while it was paused.
        let after = self.state_counts(environment);
        let written = self.written.entry(environment.to_owned()).or_default();
        let others = (written.0 - before.written.0, written.1 - before.written.1);
        let new_commits = (
            after.0 - before.counts.0 - others.0,
            after.1 - before.counts.1 - others.1,
        );
        written.0 += new_commits.0;
        written.1 += new_commits.1;
        if let Some(id) = &invoked.id {
            self.record_write(id, environment, new_commits);
        }
        let describe = || {
            format!(
                "nyl {}:\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                outcome.stdout,
                outcome.stderr
            )
        };
        if checked.killed == Some(true) {
            if self.shared.faults.is_armed() {
                return Err(format!("the armed fault never fired; {}", describe()));
            }
            return Ok(());
        }
        if let Some(exit) = checked.exit {
            if i32::from(outcome.exit) != exit {
                return Err(format!("exit {} instead of {exit}; {}", outcome.exit, describe()));
            }
        }
        let Some(expect) = &before.expect else {
            return Ok(());
        };
        // A `--local` run's transition commits stay in its own clone.
        let local = args.iter().any(|arg| arg == "--local");
        let summaries = if reads_summary(expect) && (new_commits.0 > 0 || new_commits.1 > 0) {
            let (side, count) = if new_commits.0 > 0 {
                ("desired", new_commits.0)
            } else {
                ("observed", new_commits.1)
            };
            self.commit_summaries(environment, side, count)
                .map_err(|error| format!("{error}; {}", describe()))?
        } else {
            Vec::new()
        };
        // The record of an unreadable path cannot be found.
        if let Some(Err(error)) = &before.promote {
            if expect.promotion.is_some() {
                return Err(format!(
                    "the promote step's target environment is unknown ({error}), so its record cannot be checked; {}",
                    describe()
                ));
            }
        }
        check_results(expect, &outcome.report, (!local).then_some(summaries.as_slice()))
            .map_err(|error| format!("{error}; {}", describe()))?;
        if let (Some(commits), Some(Err(error))) = (&expect.commits, &before.promote) {
            // Nothing is counted for a path that cannot be read, so only "writes
            // nothing" can hold, and every ref is then checked below.
            if (commits.desired, commits.observed) != (0, 0) {
                return Err(format!(
                    "the promote step's target environment is unknown ({error}), so only commits of 0 can hold; {}",
                    describe()
                ));
            }
        } else if let Some(commits) = &expect.commits {
            if new_commits != (commits.desired, commits.observed) {
                return Err(format!(
                    "new state commits {{desired: {}, observed: {}}} instead of {{desired: {}, observed: {}}}; {}",
                    new_commits.0,
                    new_commits.1,
                    commits.desired,
                    commits.observed,
                    describe()
                ));
            }
        }
        if let Some(refs) = &before.state_refs {
            self.check_other_refs_unchanged(refs, &before.excused_refs)
                .map_err(|error| format!("{error}; {}", describe()))?;
        }
        if let Some(promotion) = &expect.promotion {
            self.check_promotion(promotion, before, branch, &outcome)
                .map_err(|error| {
                    let hints = before.hints.iter().map(|hint| format!(" ({hint})")).collect::<String>();
                    format!("{error}{hints}; {}", describe())
                })?;
        }
        if let Some(codes) = &expect.codes {
            let mut reported = BTreeSet::new();
            collect_codes(&outcome.report, &mut reported);
            for code in codes {
                if !reported.contains(code) {
                    return Err(format!(
                        "code {code} was not reported (got {reported:?}); {}",
                        describe()
                    ));
                }
            }
        }
        for message in expect.messages.iter().flatten() {
            if !outcome.stdout.contains(message) && !outcome.stderr.contains(message) {
                return Err(format!("the output does not contain {message:?}; {}", describe()));
            }
        }
        if let Some(units) = &expect.units {
            let status = units
                .values()
                .any(|expectation| expectation.condition_state.is_some() || expectation.drift.is_some())
                .then(|| self.status(environment, branch))
                .transpose()?;
            for (unit, expectation) in units {
                self.check_unit(unit, expectation, environment, branch)?;
                if let Some(status) = &status {
                    check_status_attributes(unit, expectation, status)?;
                }
            }
        }
        if let Some(deploy) = &expect.deploy {
            let before = before
                .deploy
                .as_ref()
                .ok_or("no deploy tree was taken before the step")?;
            self.check_deploy(deploy, before, branch)?;
        }
        if let Some(revision) = &expect.source {
            let commit = self.world.rev_parse(revision);
            let ran = outcome
                .report
                .pointer("/environments/0/sourceCommit")
                .and_then(Value::as_str);
            if ran != Some(commit.as_str()) {
                return Err(format!(
                    "the run's source is {ran:?} instead of {revision} ({commit}); {}",
                    describe()
                ));
            }
        }
        if let Some(expected) = &expect.source_chosen_by {
            let chosen = self.source_choice(environment)?;
            if !subset(expected, &chosen) {
                return Err(format!(
                    "sourceChosenBy is {chosen} instead of {expected}; {}",
                    describe()
                ));
            }
        }
        if let Some(keep) = &expect.keep_refs {
            self.check_keep_refs(keep)?;
        }
        if let Some(get) = &expect.get {
            let args = &get.args;
            let outcome = self.invoke(args, branch)?;
            if let Some(value) = &get.value {
                let printed = Value::String(outcome.stdout.trim().to_owned());
                if &outcome.report != value && &printed != value {
                    return Err(format!(
                        "get {args:?} gave {} (printed {:?}) instead of {value}",
                        outcome.report, outcome.stdout
                    ));
                }
            }
            for (pointer, value) in get.json.iter().flatten() {
                if outcome.report.pointer(pointer) != Some(value) {
                    return Err(format!(
                        "get {args:?}: {pointer} is {:?} instead of {value}",
                        outcome.report.pointer(pointer)
                    ));
                }
            }
        }
        Ok(())
    }

    /// The units of `nyl status -e <environment> -o json`, whatever its exit.
    fn status(&self, environment: &str, branch: &str) -> Result<Value, String> {
        let args = ["status", "-e", environment, "-o", "json"].map(str::to_owned);
        let outcome = self.invoke(&args, branch)?;
        outcome
            .report
            .pointer("/environments/0/units")
            .cloned()
            .ok_or_else(|| format!("status of {environment} has no units: {}", outcome.stderr))
    }

    /// [`Self::invoke`] with no fault armed, so a read the harness makes for a
    /// placeholder cannot consume the fault meant for the step.
    fn invoke_without_faults(&self, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
        if self.shared.tier == 2 || !is_orchestration_command(args) {
            return invoke_binary(self.shared, args, branch);
        }
        let wiring = self.shared.wiring(&nyl_orchestration::fault::NoFaults);
        invoke(self.world, &wiring, args, branch)
    }

    /// One sequential in-process invocation, without faults beyond the slot.
    fn invoke(&self, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
        // Commands outside orchestration, such as `publish-tree`, have no
        // in-process entry: they run as the binary in both tiers.
        if self.shared.tier == 2 || !is_orchestration_command(args) {
            return invoke_binary(self.shared, args, branch);
        }
        let wiring = self.shared.wiring(&self.shared.faults);
        invoke(self.world, &wiring, args, branch)
    }
    /// `text` with its placeholders replaced, as of `branch`.
    fn expand(&self, text: &str, branch: &str) -> Result<String, String> {
        let written = |id: &str| {
            self.writes
                .get(id)
                .ok_or_else(|| format!("no nyl step with id {id} has run"))
        };
        let state = |id: &str| {
            written(id)?
                .state()
                .cloned()
                .ok_or_else(|| format!("the step {id} wrote no state commit"))
        };
        let observed = |id: &str| {
            written(id)?
                .observed
                .clone()
                .ok_or_else(|| format!("the step {id} wrote no observed-state commit"))
        };
        let commit = |revision: &str| self.world.try_rev_parse(revision);
        let plan_digest = |environment: &str, unit: &str| {
            // Through the binary in tier 2, like every step there.
            let args = ["plan", "-e", environment, "--unit", unit, "-o", "json"].map(str::to_owned);
            let outcome = self.invoke_without_faults(&args, branch)?;
            let pointer = format!(
                "/environments/0/units/{}/changeDigest",
                unit.replace('~', "~0").replace('/', "~1")
            );
            outcome
                .report
                .pointer(&pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!(
                        "nyl plan -e {environment} --unit {unit} reported no changeDigest (exit {}); stdout:\n{}\nstderr:\n{}",
                        outcome.exit, outcome.stdout, outcome.stderr
                    )
                })
        };
        expand_placeholders(
            text,
            &Resolver {
                state: &state,
                observed: &observed,
                commit: &commit,
                plan_digest: &plan_digest,
            },
        )
    }

    fn expand_all(&self, texts: &[String], branch: &str) -> Result<Vec<String>, String> {
        texts.iter().map(|text| self.expand(text, branch)).collect()
    }

    /// An `argocd` step: script what the fake Argo CD reports for the
    /// target's deployments. Each `runs` id becomes the publication commit
    /// that step's receipt for the target records; `unmatched` becomes a
    /// commit on the publication branch that edits the deployments'
    /// directories outside Nyl, which a second commit restores, so no
    /// publication matches it.
    fn argocd(&self, step: &ArgocdStep) -> Result<(), String> {
        let readings = std::iter::once((None, &step.all))
            .chain(
                step.releases
                    .iter()
                    .map(|(release, reading)| (Some(release.as_str()), reading)),
            )
            .collect::<Vec<_>>();
        // The deployments the target expects, from the facts its
        // publication unit recorded.
        let (_, document) = self
            .publication_units(&step.target)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                format!(
                    "argocd: no environment has a publication unit for target {}",
                    step.target
                )
            })?;
        let facts: ReconcilerFacts = serde_json::from_value(
            document
                .pointer("/resolvedSpec/reconciler")
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "argocd: the publication unit of target {} records no reconciler facts: does it declare observe: {{}}?",
                        step.target
                    )
                })?,
        )
        .map_err(|error| format!("argocd: the reconciler facts of target {}: {error}", step.target))?;
        let expected = &facts.expected;
        for release in step.releases.keys() {
            if !expected
                .deployments
                .iter()
                .any(|deployment| deployment.release.as_deref() == Some(release.as_str()))
            {
                return Err(format!(
                    "argocd: target {} has no Application for Release {release}",
                    step.target
                ));
            }
        }
        let mut unmatched = None;
        if readings.iter().any(|(_, reading)| reading.runs == UNMATCHED) {
            let paths = expected
                .deployments
                .iter()
                .filter(|deployment| {
                    let release = deployment.release.as_deref();
                    readings
                        .iter()
                        .find(|(named, _)| named.is_some() && *named == release)
                        .or(readings.first())
                        .is_some_and(|(_, reading)| reading.runs == UNMATCHED)
                })
                .map(|deployment| deployment.path.as_str())
                .collect::<Vec<_>>();
            if paths.is_empty() {
                return Err(format!(
                    "argocd: no Application of target {} runs unmatched",
                    step.target
                ));
            }
            if expected.publication.url != self.world.project_url() {
                return Err(format!(
                    "argocd: target {} publishes to {}, not to project.git",
                    step.target, expected.publication.url
                ));
            }
            unmatched = Some(self.world.edit_outside_nyl(&expected.publication.branch, &paths));
        }
        let mut commits = BTreeMap::new();
        for (_, reading) in &readings {
            if reading.runs != UNMATCHED && !commits.contains_key(&reading.runs) {
                commits.insert(reading.runs.clone(), self.published(&reading.runs, &step.target)?);
            }
        }
        let deployment = |reading: &ApplicationReading| ScriptedDeployment {
            running: Running::Exact(if reading.runs == UNMATCHED {
                unmatched.clone().expect("made above")
            } else {
                commits[&reading.runs].clone()
            }),
            health: match reading.health {
                ApplicationHealth::Healthy => Health::Healthy,
                ApplicationHealth::Progressing => Health::Progressing,
                ApplicationHealth::Degraded => Health::Degraded,
                ApplicationHealth::Unknown => Health::Unknown,
            },
        };
        self.shared.script.observer().set_reading(
            &step.target,
            ScriptedReading {
                all: deployment(&step.all),
                releases: step
                    .releases
                    .iter()
                    .map(|(release, reading)| (release.clone(), deployment(reading)))
                    .collect(),
            },
        );
        Ok(())
    }

    /// Every publication unit for `target`, by environment, as each
    /// environment's desired ref holds it now: the desired documents whose
    /// resolved spec names the target.
    fn publication_units(&self, target: &str) -> Result<Vec<(String, Value)>, String> {
        let bare = self.world.bare();
        let refs = git_output(&bare, &["for-each-ref", "--format=%(refname)", "refs/heads/nyl/"]);
        let mut found = Vec::new();
        for reference in refs.lines().filter(|reference| reference.ends_with("/desired")) {
            let environment = reference
                .trim_start_matches("refs/heads/nyl/")
                .trim_end_matches("/desired")
                .to_owned();
            let files = git_output(&bare, &["ls-tree", "-r", "--name-only", reference, "desired/units/"]);
            for file in files.lines() {
                let text = git_output(&bare, &["show", &format!("{reference}:{file}")]);
                let document: Value =
                    serde_saphyr::from_str(&text).map_err(|error| format!("{reference}:{file}: {error}"))?;
                if document.pointer("/resolvedSpec/target").and_then(Value::as_str) == Some(target) {
                    found.push((environment.clone(), document));
                }
            }
        }
        Ok(found)
    }

    /// The publication commit that the receipt of `target`'s publication
    /// unit records in the observed state the step with `id` wrote.
    fn published(&self, id: &str, target: &str) -> Result<String, String> {
        let write = self
            .writes
            .get(id)
            .ok_or_else(|| format!("argocd runs {id}: no nyl step with that id has run"))?;
        let observed = write
            .observed
            .as_ref()
            .ok_or_else(|| format!("argocd runs {id}: the step wrote no observed-state commit"))?;
        let (_, document) = self
            .publication_units(target)?
            .into_iter()
            .find(|(environment, _)| *environment == write.environment)
            .ok_or_else(|| {
                format!(
                    "argocd runs {id}: {} has no publication unit for target {target}",
                    write.environment
                )
            })?;
        let unit = document
            .pointer("/unit/name")
            .and_then(Value::as_str)
            .ok_or_else(|| "argocd: a desired unit without a name".to_owned())?;
        let text = self
            .world
            .show(&format!("{observed}:observed/units/{unit}/artifacts/tree.yaml"))
            .map_err(|error| {
                format!("argocd runs {id}: {unit} has no PublishedTree artifact in that state: {error}")
            })?;
        let artifact: Value =
            serde_saphyr::from_str(&text).map_err(|error| format!("{unit}'s tree artifact: {error}"))?;
        artifact
            .pointer("/spec/commit")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("argocd runs {id}: {unit}'s tree artifact records no commit"))
    }

    /// Remember the tips the step with `id` wrote to `environment`'s refs.
    /// Known limitation: for a paused step the tips are those at its resume, so
    /// a step that ran while it was paused and wrote to the same environment can
    /// be taken for its commit. No scenario names such a step in a placeholder.
    fn record_write(&mut self, id: &str, environment: &str, new_commits: Counts) {
        let repository = self.world.repository();
        let tip = |side: &str, count: usize| {
            (count > 0)
                .then(|| {
                    repository
                        .refname_to_id(&format!("refs/heads/nyl/{environment}/{side}"))
                        .ok()
                })
                .flatten()
        };
        let desired = tip("desired", new_commits.0);
        let observed = tip("observed", new_commits.1);
        let run = desired.or(observed).and_then(|oid| {
            let commit = repository.find_commit(oid).ok()?;
            let message = String::from_utf8_lossy(commit.message_bytes()).into_owned();
            message
                .lines()
                .find_map(|line| line.strip_prefix("Nyl-Run-Id:"))
                .map(|run| run.trim().to_owned())
        });
        self.writes.insert(
            id.to_owned(),
            StepWrite {
                desired: desired.map(|oid| oid.to_string()),
                observed: observed.map(|oid| oid.to_string()),
                run,
                environment: environment.to_owned(),
            },
        );
    }

    /// Every YAML document of the tip of `branch` that parses.
    fn documents(&self, branch: &str) -> Vec<Value> {
        let repository = self.world.repository();
        let Ok(tree) = repository
            .revparse_single(&format!("refs/heads/{branch}"))
            .and_then(|object| object.peel_to_tree())
        else {
            return Vec::new();
        };
        let mut documents = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |_, entry| {
            let yaml = entry
                .name()
                .is_ok_and(|name| name.ends_with(".yaml") || name.ends_with(".yml"));
            if yaml && entry.kind() == Some(git2::ObjectType::Blob) {
                if let Ok(blob) = repository.find_blob(entry.id()) {
                    if let Ok(parsed) = serde_saphyr::from_multiple::<Value>(&String::from_utf8_lossy(blob.content())) {
                        documents.extend(parsed);
                    }
                }
            }
            git2::TreeWalkResult::Ok
        })
        .expect("the tree walks");
        documents
    }

    /// The PromotionPath a `promote` step names, as declared at `branch`.
    fn promotion_path(&self, args: &[String], branch: &str) -> Result<PromotionPath, String> {
        promotion_target(&self.documents(branch), args)
    }

    /// Every branch, state ref, lease, and keep ref of `project.git` and the
    /// commit it names. The state refs are the default layout's: a state
    /// repository of its own is not read.
    fn state_refs(&self) -> BTreeMap<String, String> {
        git_output(
            &self.world.bare(),
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/",
                "refs/nyl/",
                "refs/nyl-keep/",
            ],
        )
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, commit)| (name.to_owned(), commit.to_owned()))
        .collect()
    }

    /// Promotion only reads its source: no ref but the ones the step may
    /// change (`excused`) may differ, none at all for a dry run.
    fn check_other_refs_unchanged(&self, before: &BTreeMap<String, String>, excused: &[String]) -> Result<(), String> {
        let after = self.state_refs();
        let changed = unchanged_violations(before, &after, excused);
        if changed.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the promote step changed refs it may not (excused: {excused:?}): {}",
                changed.join(", ")
            ))
        }
    }

    /// The text of `desired/<relative>` on `environment`'s own desired ref.
    /// Only the default state layout is read, so a scenario that keeps its
    /// state elsewhere fails here, not with a record that is silently missed.
    fn desired_file(&self, environment: &str, relative: &str) -> Result<String, String> {
        let reference = format!("refs/heads/nyl/{environment}/desired");
        let shown = std::process::Command::new("git")
            .current_dir(self.world.bare())
            .args(["show", &format!("{reference}:desired/{relative}")])
            .output()
            .map_err(|error| error.to_string())?;
        if shown.status.success() {
            Ok(String::from_utf8_lossy(&shown.stdout).into_owned())
        } else {
            Err(format!(
                "{reference} has no desired/{relative} (the harness reads the default state layout only): {}",
                String::from_utf8_lossy(&shown.stderr).trim()
            ))
        }
    }

    /// The PromotionRecord a `promote` step wrote, where its path keeps it.
    fn promotion_record(
        &self,
        expected: &PromotionExpectation,
        before: &Before,
        branch: &str,
        outcome: &CommandOutcome,
    ) -> Result<Value, String> {
        if before.args.iter().any(|arg| arg == "--dry-run") {
            return outcome
                .report
                .pointer("/record")
                .cloned()
                .ok_or_else(|| "the dry run's report shows no record".to_owned());
        }
        let target = &before.environment;
        match expected.record {
            RecordLocation::State => {
                let text = self.desired_file(target, &format!("promotions/{}.yaml", expected.path))?;
                serde_saphyr::from_str(&text)
                    .map_err(|error| format!("{target}'s record of {}: {error}", expected.path))
            }
            RecordLocation::Source => self
                .documents(branch)
                .into_iter()
                .filter(|document| {
                    document.get("kind").and_then(Value::as_str) == Some("Environment")
                        && document.pointer("/metadata/name").and_then(Value::as_str) == Some(target.as_str())
                })
                .find_map(|document| document.pointer("/spec/source/fromPromotion/promoted").cloned())
                .ok_or_else(|| format!("{branch} has no source.fromPromotion.promoted block on Environment {target}")),
        }
    }

    fn check_promotion(
        &self,
        expected: &PromotionExpectation,
        before: &Before,
        branch: &str,
        outcome: &CommandOutcome,
    ) -> Result<(), String> {
        let record = self.promotion_record(expected, before, branch, outcome)?;
        let path_from = before
            .promote
            .as_ref()
            .and_then(|path| path.as_ref().ok())
            .and_then(|path| path.from.clone());
        let run_of = |id: &str| {
            self.writes
                .get(id)
                .and_then(|write| write.run.clone())
                .ok_or_else(|| format!("the step {id} wrote no state commit with a run"))
        };
        // The expectation's arguments were expanded as the step started.
        let get = |args: &[String]| {
            let outcome = self.invoke(args, branch)?;
            if outcome.exit != 0 {
                return Err(format!(
                    "nyl {} exited {}: {}",
                    args.join(" "),
                    outcome.exit,
                    outcome.stderr.trim()
                ));
            }
            Ok((outcome.report, outcome.stdout.trim().to_owned()))
        };
        let expand = |text: &str| Ok(text.to_owned());
        let commit = |revision: &str| self.world.try_rev_parse(revision);
        check_promotion_record(
            expected,
            &record,
            &PromotionLookups {
                path_from: path_from.as_deref(),
                run_of: &run_of,
                get: &get,
                expand: &expand,
                commit: &commit,
            },
        )
    }

    /// Commits on the environment's desired and observed refs.
    fn state_counts(&self, environment: &str) -> (usize, usize) {
        let repository = self.world.repository();
        let count = |side: &str| history(&repository, &format!("refs/heads/nyl/{environment}/{side}")).count();
        (count("desired"), count("observed"))
    }

    /// Path to blob of every file on `branch`, empty when it does not exist.
    fn tree(&self, branch: &str) -> BTreeMap<String, String> {
        let repository = self.world.repository();
        let mut files = BTreeMap::new();
        let Ok(tree) = repository
            .revparse_single(&format!("refs/heads/{branch}"))
            .and_then(|object| object.peel_to_tree())
        else {
            return files;
        };
        tree.walk(git2::TreeWalkMode::PreOrder, |directory, entry| {
            if entry.kind() != Some(git2::ObjectType::Tree) {
                let name = entry.name().expect("UTF-8 path");
                files.insert(format!("{directory}{name}"), entry.id().to_string());
            }
            git2::TreeWalkResult::Ok
        })
        .expect("the tree walks");
        files
    }

    /// The YAML summaries of the step's `count` newest transition commits
    /// on the environment's `side` ref, newest first.
    fn commit_summaries(&self, environment: &str, side: &str, count: usize) -> Result<Vec<Value>, String> {
        let repository = self.world.repository();
        let messages = history(&repository, &format!("refs/heads/nyl/{environment}/{side}"))
            .take(count)
            .map(|oid| {
                let commit = repository.find_commit(oid).expect("a state commit");
                String::from_utf8_lossy(commit.message_bytes()).into_owned()
            })
            .collect::<Vec<_>>();
        messages
            .iter()
            .map(|message| message.trim())
            .filter(|message| !message.is_empty())
            .map(|message| {
                let body = message
                    .split("\n\n")
                    .nth(1)
                    .ok_or_else(|| format!("the transition commit has no summary:\n{message}"))?;
                serde_saphyr::from_str(body).map_err(|error| format!("summary {body:?}: {error}"))
            })
            .collect()
    }

    fn check_unit(
        &mut self,
        unit: &str,
        expectation: &UnitExpectation,
        environment: &str,
        branch: &str,
    ) -> Result<(), String> {
        let args = ["get", "unit", unit, "-e", environment, "-o", "json"].map(str::to_owned);
        let outcome = self.invoke(&args, branch)?;
        if outcome.exit != 0 {
            return Err(format!("get unit {unit} exited {}: {}", outcome.exit, outcome.stderr));
        }
        let documents = &outcome.report;
        let desired = documents.get("desired").filter(|value| !value.is_null());
        let observed = documents.get("observed").filter(|value| !value.is_null());
        if let Some(lifecycle) = &expectation.lifecycle {
            let persisted = desired
                .and_then(|desired| desired.get("lifecycle"))
                .ok_or_else(|| format!("{unit} has no desired lifecycle"))?;
            let matches = match lifecycle {
                Value::String(state) => persisted.get("state") == Some(&Value::String(state.clone())),
                other => subset(other, persisted),
            };
            if !matches {
                return Err(format!("{unit}: lifecycle is {persisted} instead of {lifecycle}"));
            }
        }
        if let Some(condition) = &expectation.condition {
            let persisted = observed
                .and_then(|observed| observed.get("condition"))
                .filter(|value| !value.is_null());
            let matches = match (condition, persisted) {
                (Value::Null, None) => true,
                (Value::Null, Some(_)) | (_, None) => false,
                (expected, Some(persisted)) => subset(expected, persisted),
            };
            if !matches {
                return Err(format!("{unit}: condition is {persisted:?} instead of {condition}"));
            }
        }
        if let Some(receipt) = expectation.receipt {
            let parse_desired = desired
                .map(|value| serde_json::from_value::<DesiredUnit>(value.clone()))
                .transpose()
                .map_err(|error| format!("{unit}: desired document: {error}"))?;
            let parse_observed = observed
                .map(|value| serde_json::from_value::<ObservedUnit>(value.clone()))
                .transpose()
                .map_err(|error| format!("{unit}: observed document: {error}"))?;
            let state = receipt_state(parse_desired.as_ref(), parse_observed.as_ref());
            let expected = match receipt {
                ReceiptExpectation::Current => ReceiptState::Current,
                ReceiptExpectation::Stale => ReceiptState::Stale,
                ReceiptExpectation::None => ReceiptState::None,
            };
            if state != expected {
                return Err(format!("{unit}: receipt is {state:?} instead of {expected:?}"));
            }
        }
        let uid = desired
            .and_then(|desired| desired.pointer("/unit/uid"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(expected) = expectation.uid {
            let previous = self.uids.get(unit);
            let same = previous.is_some_and(|previous| previous == &uid);
            match expected {
                UidExpectation::Same if !same => {
                    return Err(format!("{unit}: uid {uid:?} is not the earlier {previous:?}"));
                }
                UidExpectation::New if same || uid.is_none() => {
                    return Err(format!("{unit}: uid {uid:?} is not new"));
                }
                UidExpectation::None if uid.is_some() => {
                    return Err(format!("{unit}: has uid {uid:?} instead of none"));
                }
                _ => {}
            }
        }
        self.uids.insert(unit.to_owned(), uid);
        Ok(())
    }

    fn check_deploy(
        &self,
        deploy: &DeployExpectation,
        before: &BTreeMap<String, String>,
        branch: &str,
    ) -> Result<(), String> {
        let after = self.tree(&deploy.branch);
        for pattern in &deploy.contains {
            if !after.keys().any(|path| glob_matches(pattern, path)) {
                return Err(format!(
                    "{} has no file matching {pattern}: {:?}",
                    deploy.branch,
                    after.keys().collect::<Vec<_>>()
                ));
            }
        }
        for pattern in &deploy.absent {
            if let Some(path) = after.keys().find(|path| glob_matches(pattern, path)) {
                return Err(format!("{} still has {path}, which matches {pattern}", deploy.branch));
            }
        }
        for pattern in &deploy.unchanged {
            let select = |tree: &BTreeMap<String, String>| {
                tree.iter()
                    .filter(|(path, _)| glob_matches(pattern, path))
                    .map(|(path, blob)| (path.clone(), blob.clone()))
                    .collect::<BTreeMap<_, _>>()
            };
            if select(before) != select(&after) {
                return Err(format!("{} changed files matching {pattern}", deploy.branch));
            }
        }
        for (pattern, mentions) in &deploy.mentions {
            let files = after
                .keys()
                .filter(|path| glob_matches(pattern, path))
                .map(|path| {
                    (
                        path.clone(),
                        git_output(
                            &self.world.bare(),
                            &["show", &format!("refs/heads/{}:{path}", deploy.branch)],
                        ),
                    )
                })
                .collect::<Vec<_>>();
            if files.is_empty() {
                return Err(format!("{} has no file matching {pattern} to read", deploy.branch));
            }
            for mention in mentions {
                let text = match mention {
                    Mention::Text(text) => text.clone(),
                    Mention::Get(args) => {
                        let outcome = self.invoke(args, branch)?;
                        match &outcome.report {
                            Value::String(value) => value.clone(),
                            Value::Null => outcome.stdout.trim().to_owned(),
                            other => other.to_string(),
                        }
                    }
                };
                if text.is_empty() || !files.iter().any(|(_, content)| content.contains(&text)) {
                    return Err(format!(
                        "no file of {} matching {pattern} mentions {text:?} ({mention:?}); files: {:?}",
                        deploy.branch,
                        files.iter().map(|(path, _)| path).collect::<Vec<_>>()
                    ));
                }
            }
        }
        Ok(())
    }

    /// The `sourceChosenBy` of `environment`'s record in the state repository:
    /// beside its own refs, or under its name in a shared one.
    fn source_choice(&self, environment: &str) -> Result<Value, String> {
        let bare = self.world.bare();
        let refs = git_output(&bare, &["for-each-ref", "--format=%(refname)", "refs/heads/nyl/"]);
        let candidates = std::iter::once(format!("refs/heads/nyl/{environment}/desired:desired/environment.yaml"))
            .chain(
                refs.lines()
                    .map(|reference| format!("{reference}:{environment}/desired/environment.yaml")),
            );
        for candidate in candidates {
            let shown = std::process::Command::new("git")
                .current_dir(&bare)
                .args(["show", &candidate])
                .output()
                .map_err(|error| error.to_string())?;
            if shown.status.success() {
                let record: Value = serde_saphyr::from_str(&String::from_utf8_lossy(&shown.stdout))
                    .map_err(|error| format!("{candidate}: {error}"))?;
                return record
                    .get("sourceChosenBy")
                    .cloned()
                    .ok_or_else(|| format!("{candidate} has no sourceChosenBy"));
            }
        }
        Err(format!(
            "no environment record of {environment} in the state repository"
        ))
    }

    /// The keep refs of `project.git` against `expected`.
    fn check_keep_refs(&self, expected: &KeepRefsExpectation) -> Result<(), String> {
        let listed = git_output(
            &self.world.bare(),
            &["for-each-ref", "--format=%(refname) %(objectname)", "refs/nyl-keep/"],
        );
        let refs = listed
            .lines()
            .filter_map(|line| line.split_once(' '))
            .collect::<Vec<_>>();
        if refs.len() != expected.count {
            return Err(format!(
                "{} keep refs instead of {}: {refs:?}",
                refs.len(),
                expected.count
            ));
        }
        for revision in &expected.kept {
            let commit = self.world.rev_parse(revision);
            // A keep ref is named for the commit it keeps, and points at it.
            if !refs
                .iter()
                .any(|(name, target)| *target == commit && name.ends_with(&format!("/{commit}")))
            {
                return Err(format!("no keep ref for {revision} ({commit}): {refs:?}"));
            }
        }
        Ok(())
    }
}

/// Whether `expect` names a key checked against the transition summary.
/// The commits `reference` reaches, newest first; none when it does not exist.
fn history<'r>(repository: &'r git2::Repository, reference: &str) -> impl Iterator<Item = git2::Oid> + 'r {
    let mut walk = repository.revwalk().expect("a revision walk");
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::TIME)
        .expect("sorting");
    if let Ok(tip) = repository.refname_to_id(reference) {
        walk.push(tip).expect("the tip");
    }
    walk.map(|oid| oid.expect("a reachable commit"))
}

fn reads_summary(expect: &Expect) -> bool {
    outcome_sets(expect).iter().any(|(_, set)| set.is_some())
        || list_keys(expect).iter().any(|(_, list)| list.is_some())
        || expect.drift_checks.is_some()
        || expect.lease_broken.is_some()
}

/// The outcome keys of `expect` with the sets they name, in [`OUTCOME_KEYS`] order.
fn outcome_sets(expect: &Expect) -> [(&'static str, &Option<Vec<String>>); 14] {
    let sets = [
        &expect.executed,
        &expect.current,
        &expect.ready,
        &expect.blocked,
        &expect.failed,
        &expect.uncertain,
        &expect.awaiting_approval,
        &expect.pending_teardown,
        &expect.tearing_down,
        &expect.retained,
        &expect.removed,
        &expect.released,
        &expect.held,
        &expect.drifted,
    ];
    std::array::from_fn(|index| (OUTCOME_KEYS[index], sets[index]))
}

/// The keys of `expect` that name a list of units in both the report and
/// the transition summary: the expect key, the summary's and the report's
/// key, and the expected list.
fn list_keys(expect: &Expect) -> [(&'static str, &Option<Vec<String>>); 5] {
    [
        ("recovered", &expect.recovered),
        ("imported", &expect.imported),
        ("driftRepaired", &expect.drift_repaired),
        ("reapplied", &expect.reapplied),
        ("observationsChanged", &expect.observations_changed),
    ]
}

/// The run report of one environment inside a step's report: a run
/// report's first environment, or the teardown run a `state delete
/// --teardown` report carries under `details.teardown`.
fn run_environment(report: &Value) -> Option<&Value> {
    report
        .pointer("/environments/0")
        .or_else(|| report.pointer("/details/teardown/environments/0"))
}

/// A unit's outcome name in a report or a summary entry: its `result`
/// (reconcile, teardown), `state` (plan, status), or `check` (detect-drift, attest),
/// as `UnitReport::outcome_name` orders them.
fn outcome_of(unit: &Value) -> Option<&str> {
    ["result", "state", "check"]
        .iter()
        .find_map(|key| unit.get(*key).and_then(Value::as_str))
}

/// Check the outcome sets, the unit lists, the lease break, and the state
/// operation outcome `expect` names against a step's JSON report and the
/// summaries of the transition commits the step wrote, newest first, or
/// `None` for a `--local` run, whose commits never reach `project.git`, so
/// only its report is checked.
///
/// - Outcome keys compare against the report's units and, when the step
///   wrote a transition commit whose `units:` block carries outcomes, the
///   newest such block; a state operation's actions (`hold`, `resume`)
///   carry none, so only its report is read.
/// - `recovered`, `imported`, `driftRepaired`, `reapplied`, and
///   `observationsChanged` compare against the newest summary's list
///   (`reapplied`'s `units`, `observationsChanged`'s units) and the
///   report's list of that name, missing meaning empty, in the run report
///   or a state operation's `details`.
/// - `lease-broken` checks whether the newest summary records `leaseBroken`.
pub fn check_results(expect: &Expect, report: &Value, summaries: Option<&[Value]>) -> Result<(), String> {
    if let Some(expected) = expect.outcome {
        let reported = report.get("outcome").and_then(Value::as_str);
        if reported != Some(expected.name()) {
            return Err(format!(
                "outcome: the report has {reported:?} instead of {}",
                expected.name()
            ));
        }
    }
    let sets = outcome_sets(expect);
    if sets.iter().any(|(_, set)| set.is_some()) {
        let units = run_environment(report)
            .and_then(|environment| environment.get("units"))
            .and_then(Value::as_object)
            .ok_or("the report has no units")?;
        let summary_units = summaries
            .into_iter()
            .flatten()
            .map(summary_units)
            .find(|units| !units.is_empty());
        for (key, expected) in sets {
            let Some(expected) = expected else { continue };
            let expected = expected.iter().cloned().collect::<BTreeSet<_>>();
            let reported = units
                .iter()
                .filter(|(_, unit)| outcome_of(unit) == Some(key))
                .map(|(name, _)| name.clone())
                .collect::<BTreeSet<_>>();
            if reported != expected {
                return Err(format!("{key}: the report has {reported:?} instead of {expected:?}"));
            }
            if let Some(summary_units) = &summary_units {
                let listed = summary_units
                    .iter()
                    .filter(|(_, outcome)| outcome.as_str() == key)
                    .map(|(name, _)| name.clone())
                    .collect::<BTreeSet<_>>();
                if listed != expected {
                    return Err(format!(
                        "{key}: the transition commit summary lists {listed:?} instead of {expected:?}"
                    ));
                }
            }
        }
    }
    let newest = summaries.map(|summaries| summaries.first());
    for (key, expected) in list_keys(expect) {
        let Some(expected) = expected else { continue };
        let expected = expected.iter().cloned().collect::<BTreeSet<_>>();
        if let Some(newest) = newest {
            let summary =
                newest.ok_or_else(|| format!("{key}: the step wrote no transition commit whose summary lists it"))?;
            let listed = match key {
                "reapplied" => names(summary.pointer("/reapplied/units")),
                // Per unit, the observations whose value changed.
                "observationsChanged" => keys(summary.get(key)),
                _ => names(summary.get(key)),
            };
            if listed != expected {
                return Err(format!(
                    "{key}: the transition commit summary lists {listed:?} instead of {expected:?}"
                ));
            }
        }
        let reported = run_environment(report)
            .and_then(|environment| environment.get(key))
            .or_else(|| report.pointer(&format!("/details/{key}")));
        let reported = if key == "observationsChanged" {
            keys(reported)
        } else {
            names(reported)
        };
        if reported != expected {
            return Err(format!("{key}: the report lists {reported:?} instead of {expected:?}"));
        }
    }
    if let Some(expected) = &expect.drift_checks {
        // The report's and the newest summary's units that carry a `driftCheck`.
        let found = |units: Option<&serde_json::Map<String, Value>>| -> BTreeMap<String, String> {
            units
                .into_iter()
                .flatten()
                .filter_map(|(name, unit)| {
                    Some((name.clone(), unit.pointer("/driftCheck/check")?.as_str()?.to_owned()))
                })
                .collect()
        };
        let reported = found(
            run_environment(report)
                .and_then(|environment| environment.get("units"))
                .and_then(Value::as_object),
        );
        if &reported != expected {
            return Err(format!(
                "drift-checks: the report has {reported:?} instead of {expected:?}"
            ));
        }
        // No commit lists no check, so it satisfies only an empty expectation.
        if let Some(newest) = newest {
            let listed = newest.map_or_else(BTreeMap::new, |summary| {
                found(summary.get("units").and_then(Value::as_object))
            });
            if &listed != expected {
                return Err(format!(
                    "drift-checks: the transition commit summary has {listed:?} instead of {expected:?}"
                ));
            }
        }
    }
    if let Some(expected) = expect.lease_broken {
        let newest = newest.ok_or("lease-broken: a --local run's transition commit is not in project.git")?;
        let recorded = newest.is_some_and(|summary| summary.get("leaseBroken").is_some());
        if recorded != expected {
            return Err(format!(
                "lease-broken: the transition commit summary {} a lease break",
                if recorded { "records" } else { "does not record" }
            ));
        }
    }
    Ok(())
}

/// Check the attributes `nyl status` shows for `unit` (`status` holds its
/// units): the condition next to its state, and its drift observation.
fn check_status_attributes(unit: &str, expectation: &UnitExpectation, status: &Value) -> Result<(), String> {
    let shown = status.get(unit);
    if let Some(expected) = &expectation.condition_state {
        let condition = shown
            .and_then(|unit| unit.get("condition"))
            .filter(|value| !value.is_null());
        let matches = match (expected, condition) {
            (Value::Null, None) => true,
            (expected, Some(condition)) => expected == condition,
            _ => false,
        };
        if !matches {
            return Err(format!(
                "{unit}: status shows condition {condition:?} instead of {expected}"
            ));
        }
    }
    if let Some(drift) = &expectation.drift {
        let result = shown
            .and_then(|unit| unit.pointer("/drift/result"))
            .and_then(Value::as_str);
        if result != Some(drift.result.name()) {
            return Err(format!(
                "{unit}: status shows drift {result:?} instead of {}",
                drift.result.name()
            ));
        }
    }
    Ok(())
}

/// What a `promote` step's PromotionPath declares, as far as the harness reads it.
#[derive(Debug, Clone)]
pub struct PromotionPath {
    /// The source environment, absent for a target source.
    pub from: Option<String>,
    /// The environment whose state holds the records.
    pub to: String,
    /// Whether the target Environment keeps its records in its source
    /// (`record: source`), which the step commits to the branch it ran at.
    pub record_source: bool,
}

/// The options of `nyl promote` that take a value, which the next argument is.
const PROMOTE_VALUE_OPTIONS: [&str; 8] = [
    "--reason",
    "--revision",
    "--state-revision",
    "--value",
    "--evidence",
    "--supersede",
    "--ignore-attestation",
    "--author",
];

/// The PromotionPath of `documents` named by the first positional argument of
/// a `promote` step.
pub fn promotion_target(documents: &[Value], args: &[String]) -> Result<PromotionPath, String> {
    let mut rest = args.iter().skip(1);
    let mut name = None;
    while let Some(arg) = rest.next() {
        if PROMOTE_VALUE_OPTIONS.contains(&arg.as_str()) {
            rest.next();
        } else if !arg.starts_with('-') {
            name = Some(arg.as_str());
            break;
        }
    }
    let name = name.ok_or_else(|| format!("{args:?} names no PromotionPath"))?;
    let kind = |document: &Value, kind: &str| document.get("kind").and_then(Value::as_str) == Some(kind);
    let named = |document: &Value, name: &str| document.pointer("/metadata/name").and_then(Value::as_str) == Some(name);
    let path = documents
        .iter()
        .find(|document| kind(document, "PromotionPath") && named(document, name))
        .ok_or_else(|| format!("no PromotionPath {name} is declared"))?;
    let environment = |side: &str| {
        path.pointer(&format!("/spec/{side}/environment"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let to = environment("to").ok_or("the PromotionPath promotes into no environment (a target?)")?;
    let record_source = documents.iter().any(|document| {
        kind(document, "Environment")
            && named(document, &to)
            && document
                .pointer("/spec/source/fromPromotion/record")
                .and_then(Value::as_str)
                == Some("source")
    });
    Ok(PromotionPath {
        from: environment("from"),
        to,
        record_source,
    })
}

/// The environment whose refs a step's `commits` count: the one its path
/// promotes into for a `promote` step (none when the path cannot be read),
/// else the step's `-e`, or dev.
pub fn counted_environment(args: &[String], promote: Option<&Result<PromotionPath, String>>) -> Option<String> {
    match promote {
        Some(Ok(path)) => Some(path.to.clone()),
        Some(Err(_)) => None,
        None => Some(environment_of(args)),
    }
}

/// The refs a promotion into `target` may change: the target environment's
/// state branches and lease, and for `record: source` the branch it commits to.
pub fn excused_refs(target: &str, source_branch: Option<&str>) -> Vec<String> {
    let mut refs = vec![format!("refs/heads/nyl/{target}/"), format!("refs/nyl/{target}/")];
    refs.extend(source_branch.map(|branch| format!("refs/heads/{branch}")));
    refs
}

/// The refs in `after` that differ from `before` (added, removed, or moved),
/// leaving out `excused`: a name ending in `/` excuses the refs under it, any
/// other name that ref only.
pub fn unchanged_violations(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    excused: &[String],
) -> Vec<String> {
    let excuses = |name: &str| {
        excused
            .iter()
            .any(|excused| name == excused || (excused.ends_with('/') && name.starts_with(excused.as_str())))
    };
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| !excuses(name))
        .filter(|name| before.get(*name) != after.get(*name))
        .cloned()
        .collect()
}

/// Where placeholders get their values; see [`expand_placeholders`].
pub struct Resolver<'a> {
    pub state: &'a dyn Fn(&str) -> Result<String, String>,
    pub observed: &'a dyn Fn(&str) -> Result<String, String>,
    pub commit: &'a dyn Fn(&str) -> Result<String, String>,
    pub plan_digest: &'a dyn Fn(&str, &str) -> Result<String, String>,
}

/// `text` with every placeholder (`%STATE(<id>)%`, `%OBSERVED(<id>)%`,
/// `%COMMIT(<revision>)%`, `%PLAN-DIGEST(<environment>/<unit>)%`) replaced by
/// what `resolver` gives for it.
pub fn expand_placeholders(text: &str, resolver: &Resolver<'_>) -> Result<String, String> {
    let mut expanded = text.to_owned();
    for placeholder in placeholders(text)? {
        let value = match &placeholder {
            Placeholder::State(id) => (resolver.state)(id),
            Placeholder::Observed(id) => (resolver.observed)(id),
            Placeholder::Commit(revision) => (resolver.commit)(revision),
            Placeholder::PlanDigest { environment, unit } => (resolver.plan_digest)(environment, unit),
        }
        .map_err(|error| format!("{}: {error}", placeholder.token()))?;
        expanded = expanded.replace(&placeholder.token(), &value);
    }
    Ok(expanded)
}

/// A `nyl get` invocation's report and its printed value.
pub type GetValue = (Value, String);

/// What [`check_promotion_record`] looks things up with.
pub struct PromotionLookups<'a> {
    /// The path's source environment, for a `from.environment` that is not given.
    pub path_from: Option<&'a str>,
    /// The run recorded on the state commits of the `nyl` step with an id.
    pub run_of: &'a dyn Fn(&str) -> Result<String, String>,
    /// A `nyl get` invocation's report and printed value.
    pub get: &'a dyn Fn(&[String]) -> Result<GetValue, String>,
    /// Replaces the placeholders of a commit the expectation names.
    pub expand: &'a dyn Fn(&str) -> Result<String, String>,
    /// A revision of `project.git` as a commit ID.
    pub commit: &'a dyn Fn(&str) -> Result<String, String>,
}

/// Compare a PromotionRecord with a `promotion` expectation: every key given
/// (`design/reference-scenarios.md#expect`), and only those. Lists the first
/// difference of every key that differs.
pub fn check_promotion_record(
    expected: &PromotionExpectation,
    record: &Value,
    lookups: &PromotionLookups<'_>,
) -> Result<(), String> {
    let mut problems = Problems::default();
    problems.compare("path", Value::String(expected.path.clone()), record.get("path"));
    if let Some(sequence) = expected.sequence {
        problems.compare("sequence", Value::from(sequence), record.get("sequence"));
    }
    match &expected.source_commit {
        None => {}
        Some(Value::Null) => problems.compare("sourceCommit", Value::Null, record.get("sourceCommit")),
        Some(Value::String(revision)) => {
            if let Some(commit) = problems.resolved("sourceCommit", (lookups.commit)(revision)) {
                problems.compare("sourceCommit", Value::String(commit), record.get("sourceCommit"));
            }
        }
        Some(other) => problems.push(format!("sourceCommit {other} must be a revision or null")),
    }

    let from = record.get("from");
    let field = |value: Option<&Value>, name: &str| value.and_then(|value| value.get(name)).cloned();
    if let Some(environment) = expected.from.environment.as_deref().or(lookups.path_from) {
        problems.compare(
            "from.environment",
            Value::String(environment.to_owned()),
            from.and_then(|from| from.get("environment")),
        );
    }
    if let Some(run) = problems.resolved("from.run", (lookups.run_of)(&expected.from.run)) {
        problems.compare("from.run", Value::String(run), from.and_then(|from| from.get("run")));
    }
    for (name, wanted) in [
        ("desiredCommit", &expected.from.desired_commit),
        ("observedCommit", &expected.from.observed_commit),
    ] {
        if let Some(wanted) = wanted {
            if let Some(commit) = problems.resolved(&format!("from.{name}"), (lookups.expand)(wanted)) {
                problems.compare(
                    &format!("from.{name}"),
                    Value::String(commit),
                    from.and_then(|from| from.get(name)),
                );
            }
        }
    }

    if let Some(values) = &expected.values {
        let recorded = record.get("values").and_then(Value::as_object);
        let names = |map: Option<&serde_json::Map<String, Value>>| {
            map.into_iter()
                .flat_map(|map| map.keys().cloned())
                .collect::<BTreeSet<_>>()
        };
        let wanted_names = values.keys().cloned().collect::<BTreeSet<_>>();
        if names(recorded) != wanted_names {
            problems.push(format!("values are {:?} instead of {wanted_names:?}", names(recorded)));
        }
        for (name, wanted) in values {
            let Some(entry) = recorded.and_then(|recorded| recorded.get(name)) else {
                continue;
            };
            let actual = entry.get("value");
            match &wanted.equals {
                Mention::Text(text) => problems.compare(&format!("values.{name}"), Value::String(text.clone()), actual),
                Mention::Get(args) => match (lookups.get)(args) {
                    Ok((report, printed)) => {
                        if actual != Some(&report) && actual != Some(&Value::String(printed.clone())) {
                            problems.push(format!(
                                "values.{name} is {} instead of {report} (printed {printed:?}) from {args:?}",
                                actual.map_or_else(|| "absent".to_owned(), Value::to_string)
                            ));
                        }
                    }
                    Err(error) => problems.push(format!("values.{name}: {error}")),
                },
            }
            let state = entry.get("state").filter(|state| !state.is_null());
            match &wanted.state {
                Some(id) => {
                    if let Some(run) = problems.resolved(&format!("values.{name}.state"), (lookups.run_of)(id)) {
                        problems.compare(
                            &format!("values.{name}.state.run"),
                            Value::String(run),
                            state.and_then(|state| state.get("run")),
                        );
                    }
                }
                None => {
                    if let Some(state) = state {
                        problems.push(format!(
                            "values.{name} names its own state {state}, but the expectation says it came from `from`"
                        ));
                    }
                }
            }
        }
    }

    if let Some(evidence) = &expected.evidence {
        let recorded = record.get("evidence");
        problems.compare(
            "evidence.level",
            Value::String(match evidence.level {
                EvidenceLevel::Published => "published".to_owned(),
                EvidenceLevel::Attested => "attested".to_owned(),
            }),
            recorded.and_then(|recorded| recorded.get("level")),
        );
        if let Some(overridden) = evidence.overridden {
            let actual = field(recorded, "override").is_some_and(|value| !value.is_null());
            if actual != overridden {
                problems.push(format!("evidence.override is {actual} instead of {overridden}"));
            }
        }
        if let Some(attestations) = &evidence.attestations {
            let actual = scoped_names(recorded.and_then(|recorded| recorded.get("attestations")));
            let wanted = attestations.iter().cloned().collect::<BTreeSet<_>>();
            if actual != wanted {
                problems.push(format!("evidence.attestations are {actual:?} instead of {wanted:?}"));
            }
        }
    }

    // An empty list is a key the record writes, not one it leaves out.
    let present = |problems: &mut Problems, what: &str, value: Option<&Value>| {
        if value.is_none() {
            problems.push(format!("{what} is absent"));
        }
    };
    if expected.superseded.is_some() {
        present(&mut problems, "superseded", record.get("superseded"));
    }
    if let Some(excluded) = &expected.excluded_deployments {
        problems.compare(
            "excludedDeployments",
            serde_json::to_value(excluded).expect("a map of names serializes"),
            record.get("excludedDeployments"),
        );
    }
    if expected.ignored_attestations.is_some() {
        present(&mut problems, "ignoredAttestations", record.get("ignoredAttestations"));
    }
    if expected
        .evidence
        .as_ref()
        .is_some_and(|evidence| evidence.attestations.is_some())
    {
        present(
            &mut problems,
            "evidence.attestations",
            record.get("evidence").and_then(|evidence| evidence.get("attestations")),
        );
    }
    if let Some(superseded) = &expected.superseded {
        let actual = record
            .get("superseded")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| {
                        (
                            entry
                                .get("commit")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            entry
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut wanted = Vec::new();
        for entry in superseded {
            if let Some(commit) = problems.resolved("superseded", (lookups.commit)(&entry.commit)) {
                wanted.push((commit, entry.reason.clone()));
            }
        }
        let recorded = record.get("superseded").and_then(Value::as_array);
        for (index, entry) in superseded.iter().enumerate() {
            if let Some(repository) = &entry.repository {
                problems.compare(
                    &format!("superseded[{index}].repository"),
                    Value::String(repository.clone()),
                    recorded
                        .and_then(|entries| entries.get(index))
                        .and_then(|entry| entry.get("repository")),
                );
            }
        }
        if wanted.len() == superseded.len() && actual != wanted {
            problems.push(format!("superseded is {actual:?} instead of {wanted:?}"));
        }
    }
    if let Some(verified) = expected.artifacts_verified {
        problems.compare(
            "artifactsVerified",
            Value::Bool(verified),
            record.get("artifactsVerified"),
        );
    }
    if let Some(requested) = &expected.requested {
        problems.compare(
            "requested.reason",
            Value::String(requested.reason.clone()),
            record.get("requested").and_then(|requested| requested.get("reason")),
        );
    }
    if let Some(ignored) = &expected.ignored_attestations {
        let mut actual = record
            .get("ignoredAttestations")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| {
                        (
                            scoped_name(entry),
                            entry
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut wanted = ignored
            .iter()
            .map(|entry| (entry.attestation.clone(), entry.reason.clone()))
            .collect::<Vec<_>>();
        actual.sort();
        wanted.sort();
        if actual != wanted {
            problems.push(format!("ignoredAttestations are {actual:?} instead of {wanted:?}"));
        }
    }

    if problems.found.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the promotion record of {}: {}",
            expected.path,
            problems.found.join("; ")
        ))
    }
}

/// The differences found while comparing a record.
#[derive(Default)]
struct Problems {
    found: Vec<String>,
}

impl Problems {
    fn push(&mut self, problem: String) {
        self.found.push(problem);
    }

    /// `wanted` against `actual`, where an absent value equals `null`.
    fn compare(&mut self, what: &str, wanted: Value, actual: Option<&Value>) {
        let same = match actual {
            Some(actual) => *actual == wanted,
            None => wanted.is_null(),
        };
        if !same {
            self.push(format!(
                "{what} is {} instead of {wanted}",
                actual.map_or_else(|| "absent".to_owned(), Value::to_string)
            ));
        }
    }

    /// The value of a lookup, or the reason it failed as a problem.
    fn resolved(&mut self, what: &str, result: Result<String, String>) -> Option<String> {
        result.map_err(|error| self.push(format!("{what}: {error}"))).ok()
    }
}

/// `<scope>/<name>` of an attestation entry.
fn scoped_name(entry: &Value) -> String {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default();
    format!("{}/{}", text("scope"), text("name"))
}

fn scoped_names(entries: Option<&Value>) -> BTreeSet<String> {
    entries
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(scoped_name)
        .collect()
}

/// The environment of a step's `-e`, or `dev`.
fn environment_of(args: &[String]) -> String {
    args.iter()
        .position(|arg| arg == "-e" || arg == "--environment")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .unwrap_or_else(|| "dev".to_owned())
}

/// Every `code` a report carries, in findings anywhere in it.
fn collect_codes(value: &Value, codes: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(code)) = map.get("code") {
                codes.insert(code.clone());
            }
            map.values().for_each(|value| collect_codes(value, codes));
        }
        Value::Array(items) => items.iter().for_each(|value| collect_codes(value, codes)),
        _ => {}
    }
}

/// Whether every field of `expected` has the same value in `actual`.
fn subset(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|actual| subset(value, actual))),
        _ => expected == actual,
    }
}

/// A path glob: `**` matches any number of segments, `*` any characters
/// within one segment.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    fn segments(pattern: &[&str], path: &[&str]) -> bool {
        match (pattern.first(), path.first()) {
            (None, None) => true,
            (Some(&"**"), _) => segments(&pattern[1..], path) || (!path.is_empty() && segments(pattern, &path[1..])),
            (Some(part), Some(segment)) => segment_matches(part, segment) && segments(&pattern[1..], &path[1..]),
            _ => false,
        }
    }
    fn segment_matches(pattern: &str, text: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == text,
            Some((prefix, rest)) => {
                let Some(text) = text.strip_prefix(prefix) else {
                    return false;
                };
                (0..=text.len())
                    .filter(|index| text.is_char_boundary(*index))
                    .any(|index| segment_matches(rest, &text[index..]))
            }
        }
    }
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let path = path.split('/').collect::<Vec<_>>();
    segments(&pattern, &path)
}

/// One in-process invocation in a fresh clone of `branch`.
pub fn invoke(world: &World, wiring: &Wiring<'_>, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
    invoke_in(world, wiring, args, branch, "")
}

/// One in-process invocation in `directory` of a fresh clone of `branch`.
pub fn invoke_in(
    world: &World,
    wiring: &Wiring<'_>,
    args: &[String],
    branch: &str,
    directory: &str,
) -> Result<CommandOutcome, String> {
    let mut path = world.fresh_clone(branch);
    if !directory.is_empty() {
        path.push(directory);
    }
    // The project path goes before `--`, so arguments a unit driver takes
    // after it (`nyl build <unit> -- --diff`) stay the driver's own.
    let split = args.iter().position(|arg| arg == "--").unwrap_or(args.len());
    let argv = std::iter::once("nyl".to_owned())
        .chain(args[..split].iter().cloned())
        .chain(std::iter::once(path.display().to_string()))
        .chain(args[split..].iter().cloned());
    let cli = Cli::try_parse_from(argv).map_err(|error| format!("nyl {}: {error}", args.join(" ")))?;
    cli.execute_in_process(wiring)
        .map_err(|error| format!("nyl {}: {error}", args.join(" ")))
}

/// Each unit's outcome in a transition summary's `units:` block: its
/// `result`, or a check's `check`; an operator action has none.
fn summary_units(summary: &Value) -> BTreeMap<String, String> {
    summary
        .get("units")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| Some((name.clone(), outcome_of(value)?.to_owned())))
        .collect()
}

/// The unit names of a list, empty when it is absent.
/// The keys of a map, such as the units of `observationsChanged`.
fn keys(map: Option<&Value>) -> BTreeSet<String> {
    map.and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(key, _)| key.clone())
        .collect()
}

fn names(list: Option<&Value>) -> BTreeSet<String> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// Whether `nyl <args>` is an orchestration command, which runs in-process in
/// tier 1 and prints its report as JSON when asked: the command line's own
/// answer, [`Cli::is_orchestration`]. Arguments that do not parse run
/// in-process, where the parse error is reported.
fn is_orchestration_command(args: &[String]) -> bool {
    let argv = std::iter::once("nyl")
        .chain(args.iter().map(String::as_str))
        .chain(std::iter::once("."));
    match Cli::try_parse_from(argv) {
        Ok(cli) => cli.is_orchestration(),
        Err(_) => true,
    }
}

/// Whether `nyl <args>`, an orchestration command, takes `--output json`:
/// not `build` and `exec`, which pass what follows `--` to the unit driver,
/// and not the listings of declarations (`get environments`), which print
/// a table only.
fn takes_json_output(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("build" | "exec") => false,
        Some("get") => matches!(
            args.get(1).map(String::as_str),
            Some(
                "unit"
                    | "units"
                    | "output"
                    | "artifact"
                    | "observation"
                    | "artifacts"
                    | "states"
                    | "promotion-candidates"
                    | "promotions"
            )
        ),
        _ => true,
    }
}

/// A fresh CI checkout, isolated process environment, and the same injected
/// time as tier 1. Image, infrastructure, and publication drivers are the
/// built-in ones, as in a release build.
fn invoke_binary(shared: &Shared, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
    let checkout = shared.world.fresh_clone(branch);
    super::world::git(&checkout, &["config", "user.name", "Scenario"]);
    super::world::git(&checkout, &["config", "user.email", "scenario@example.invalid"]);
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).map_err(|error| error.to_string())?;
    let orchestration = is_orchestration_command(args);
    let mut command = super::nyl_binary(&checkout, &home, &[]);
    command
        .env_clear()
        .envs(&shared.process_env)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1")
        .env("NYL_TEST_NOW", shared.clock.now().to_rfc3339())
        .args(args);
    // Winsock needs SystemRoot to load its service providers, and the binary
    // makes scratch directories in the temporary one; the rest of the
    // environment stays cleared.
    #[cfg(windows)]
    for name in ["SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let prints_json = orchestration && takes_json_output(args);
    if prints_json && !args.iter().any(|arg| arg == "-o" || arg == "--output") {
        command.args(["--output", "json"]);
    }
    let output = command.output().map_err(|error| format!("run nyl: {error}"))?;
    let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let stderr = String::from_utf8(output.stderr).map_err(|error| error.to_string())?;
    // `build`, `exec`, and the commands outside orchestration print what
    // their unit driver or their own format says, so a report is read only
    // where `--output json` asked for one.
    let report = if prints_json {
        serde_json::from_str(&stdout).map_err(|error| {
            format!(
                "nyl {} did not print JSON: {error}; stdout:\n{stdout}\nstderr:\n{stderr}",
                args.join(" ")
            )
        })?
    } else {
        serde_json::from_str(&stdout).unwrap_or(Value::Null)
    };
    Ok(CommandOutcome {
        exit: output.status.code().unwrap_or(1).try_into().unwrap_or(1),
        report,
        stdout,
        stderr,
    })
}
