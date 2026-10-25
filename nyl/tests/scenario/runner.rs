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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::driver::DriverOperation;
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::CommitIdentity;
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
    DeployExpectation, DriverOperation as StepOperation, DriverOutcome, Expect, FaultPointName, ReceiptExpectation,
    Step, StepKind, UidExpectation, UnitExpectation,
};
use super::world::{git_output, World};
use super::Loaded;

/// The time every scenario starts at.
pub const START: &str = "2026-09-24T10:00:00Z";

/// The commit identity of every state and publication commit.
pub const AUTHOR: &str = "Scenario <scenario@example.invalid>";

/// The outcome keys of `expect` that name sets of units.
const OUTCOME_KEYS: [&str; 6] = [
    "executed",
    "current",
    "blocked",
    "failed",
    "uncertain",
    "awaiting-approval",
];

/// A fault the next `nyl` step dies at, consumed when it fires.
#[derive(Default)]
struct FaultSlot {
    armed: Mutex<Option<(FaultPoint, UnitName)>>,
}

impl FaultInjector for FaultSlot {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        let mut armed = self.armed.lock().expect("fault lock");
        match armed.as_ref() {
            Some((armed_point, armed_unit)) if *armed_point == point && Some(armed_unit) == unit => {
                *armed = None;
                FaultAction::Kill
            }
            _ => FaultAction::Continue,
        }
    }
}

/// Run every step of `loaded`, returning the first failed check.
pub fn run(loaded: &Loaded) -> Result<(), String> {
    let world = World::new(&loaded.directory.join(&loaded.scenario.project));
    let start = chrono::DateTime::parse_from_rfc3339(START)
        .expect("start time")
        .to_utc();
    let clock = Arc::new(FakeClock::new(start));
    let ids = SequentialIds::new();
    let script = Script::new();
    let drivers = nyl_drivers::registry::with_test_kinds(&script);
    let tools = ProcessRunner::new();
    let faults = FaultSlot::default();
    let author: CommitIdentity = AUTHOR.parse().expect("identity");
    let process_env = std::env::var("PATH")
        .map(|path| BTreeMap::from([("PATH".to_owned(), path)]))
        .unwrap_or_default();
    let wiring = Wiring {
        clock: clock.clone(),
        ids: &ids,
        drivers: &drivers,
        tools: &tools,
        faults: &faults,
        runner: "scenario".to_owned(),
        author: Some(author),
        process_env,
    };
    let mut state = RunState {
        world: &world,
        wiring: &wiring,
        uids: BTreeMap::new(),
    };
    for (index, step) in loaded.scenario.steps.iter().enumerate() {
        let context = |error: String| format!("{}: step {}: {error}", loaded.name, index + 1);
        match step.kind().map_err(context)? {
            StepKind::Commit => world.commit(step.commit.as_ref().expect("kind"), &loaded.directory),
            StepKind::Branch => world.branch(step.branch.as_ref().expect("kind")),
            StepKind::SquashMerge => world.squash_merge(step.squash_merge.as_ref().expect("kind")),
            StepKind::DeleteBranch => world.delete_branch(step.delete_branch.as_ref().expect("kind")),
            StepKind::Ref => world.delete_ref(&step.reference.as_ref().expect("kind").delete),
            StepKind::Clock => {
                let step = step.clock.as_ref().expect("kind");
                if let Some(set) = &step.set {
                    let time = chrono::DateTime::parse_from_rfc3339(set)
                        .map_err(|error| context(format!("clock set {set}: {error}")))?;
                    clock.set(time.to_utc());
                }
                if let Some(advance) = &step.advance {
                    let duration = advance
                        .parse::<Duration>()
                        .map_err(|error| context(format!("clock advance {advance}: {error}")))?;
                    clock.advance(duration);
                }
            }
            StepKind::Driver => {
                let step = step.driver.as_ref().expect("kind");
                let unit = UnitName::new(&step.unit).map_err(|error| context(error.to_string()))?;
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
                    DriverOutcome::Failed => Scripted::Failed {
                        category: serde_json::from_value::<FailureCategory>(Value::String(
                            step.category.clone().unwrap_or_default(),
                        ))
                        .map_err(|error| context(format!("category: {error}")))?,
                        retryable: step.retryable.unwrap_or(false),
                    },
                };
                script.push(unit, operation, &result, step.times.unwrap_or(1));
            }
            StepKind::Fault => {
                let step = step.fault.as_ref().expect("kind");
                let point = match step.point {
                    FaultPointName::AfterEffect => FaultPoint::AfterEffect,
                    FaultPointName::BeforeCheckpoint => FaultPoint::BeforeCheckpoint,
                    FaultPointName::BeforeLeaseUpdate => FaultPoint::BeforeLeaseUpdate,
                    FaultPointName::BeforePush => FaultPoint::BeforePush,
                };
                let unit = UnitName::new(&step.unit).map_err(|error| context(error.to_string()))?;
                *faults.armed.lock().expect("fault lock") = Some((point, unit));
            }
            StepKind::Nyl => {
                if step.pause.is_some() {
                    return Err(context("tier 1 does not pause invocations yet".to_owned()));
                }
                state.nyl(step).map_err(context)?;
            }
            kind @ (StepKind::Resume | StepKind::CiAdmit | StepKind::CiWorker | StepKind::CiExport) => {
                return Err(context(format!("tier 1 does not run {kind:?} steps yet")));
            }
        }
    }
    Ok(())
}

