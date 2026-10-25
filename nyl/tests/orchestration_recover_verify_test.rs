//! `nyl recover` and `nyl verify` in-process, in the scenario test world
//! with the walkthroughs' shared project: Git state in `project.git`, the
//! fake kinds standing in for the OpenTofu, image, and publication units,
//! and a fault injector to crash a run where a test needs a dead run.

mod scenario;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser as _;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::driver::{DriverOperation, DriverSet};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Clock as _, CommitIdentity, StateStore, Transition, TransitionChanges,
};
use nyl_core::orchestration::state::{
    Condition, FailureCategory, Lease, LeaseKind, ObservedUnit, Operation, Run, RunKind, StateLocation, StateV1,
};
use nyl_core::orchestration::time::Duration;
use nyl_drivers::fake::{Script, Scripted};
use nyl_drivers::process::ProcessRunner;
use nyl_git::GitAuth;
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint, NoFaults};
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_state::git::{CommitAuthor, GitStateStore};
use scenario::world::{git_output, World};
use serde_json::Value;

const AUTHOR: &str = "Scenario <scenario@example.invalid>";
const OBSERVED: &str = "refs/heads/nyl/dev/observed";
const DESIRED: &str = "refs/heads/nyl/dev/desired";

fn project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios/walkthroughs/_project")
}

fn unit(name: &str) -> UnitName {
    UnitName::new(name).unwrap()
}

fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

/// Kills a run at one fault point, for one unit or for the unit-less call.
struct KillAt {
    point: FaultPoint,
    unit: Option<&'static str>,
}

impl FaultInjector for KillAt {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        if point == self.point && unit.map(UnitName::as_str) == self.unit {
            FaultAction::Kill
        } else {
            FaultAction::Continue
        }
    }
}

/// One test world: the project, a clock tests advance, and the fake kinds.
struct Harness {
    world: World,
    clock: Arc<FakeClock>,
    ids: SequentialIds,
    script: Script,
    drivers: DriverSet,
    tools: ProcessRunner,
}

impl Harness {
    fn new() -> Self {
        let start = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        let script = Script::new();
        let drivers = nyl_drivers::registry::with_test_kinds(&script);
        Self {
            world: World::new(&project()),
            clock: Arc::new(FakeClock::new(start)),
            ids: SequentialIds::new(),
            script,
            drivers,
            tools: ProcessRunner::new(),
        }
    }

    /// `nyl <args>` in-process in a fresh clone of `main`, with `faults`.
    fn invoke_with(&self, args: &[&str], faults: &dyn FaultInjector) -> CommandOutcome {
        let clone = self.world.fresh_clone("main");
        let process_env = std::env::var("PATH")
            .map(|path| BTreeMap::from([("PATH".to_owned(), path)]))
            .unwrap_or_default();
        let wiring = Wiring {
            clock: self.clock.clone(),
            ids: &self.ids,
            drivers: &self.drivers,
            tools: &self.tools,
            faults,
            runner: "test-runner".to_owned(),
            author: Some(AUTHOR.parse().unwrap()),
            process_env,
        };
        let argv = std::iter::once("nyl".to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .chain(std::iter::once(clone.display().to_string()));
        Cli::try_parse_from(argv).unwrap().execute_in_process(&wiring).unwrap()
    }

    fn invoke(&self, args: &[&str]) -> CommandOutcome {
        self.invoke_with(args, &NoFaults)
    }

    fn init(&self) {
        let outcome = self.invoke(&["state", "init", "-e", "dev"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    }

    fn reconcile(&self, args: &[&str]) -> CommandOutcome {
        let mut argv = vec!["reconcile", "-e", "dev", "-o", "json"];
        argv.extend_from_slice(args);
        self.invoke(&argv)
    }

    fn tip(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["rev-parse", reference])
    }

    fn message(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["log", "-1", "--format=%B", reference])
    }

    /// The coordination refs of `dev` beneath `kind`, such as `runs`.
    fn coordination_refs(&self, kind: &str) -> String {
        git_output(
            &self.world.bare(),
            &["for-each-ref", "--format=%(refname)", &format!("refs/nyl/dev/{kind}/")],
        )
    }

    fn show(&self, reference: &str, path: &str) -> String {
        git_output(&self.world.bare(), &["show", &format!("{reference}:{path}")])
    }

    /// A state store of its own on a fresh clone, to stage state the
    /// commands then work on.
    fn store(&self) -> (GitStateStore, StateLocation) {
        let clone = self.world.fresh_clone("main");
        let identity: CommitIdentity = AUTHOR.parse().unwrap();
        let store = GitStateStore::new(
            clone.join(".git"),
            GitAuth::none(),
            CommitAuthor {
                identity,
                clock: self.clock.clone(),
            },
        );
        let location = StateLocation {
            repository: self.world.project_url(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        };
        (store, location)
    }
}

fn unit_field<'a>(report: &'a Value, name: &str, field: &str) -> &'a Value {
    &report["environments"][0]["units"][name][field]
}

