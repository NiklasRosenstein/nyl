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
use nyl_drivers::fake::{Script, Scripted};
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint};
use nyl_orchestration::rules::unit_state::receipt_state;
use nyl_orchestration::run_report::ReceiptState;
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use serde_json::Value;

use super::format::{
    DeployExpectation, DriverOperation as StepOperation, DriverOutcome, Expect, PointName, ReceiptExpectation, Step,
    StepKind, UidExpectation, UnitExpectation,
};
use super::world::{git_output, World};
use super::Loaded;

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
pub const OUTCOME_KEYS: [&str; 13] = [
    "executed",
    "current",
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
            observer: None,
        }
    }
}

/// Run every step of `loaded`, returning the first failed check.
pub fn run(loaded: &Loaded) -> Result<(), String> {
    run_tier(loaded, 1)
}

/// Run the same scenario and assertions through separate binary processes.
#[cfg(feature = "tier2")]
pub fn run_binary(loaded: &Loaded) -> Result<(), String> {
    run_tier(loaded, 2)
}

fn run_tier(loaded: &Loaded, tier: u8) -> Result<(), String> {
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
    let shared = Shared {
        world,
        clock: Arc::new(FakeClock::new(start)),
        ids: SequentialIds::new(),
        drivers: nyl_drivers::registry::with_test_kinds(&script),
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
    environment: String,
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
            StepKind::DeleteBranch => self.world.delete_branch(step.delete_branch.as_ref().expect("kind")),
            StepKind::Ref => self.world.delete_ref(&step.reference.as_ref().expect("kind").delete),
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
                    StepOperation::Verify => DriverOperation::Verify,
                    StepOperation::Teardown => DriverOperation::Teardown,
                };
                let result = match step.outcome {
                    DriverOutcome::Succeeded => Scripted::Succeeded {
                        outputs: step.outputs.clone(),
                    },
                    DriverOutcome::Uncertain => Scripted::Uncertain,
                    DriverOutcome::Drifted => Scripted::Drifted,
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
                let before = self.before(args, step.expect.as_ref());
                let outcome = self.invoke(args, branch)?;
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
        let before = self.before(args, resume.and_then(|step| step.expect.as_ref()));
        let branch = step.at.as_deref().unwrap_or("main");
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
            invoke(world, &wiring, args, branch)
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

    /// What `check` compares a step's result against, taken before it runs.
    fn before(&self, args: &[String], expect: Option<&Expect>) -> Before {
        let environment = environment_of(args);
        Before {
            counts: self.state_counts(&environment),
            written: self.written.get(&environment).copied().unwrap_or_default(),
            deploy: expect
                .and_then(|expect| expect.deploy.as_ref())
                .map(|deploy| self.tree(&deploy.branch)),
            environment,
        }
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
        let args = invoked.nyl.as_ref().expect("kind");
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
        let Some(expect) = &checked.expect else {
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
        check_results(expect, &outcome.report, (!local).then_some(summaries.as_slice()))
            .map_err(|error| format!("{error}; {}", describe()))?;
        if let Some(commits) = &expect.commits {
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
            self.check_deploy(deploy, before)?;
        }
        if let Some(get) = &expect.get {
            let outcome = self.invoke(&get.args, branch)?;
            if let Some(value) = &get.value {
                let printed = Value::String(outcome.stdout.trim().to_owned());
                if &outcome.report != value && &printed != value {
                    return Err(format!(
                        "get {:?} gave {} (printed {:?}) instead of {value}",
                        get.args, outcome.report, outcome.stdout
                    ));
                }
            }
            for (pointer, value) in get.json.iter().flatten() {
                if outcome.report.pointer(pointer) != Some(value) {
                    return Err(format!(
                        "get {:?}: {pointer} is {:?} instead of {value}",
                        get.args,
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

    /// One sequential in-process invocation, without faults beyond the slot.
    fn invoke(&self, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
        if self.shared.tier == 2 {
            return invoke_binary(self.shared, args, branch);
        }
        let wiring = self.shared.wiring(&self.shared.faults);
        invoke(self.world, &wiring, args, branch)
    }
    /// Commits on the environment's desired and observed refs.
    fn state_counts(&self, environment: &str) -> (usize, usize) {
        let count = |side: &str| {
            let reference = format!("refs/heads/nyl/{environment}/{side}");
            let exists = std::process::Command::new("git")
                .current_dir(self.world.bare())
                .args(["rev-parse", "--verify", "--quiet", &reference])
                .output()
                .is_ok_and(|output| output.status.success());
            if exists {
                git_output(&self.world.bare(), &["rev-list", "--count", &reference])
                    .parse()
                    .expect("count")
            } else {
                0
            }
        };
        (count("desired"), count("observed"))
    }

    /// Path to blob of every file on `branch`, empty when it does not exist.
    fn tree(&self, branch: &str) -> BTreeMap<String, String> {
        let output = std::process::Command::new("git")
            .current_dir(self.world.bare())
            .args(["ls-tree", "-r", &format!("refs/heads/{branch}")])
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (meta, path) = line.split_once('\t')?;
                Some((path.to_owned(), meta.split_whitespace().nth(2)?.to_owned()))
            })
            .collect()
    }

    /// The YAML summaries of the step's `count` newest transition commits
    /// on the environment's `side` ref, newest first.
    fn commit_summaries(&self, environment: &str, side: &str, count: usize) -> Result<Vec<Value>, String> {
        let log = git_output(
            &self.world.bare(),
            &[
                "log",
                &format!("-{count}"),
                "--format=%B%x00",
                &format!("refs/heads/nyl/{environment}/{side}"),
            ],
        );
        log.split('\0')
            .map(str::trim)
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

    fn check_deploy(&self, deploy: &DeployExpectation, before: &BTreeMap<String, String>) -> Result<(), String> {
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
        Ok(())
    }
}

/// Whether `expect` names a key checked against the transition summary.
fn reads_summary(expect: &Expect) -> bool {
    outcome_sets(expect).iter().any(|(_, set)| set.is_some())
        || list_keys(expect).iter().any(|(_, list)| list.is_some())
        || expect.lease_broken.is_some()
}

/// The outcome keys of `expect` with the sets they name, in [`OUTCOME_KEYS`] order.
fn outcome_sets(expect: &Expect) -> [(&'static str, &Option<Vec<String>>); 13] {
    let sets = [
        &expect.executed,
        &expect.current,
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
fn list_keys(expect: &Expect) -> [(&'static str, &Option<Vec<String>>); 4] {
    [
        ("recovered", &expect.recovered),
        ("imported", &expect.imported),
        ("driftRepaired", &expect.drift_repaired),
        ("reapplied", &expect.reapplied),
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
/// (reconcile, teardown), `state` (plan, status), or `verification` (verify),
/// as `UnitReport::outcome_name` orders them.
fn outcome_of(unit: &Value) -> Option<&str> {
    ["result", "state", "verification"]
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
/// - `recovered`, `imported`, `driftRepaired`, and `reapplied` compare
///   against the newest summary's list (`reapplied`'s `units`) and the
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
        let reported = names(reported);
        if reported != expected {
            return Err(format!("{key}: the report lists {reported:?} instead of {expected:?}"));
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
    let argv = std::iter::once("nyl".to_owned())
        .chain(args.iter().cloned())
        .chain(std::iter::once(path.display().to_string()));
    let cli = Cli::try_parse_from(argv).map_err(|error| format!("nyl {}: {error}", args.join(" ")))?;
    cli.execute_in_process(wiring)
        .map_err(|error| format!("nyl {}: {error}", args.join(" ")))
}

/// Each unit's outcome in a transition summary's `units:` block: its
/// `result`, or a verify's `verification`; an operator action has none.
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
fn names(list: Option<&Value>) -> BTreeSet<String> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// A fresh CI checkout, isolated process environment, and the same injected
/// time as tier 1. Image and infrastructure drivers are real; publication
/// remains fake until M5.
fn invoke_binary(shared: &Shared, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
    let checkout = shared.world.fresh_clone(branch);
    super::world::git(&checkout, &["config", "user.name", "Scenario"]);
    super::world::git(&checkout, &["config", "user.email", "scenario@example.invalid"]);
    let home = checkout.with_extension("home");
    std::fs::create_dir_all(&home).map_err(|error| error.to_string())?;
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
    if !args.iter().any(|arg| arg == "-o" || arg == "--output") {
        command.args(["--output", "json"]);
    }
    let output = command.output().map_err(|error| format!("run nyl: {error}"))?;
    let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let stderr = String::from_utf8(output.stderr).map_err(|error| error.to_string())?;
    let report = serde_json::from_str(&stdout).map_err(|error| {
        format!(
            "nyl {} did not print JSON: {error}; stdout:\n{stdout}\nstderr:\n{stderr}",
            args.join(" ")
        )
    })?;
    Ok(CommandOutcome {
        exit: output.status.code().unwrap_or(1).try_into().unwrap_or(1),
        report,
        stdout,
        stderr,
    })
}