struct RunState<'a> {
    world: &'a World,
    wiring: &'a Wiring<'a>,
    /// Each unit's uid as the last `units` check saw it.
    uids: BTreeMap<String, Option<String>>,
}

impl RunState<'_> {
    fn nyl(&mut self, step: &Step) -> Result<(), String> {
        let args = step.nyl.as_ref().expect("kind");
        let environment = environment_of(args);
        let branch = step.at.as_deref().unwrap_or("main");
        let before = self.state_counts(&environment);
        let deploy_before = step
            .expect
            .as_ref()
            .and_then(|expect| expect.deploy.as_ref())
            .map(|deploy| self.tree(&deploy.branch));
        let outcome = self.invoke(args, branch)?;
        let describe = || {
            format!(
                "nyl {}:\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                outcome.stdout,
                outcome.stderr
            )
        };
        if step.killed == Some(true) {
            return Ok(());
        }
        if let Some(exit) = step.exit {
            if i32::from(outcome.exit) != exit {
                return Err(format!("exit {} instead of {exit}; {}", outcome.exit, describe()));
            }
        }
        let Some(expect) = &step.expect else {
            return Ok(());
        };
        let after = self.state_counts(&environment);
        let new_commits = (after.0 - before.0, after.1 - before.1);
        self.check_outcomes(expect, &outcome, args, &environment, new_commits)
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
            for (unit, expectation) in units {
                self.check_unit(unit, expectation, &environment, branch)?;
            }
        }
        if let Some(deploy) = &expect.deploy {
            self.check_deploy(deploy, deploy_before.as_ref().expect("taken before"))?;
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

    /// One in-process invocation in a fresh clone of `branch`.
    fn invoke(&self, args: &[String], branch: &str) -> Result<CommandOutcome, String> {
        let clone = self.world.fresh_clone(branch);
        let argv = std::iter::once("nyl".to_owned())
            .chain(args.iter().cloned())
            .chain(std::iter::once(clone.display().to_string()));
        let cli = Cli::try_parse_from(argv).map_err(|error| format!("nyl {}: {error}", args.join(" ")))?;
        cli.execute_in_process(self.wiring)
            .map_err(|error| format!("nyl {}: {error}", args.join(" ")))
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

    fn check_outcomes(
        &self,
        expect: &Expect,
        outcome: &CommandOutcome,
        args: &[String],
        environment: &str,
        new_commits: (usize, usize),
    ) -> Result<(), String> {
        let expected = [
            &expect.executed,
            &expect.current,
            &expect.blocked,
            &expect.failed,
            &expect.uncertain,
            &expect.awaiting_approval,
        ];
        if expected.iter().all(|set| set.is_none()) {
            return Ok(());
        }
        let units = outcome
            .report
            .pointer("/environments/0/units")
            .and_then(Value::as_object)
            .ok_or("the report has no units")?;
        let outcome_of = |unit: &Value| {
            unit.get("result")
                .or_else(|| unit.get("state"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let reconcile = args.first().is_some_and(|command| command == "reconcile");
        let summary = if reconcile && (new_commits.0 > 0 || new_commits.1 > 0) {
            let side = if new_commits.0 > 0 { "desired" } else { "observed" };
            Some(self.commit_units(environment, side)?)
        } else {
            None
        };
        for (key, expected) in OUTCOME_KEYS.iter().zip(expected) {
            let Some(expected) = expected else { continue };
            let expected = expected.iter().cloned().collect::<BTreeSet<_>>();
            let reported = units
                .iter()
                .filter(|(_, unit)| outcome_of(unit).as_deref() == Some(*key))
                .map(|(name, _)| name.clone())
                .collect::<BTreeSet<_>>();
            if reported != expected {
                return Err(format!("{key}: the report has {reported:?} instead of {expected:?}"));
            }
            if let Some(summary) = &summary {
                let listed = summary
                    .iter()
                    .filter(|(_, result)| result.as_str() == *key)
                    .map(|(name, _)| name.clone())
                    .collect::<BTreeSet<_>>();
                if listed != expected {
                    return Err(format!(
                        "{key}: the transition commit summary lists {listed:?} instead of {expected:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Each unit's result in the `units:` block of the latest transition
    /// commit on the environment's `side` ref.
    fn commit_units(&self, environment: &str, side: &str) -> Result<BTreeMap<String, String>, String> {
        let message = git_output(
            &self.world.bare(),
            &[
                "log",
                "-1",
                "--format=%B",
                &format!("refs/heads/nyl/{environment}/{side}"),
            ],
        );
        let body = message
            .split("\n\n")
            .nth(1)
            .ok_or_else(|| format!("the transition commit has no summary:\n{message}"))?;
        let summary: Value = serde_saphyr::from_str(body).map_err(|error| format!("summary {body:?}: {error}"))?;
        Ok(summary
            .get("units")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(name, value)| Some((name.clone(), value.get("result")?.as_str()?.to_owned())))
            .collect())
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