/// `recover --retry` clears a non-retryable failure in one observed commit
/// that records who asked and why, never executes, and the next reconcile
/// executes the unit and its dependent.
#[test]
fn test_recover_retry_writes_one_observed_commit_and_the_next_reconcile_executes_the_unit() {
    let harness = Harness::new();
    harness.init();
    harness.script.push(
        unit("web-image"),
        DriverOperation::Reconcile,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: false,
        },
        1,
    );
    let failed = harness.reconcile(&[]);
    assert_eq!(failed.exit, 3, "{}", failed.stderr);
    let desired = harness.tip(DESIRED);
    let observed = harness.tip(OBSERVED);

    let outcome = harness.invoke(&[
        "recover",
        "-e",
        "dev",
        "--unit",
        "web-image",
        "--retry",
        "--reason",
        "the registry rejected nothing",
        "-o",
        "json",
    ]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(outcome.report["outcome"], "done");
    assert_eq!(outcome.report["details"]["condition"]["state"], "failed");
    assert_eq!(harness.tip(DESIRED), desired, "recover writes only observed state");
    assert_eq!(
        git_output(
            &harness.world.bare(),
            &["rev-list", "--count", &format!("{observed}..{OBSERVED}")]
        ),
        "1"
    );
    let message = harness.message(OBSERVED);
    for expected in [
        "nyl: recover dev (web-image: retry)",
        "web-image: {\"action\":\"retry\"}",
        "recovered: [web-image]",
        "Nyl-Operation: recover",
        &format!("Nyl-Requested-By: {AUTHOR}"),
        "Nyl-Reason: the registry rejected nothing",
    ] {
        assert!(message.contains(expected), "{expected:?} missing in\n{message}");
    }
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");

    let again = harness.invoke(&["recover", "-e", "dev", "--unit", "web-image", "--retry", "-o", "json"]);
    assert_eq!(again.exit, 1);
    assert_eq!(again.report["errors"][0]["code"], "NYL-RECOVER-NOTHING");

    let next = harness.reconcile(&[]);
    assert_eq!(next.exit, 0, "{}", next.stderr);
    assert_eq!(unit_field(&next.report, "web-image", "result"), "executed");
    assert_eq!(unit_field(&next.report, "kubernetes", "result"), "executed");
}

/// `recover` takes the lease like every state-writing operation: while a
/// run holds it, recover exits 2 and writes nothing.
#[test]
fn test_recover_exits_2_while_another_run_holds_the_lease() {
    let harness = Harness::new();
    harness.init();
    let killed = harness.invoke_with(
        &["reconcile", "-e", "dev"],
        &KillAt {
            point: FaultPoint::BeforePush,
            unit: None,
        },
    );
    assert_ne!(killed.exit, 0);
    let observed = harness.tip(OBSERVED);
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "network", "--retry", "-o", "json"]);
    assert_eq!(outcome.exit, 2, "{}", outcome.stderr);
    assert_eq!(outcome.report["errors"][0]["code"], "NYL-LEASE-HELD");
    assert_eq!(harness.tip(OBSERVED), observed);
}

/// A dead run that crashed after an effect, before its checkpoint.
fn crash_after_effect(harness: &Harness) {
    harness.init();
    harness.script.push(
        unit("web-image"),
        DriverOperation::Reconcile,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: false,
        },
        1,
    );
    assert_eq!(harness.reconcile(&["--unit", "web-image"]).exit, 3);
    let killed = harness.invoke_with(
        &["reconcile", "-e", "dev", "--unit", "network"],
        &KillAt {
            point: FaultPoint::AfterEffect,
            unit: Some("network"),
        },
    );
    assert_ne!(killed.exit, 0);
    assert!(
        !harness.coordination_refs("runs").is_empty(),
        "the dead run keeps its run ref"
    );
    // The dead run's lease and run deadline pass.
    harness.clock.advance(Duration::from_mins(180));
}

/// `recover` never consumes a dead run's ref: it is the only record of the
/// unit the run was executing when it died, which the next reconcile's
/// takeover marks uncertain. Recovering an unrelated unit keeps it.
#[test]
fn test_recover_keeps_the_run_ref_of_a_dead_run() {
    let harness = Harness::new();
    crash_after_effect(&harness);
    let runs = harness.coordination_refs("runs");
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "web-image", "--retry"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(harness.coordination_refs("runs"), runs);
}

/// After `recover` on an unrelated unit, the next reconcile still takes
/// over the dead run: it consumes its run ref and recovers the unit that
/// run was executing instead of treating it as never started.
#[test]
#[ignore = "needs run takeover (package takeover of M3 part 4)"]
fn test_reconcile_after_recover_takes_over_the_dead_run() {
    let harness = Harness::new();
    crash_after_effect(&harness);
    let outcome = harness.invoke(&["recover", "-e", "dev", "--unit", "web-image", "--retry"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let next = harness.reconcile(&["--unit", "network"]);
    let result = unit_field(&next.report, "network", "result");
    assert!(
        result == "recovered" || result == "uncertain",
        "{result}: {}",
        next.stdout
    );
    assert!(harness.coordination_refs("runs").is_empty());
}

/// `--accept` adopts the receipt a late runner checkpointed for an
/// uncertain unit, shows the checkpoint as acceptable evidence first, and
/// deletes the evidence ref once no uncertain unit needs it.
#[test]
fn test_accept_adopts_a_late_checkpoint_and_deletes_its_evidence_ref() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile(&[]).exit, 0);

    // Stage what a takeover leaves: network uncertain, and the late run's
    // checkpoint of it on an evidence ref.
    let (store, location) = harness.store();
    let snapshot = store.snapshot(&location).unwrap();
    let network = unit("network");
    let late = RunId::new("00000000-0000-7000-9000-0000000000ff").unwrap();
    let now = harness.clock.now();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("00000000-0000-7000-9000-0000000000fe").unwrap(),
        runner: "staging".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: Duration::from_mins(30).after(now),
        taken_at: now,
        broken: None,
    };
    let version = store.take_lease(&location, &dev(), &lease, None).unwrap();
    let current = snapshot.observed[&network].clone();
    let uncertain = ObservedUnit {
        condition: Some(Condition::Uncertain {
            since: now,
            run: late.clone(),
            reason: "run lost its lease".to_owned(),
        }),
        ..current.clone()
    };
    let committed = store
        .commit(
            &location,
            &dev(),
            &snapshot.tips,
            &Transition {
                message: "nyl: reconcile dev (1 uncertain)\n\nNyl-Operation: reconcile\n".to_owned(),
                changes: TransitionChanges {
                    observed: BTreeMap::from([(network.clone(), Some(uncertain))]),
                    ..TransitionChanges::default()
                },
            },
            &version,
        )
        .unwrap();
    let mut late_receipt = current.receipt.clone().unwrap();
    late_receipt.run = late.clone();
    late_receipt.finished_at = now;
    let record = Run {
        api_version: StateV1,
        kind: RunKind,
        run: late.clone(),
        runner: "late-runner".to_owned(),
        operation: Operation::Reconcile,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: now,
        units: Vec::new(),
        finished: true,
    };
    let checkpoint = Checkpoint {
        desired: snapshot.desired[&network].clone(),
        observed: ObservedUnit {
            receipt: Some(late_receipt),
            condition: None,
            ..current.clone()
        },
        artifacts: BTreeMap::new(),
    };
    store.write_run(&location, &dev(), &record, Some(&checkpoint)).unwrap();
    store.retire_run(&location, &dev(), &late).unwrap();
    store
        .release_lease(&location, &dev(), &committed.lease_version)
        .unwrap();
    assert!(!harness.coordination_refs("evidence").is_empty());

    let unknown = harness.invoke(&[
        "recover", "-e", "dev", "--unit", "network", "--accept", "nope", "--reason", "r", "-o", "json",
    ]);
    assert_eq!(unknown.exit, 1);
    assert_eq!(unknown.report["errors"][0]["code"], "NYL-RECOVER-CHECKPOINT-MISSING");

    let outcome = harness.invoke(&[
        "recover",
        "-e",
        "dev",
        "--unit",
        "network",
        "--accept",
        late.as_str(),
        "--reason",
        "the runner finished after losing its lease",
        "-o",
        "json",
    ]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let evidence = &outcome.report["details"]["evidence"][0];
    assert_eq!(evidence["run"], late.as_str());
    assert_eq!(
        (evidence["result"].as_str(), evidence["acceptable"].as_bool()),
        (Some("receipt"), Some(true))
    );
    let observed = harness.show(OBSERVED, "observed/units/network.yaml");
    assert!(
        observed.contains(late.as_str()) && !observed.contains("uncertain"),
        "{observed}"
    );
    assert!(harness
        .message(OBSERVED)
        .contains(&format!("\"action\":\"accept\",\"run\":\"{late}\"")));
    assert!(
        harness.coordination_refs("evidence").is_empty(),
        "the evidence ref is deleted"
    );
}

/// `verify` records a drift observation per verified unit in one observed
/// commit and releases the lease; units without a current receipt are
/// reported (exit 2). An unchanged second verify writes nothing, a verify
/// after every unit has a receipt exits 0, and a change outside Nyl is
/// recorded as drift (exit 3).
#[test]
fn test_verify_records_observations_once_and_reports_drift() {
    let harness = Harness::new();
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
    assert_eq!(harness.reconcile(&[]).exit, 3);
    let reconciled = harness.tip(OBSERVED);

    let partial = harness.invoke(&["verify", "-e", "dev", "-o", "json"]);
    assert_eq!(partial.exit, 2, "{}", partial.stderr);
    assert_eq!(unit_field(&partial.report, "network", "verification"), "clean");
    assert_eq!(unit_field(&partial.report, "web-image", "verification"), "no-receipt");
    assert_eq!(unit_field(&partial.report, "kubernetes", "verification"), "no-receipt");
    assert!(partial.stderr.contains("NYL-VERIFY-NO-RECEIPT"), "{}", partial.stderr);
    let verified = harness.tip(OBSERVED);
    assert_eq!(
        git_output(
            &harness.world.bare(),
            &["rev-list", "--count", &format!("{reconciled}..{OBSERVED}")]
        ),
        "1"
    );
    let message = harness.message(OBSERVED);
    assert!(
        message.starts_with("nyl: verify dev (2 clean, 2 no-receipt)"),
        "{message}"
    );
    assert!(message.contains("Nyl-Operation: verify"), "{message}");
    assert!(!message.contains("Nyl-Requested-By"), "{message}");
    let drift = harness.show(OBSERVED, "observed/units/network/observations/drift.yaml");
    assert!(
        drift.contains("kind: Observation") && drift.contains("drift: clean"),
        "{drift}"
    );
    assert!(harness.coordination_refs("").is_empty(), "the lease is released");

    harness.clock.advance(Duration::from_mins(60));
    let again = harness.invoke(&["verify", "-e", "dev"]);
    assert_eq!(again.exit, 2, "{}", again.stderr);
    assert_eq!(harness.tip(OBSERVED), verified, "an unchanged verify writes nothing");

    assert_eq!(harness.reconcile(&[]).exit, 0);
    let clean = harness.invoke(&["verify", "-e", "dev", "-o", "json"]);
    assert_eq!(clean.exit, 0, "{}", clean.stderr);
    for name in ["network", "database", "web-image", "kubernetes"] {
        assert_eq!(unit_field(&clean.report, name, "verification"), "clean", "{name}");
    }

    // Someone changes the network's state outside Nyl.
    let backend = harness.world.scenario_tmp().join("tofu/dev-network.tfstate");
    std::fs::write(&backend, r#"{"inputs": {"cidr": "10.1.0.0/16"}, "outputs": {}}"#).unwrap();
    let drifted = harness.invoke(&["verify", "-e", "dev", "--unit", "network", "-o", "json"]);
    assert_eq!(drifted.exit, 3, "{}", drifted.stderr);
    assert_eq!(unit_field(&drifted.report, "network", "verification"), "drifted");
    assert_eq!(drifted.report["environments"][0]["units"].as_object().unwrap().len(), 1);
    assert!(drifted.stderr.contains("NYL-VERIFY-DRIFT"), "{}", drifted.stderr);
    assert!(harness
        .show(OBSERVED, "observed/units/network/observations/drift.yaml")
        .contains("drift: drifted"));
}
