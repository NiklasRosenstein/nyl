//! `nyl teardown` and `nyl state delete` in-process, in the scenario test
//! world with the walkthroughs' shared project: Git state in `project.git`
//! and the fake kinds standing in for the OpenTofu, image, and publication
//! units.
//!
//! Contract: design/orchestration-core.md, Deletion ("Explicit teardown"),
//! Instances (`teardown --all`, `state delete`, decommissioning a declared
//! environment), and State lifecycle, as amended by the part 5 decisions
//! (Q14, Q15, Q24, Q54).

mod scenario;

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::driver::DriverOperation;
use nyl_core::orchestration::ids::{CommitId, RunId};
use nyl_core::orchestration::ports::Checkpoint;
use nyl_core::orchestration::ports::Clock as _;
use nyl_core::orchestration::state::{
    FailureCategory, Lease, LeaseKind, Operation, Request, Run, RunKind, StateV1, TeardownWaiting,
};
use nyl_drivers::fake::Scripted;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self, dev, shared_project};
use scenario::world::git_output;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";
const DESIRED: &str = "refs/heads/nyl/dev/desired";
const OTHER_RUN: &str = "00000000-0000-7000-9000-0000000000fe";

struct Harness {
    shell: shell::Harness,
}

impl std::ops::Deref for Harness {
    type Target = shell::Harness;

    fn deref(&self) -> &shell::Harness {
        &self.shell
    }
}

impl Harness {
    fn new() -> Self {
        Self {
            shell: shell::Harness::new(&shared_project(), true),
        }
    }

    /// `nyl <args>` in-process in a fresh clone of `main`.
    fn invoke(&self, args: &[&str]) -> CommandOutcome {
        let tools = ProcessRunner::new();
        let wiring = Wiring {
            clock: self.clock.clone(),
            ids: &self.ids,
            drivers: &self.drivers,
            tools: &tools,
            faults: &NoFaults,
            runner: "test-runner".to_owned(),
            author: Some(AUTHOR.parse().unwrap()),
            process_env: self.process_env.clone(),
            cancellation: Cancellation::default(),
            prompt: None,
            observer: None,
        };
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        invoke_in(&self.world, &wiring, &args, "main", "").unwrap()
    }

    fn init(&self) {
        let outcome = self.invoke(&["state", "init", "-e", "dev"]);
        assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    }

    fn reconcile(&self) -> CommandOutcome {
        self.invoke(&["reconcile", "-e", "dev", "-o", "json"])
    }

    fn delete(&self, extra: &[&str]) -> CommandOutcome {
        let mut args = vec!["state", "delete", "-e", "dev", "--reason", "decommission", "-o", "json"];
        args.extend_from_slice(extra);
        self.invoke(&args)
    }

    fn tip(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["rev-parse", reference])
    }

    /// The files on `reference`.
    fn files(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["ls-tree", "-r", "--name-only", reference])
    }

    fn show(&self, reference: &str, path: &str) -> String {
        git_output(&self.world.bare(), &["show", &format!("{reference}:{path}")])
    }

    fn lease(&self, minutes: i64) -> Lease {
        let now = self.clock.now();
        Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: RunId::new(OTHER_RUN).unwrap(),
            runner: "https://ci.example.com/runs/7".to_owned(),
            operation: Operation::Reconcile,
            units: Vec::new(),
            deadline: now + chrono::Duration::minutes(minutes),
            taken_at: now,
            broken: None,
        }
    }
}

/// Instances: `state delete` refuses while a unit has an incarnation, so
/// it never forgets resources that still exist; the fix names the ways to
/// get rid of them.
#[test]
fn test_state_delete_refuses_while_units_have_incarnations() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    let desired = harness.tip(DESIRED);
    let refused = harness.delete(&[]);
    assert_eq!(refused.exit, 1, "{}", refused.stderr);
    assert_eq!(refused.report["errors"][0]["code"], "NYL-STATE-DELETE-INCARNATIONS");
    let fix = refused.report["errors"][0]["fix"].as_str().unwrap();
    assert!(
        fix.contains("--teardown") && fix.contains("nyl teardown") && fix.contains("nyl state forget"),
        "{fix}"
    );
    assert_eq!(harness.tip(DESIRED), desired);
}

/// Q24: a run ref no run has imported may be the only trace of units its
/// run created, so `state delete` refuses until `reconcile` or `recover`
/// imports it.
#[test]
fn test_state_delete_refuses_while_run_refs_are_unimported() {
    let harness = Harness::new();
    harness.init();
    let run = Run {
        api_version: StateV1,
        kind: RunKind,
        run: RunId::new(OTHER_RUN).unwrap(),
        runner: "https://ci.example.com/runs/7".to_owned(),
        operation: Operation::Reconcile,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: harness.clock.now(),
        units: Vec::new(),
        finished: true,
    };
    harness.store.write_run(&harness.location, &dev(), &run, None).unwrap();
    let refused = harness.delete(&[]);
    assert_eq!(refused.exit, 1, "{}", refused.stderr);
    let fix = refused.report["errors"][0]["fix"].as_str().unwrap();
    assert!(
        fix.contains("nyl reconcile -e dev") && fix.contains("nyl recover -e dev"),
        "{fix}"
    );
}

/// Q14, Q15, Instances: `state delete` writes one commit per ref that
/// removes the environment's other files and leaves `state.yaml` with the
/// deleted marker; the refs stay. Every later command fails with exit 1
/// and NYL-STATE-DELETED naming both ways forward (a pipeline that still
/// runs it fails visibly instead of re-creating anything); a plain `state
/// init` refuses too, and `state init --fresh` starts over. Held markers
/// never stop it (Q19).
#[test]
fn test_state_delete_leaves_the_deleted_marker_and_refuses_later_commands() {
    let harness = Harness::new();
    harness.init();
    let held = harness.invoke(&["hold", "-e", "dev", "--unit", "network"]);
    assert_eq!(held.exit, 0, "{}", held.stderr);
    let (desired, observed) = (harness.tip(DESIRED), harness.tip(OBSERVED));

    let deleted = harness.delete(&[]);
    assert_eq!(deleted.exit, 0, "{}", deleted.stderr);
    assert_eq!(deleted.report["outcome"], "done");
    for (reference, before) in [(DESIRED, &desired), (OBSERVED, &observed)] {
        assert_eq!(
            git_output(
                &harness.world.bare(),
                &["rev-list", "--count", &format!("{before}..{reference}")]
            ),
            "1",
            "one commit on {reference}"
        );
        assert_eq!(harness.files(reference), "state.yaml", "only the marker on {reference}");
        assert!(harness.show(reference, "state.yaml").contains("type: state-deleted"));
    }
    let message = git_output(&harness.world.bare(), &["log", "-1", "--format=%B", DESIRED]);
    assert!(message.contains("Nyl-Operation: state-delete"), "{message}");
    assert!(message.contains("Nyl-Reason: decommission"), "{message}");

    for args in [
        &["reconcile", "-e", "dev"][..],
        &["status", "-e", "dev"],
        &["hold", "-e", "dev", "--unit", "network"],
        &["state", "init", "-e", "dev"],
    ] {
        let refused = harness.invoke(args);
        assert_eq!(refused.exit, 1, "{args:?}: {}", refused.stderr);
        assert!(
            refused.stderr.contains("NYL-STATE-DELETED"),
            "{args:?}: {}",
            refused.stderr
        );
        assert!(
            refused.stderr.contains("remove the Environment") && refused.stderr.contains("--fresh"),
            "{args:?}: {}",
            refused.stderr
        );
    }

    let fresh = harness.invoke(&["state", "init", "-e", "dev", "--fresh"]);
    assert_eq!(fresh.exit, 0, "{}", fresh.stderr);
    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert!(!status.stderr.contains("NYL-STATE-DELETED"), "{}", status.stderr);
}

/// Instances: `state delete --teardown` stops with exit 2 while the
/// publication's `manual` teardown wait (the default of `mode: publish`)
/// waits; once `--confirm-removed` ran phase 2, running it again resumes,
/// tears the rest down, and continues to the deletion in the same command.
/// Held files without an incarnation (the units `--all` holds) do not stop
/// it. Catches a decommission that cannot be resumed after a manual wait.
#[test]
fn test_state_delete_teardown_tears_down_and_deletes() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    let waits = harness.delete(&["--teardown"]);
    assert_eq!(waits.exit, 2, "{}", waits.stderr);
    assert!(harness.files(DESIRED).contains("desired/units/kubernetes.yaml"));
    let confirmed = harness.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "kubernetes",
        "--confirm-removed",
        "--reason",
        "checked by hand",
    ]);
    assert_eq!(confirmed.exit, 0, "{}", confirmed.stderr);
    let deleted = harness.delete(&["--teardown"]);
    assert_eq!(deleted.exit, 0, "{}", deleted.stderr);
    assert_eq!(deleted.report["details"]["teardown"]["kind"], "TeardownReport");
    assert_eq!(harness.files(DESIRED), "state.yaml");
}

/// Q26, Q54: `state delete --teardown` stops after the teardown while a
/// unit keeps an incarnation, and deletes nothing: exit 2 when every
/// remaining unit waits (a held unit only records the teardown request),
/// exit 1 when a teardown failed, keeping the teardown's report.
#[test]
fn test_state_delete_teardown_stops_while_units_remain() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    assert_eq!(harness.invoke(&["hold", "-e", "dev", "--unit", "network"]).exit, 0);
    let waits = harness.delete(&["--teardown"]);
    assert_eq!(waits.exit, 2, "{}", waits.stderr);
    assert_eq!(waits.report["details"]["teardown"]["kind"], "TeardownReport");
    assert!(waits.report["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error["code"] == "NYL-STATE-DELETE-INCARNATIONS"));
    assert!(harness.files(DESIRED).contains("desired/units/network.yaml"));

    assert_eq!(harness.invoke(&["resume", "-e", "dev", "--unit", "network"]).exit, 0);
    // The publication's manual wait gates network, the unit it depends on.
    let confirmed = harness.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "kubernetes",
        "--confirm-removed",
        "--reason",
        "checked by hand",
    ]);
    assert_eq!(confirmed.exit, 0, "{}", confirmed.stderr);
    harness.script.push(
        shell::unit("network"),
        DriverOperation::Teardown,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: false,
        },
        1,
    );
    let failed = harness.delete(&["--teardown"]);
    assert_eq!(failed.exit, 1, "{}", failed.stderr);
    assert!(harness.files(DESIRED).contains("desired/units/network.yaml"));
}

/// Deletion, "Explicit teardown": a unit the state does not hold is an
/// error (exit 1), not a silent no-op.
#[test]
fn test_teardown_of_an_unknown_unit_exits_1() {
    let harness = Harness::new();
    harness.init();
    let outcome = harness.invoke(&["teardown", "-e", "dev", "--unit", "nope", "-o", "json"]);
    assert_eq!(outcome.exit, 1, "{}", outcome.stderr);
    assert_eq!(outcome.report["kind"], "TeardownReport");
    assert_eq!(
        outcome.report["environments"][0]["errors"][0]["code"],
        "NYL-UNIT-UNKNOWN"
    );
}

/// Kubernetes publication unit, the signal path: `--confirm-removed` sent
/// to the run waiting on the unit's teardown is delivered (exit 0) and
/// starts no run of its own.
#[test]
fn test_confirm_removed_is_delivered_to_the_waiting_run() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    let mut lease = harness.lease(30);
    lease.operation = Operation::Teardown;
    lease.units = vec![shell::unit("kubernetes")];
    harness
        .store
        .take_lease(&harness.location, &dev(), &lease, None)
        .unwrap();
    // The run waits on kubernetes's teardown after phase 1: its checkpoint
    // records the wait.
    let snapshot = harness.store.snapshot(&harness.location).unwrap();
    let name = shell::unit("kubernetes");
    let mut observed = snapshot.observed[&name].clone();
    observed.waiting = Some(TeardownWaiting {
        phase: "applications-removed".to_owned(),
        reason: "phase 1 published".to_owned(),
        since: harness.clock.now(),
        until: None,
        phase1_commit: Some(CommitId::new(harness.tip(DESIRED)).unwrap()),
        run: lease.run.clone(),
    });
    let run = Run {
        api_version: StateV1,
        kind: RunKind,
        run: lease.run.clone(),
        runner: lease.runner.clone(),
        operation: Operation::Teardown,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: lease.deadline,
        units: vec![name.clone()],
        finished: false,
    };
    harness.store.write_run(&harness.location, &dev(), &run, None).unwrap();
    let checkpoint = Checkpoint {
        desired: snapshot.desired[&name].clone(),
        observed,
        artifacts: std::collections::BTreeMap::new(),
    };
    harness
        .store
        .write_run(&harness.location, &dev(), &run, Some(&checkpoint))
        .unwrap();
    let desired = harness.tip(DESIRED);
    let outcome = harness.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "kubernetes",
        "--confirm-removed",
        "-o",
        "json",
    ]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.report["environments"][0]["warnings"][0]["code"],
        "NYL-CONFIRMATION-DELIVERED"
    );
    assert_eq!(harness.tip(DESIRED), desired);
}

/// Runs and leases, item 1: `lease break` of a lease that is already broken
/// reports it as such and names the importers.
#[test]
fn test_lease_break_of_a_broken_lease_reports_it_broken() {
    let harness = Harness::new();
    harness.init();
    let mut lease = harness.lease(30);
    lease.broken = Some(Request {
        by: "alice".to_owned(),
        at: harness.clock.now(),
        reason: None,
    });
    harness
        .store
        .take_lease(&harness.location, &dev(), &lease, None)
        .unwrap();
    let outcome = harness.invoke(&["lease", "break", "-e", "dev", "--reason", "again", "-o", "json"]);
    assert_eq!(outcome.exit, 0, "{}", outcome.stderr);
    let note = outcome.report["details"]["note"].as_str().unwrap();
    assert!(
        note.contains("already broken") && note.contains("nyl recover -e dev"),
        "{note}"
    );
}

/// `teardown --hold` whose teardown does not finish in its invocation: the
/// unit is held where it stopped, `resume` lets the next run finish the
/// teardown, and that run holds the unit again, so it is not recreated
/// until it is resumed once more.
#[test]
fn test_an_unfinished_teardown_hold_holds_again_once_it_completes_after_resume() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    harness.script.push(
        shell::unit("network"),
        DriverOperation::Teardown,
        &Scripted::Failed {
            category: FailureCategory::ToolError,
            retryable: true,
        },
        1,
    );
    let failed = harness.invoke(&[
        "teardown",
        "-e",
        "dev",
        "--unit",
        "network",
        "--hold",
        "--reason",
        "pause network",
        "-o",
        "json",
    ]);
    assert_ne!(failed.exit, 0, "{}", failed.stderr);
    let stopped = harness.show(DESIRED, "desired/units/network.yaml");
    assert!(
        stopped.contains("state: held") && stopped.contains("hold: true"),
        "{stopped}"
    );

    let resumed = harness.invoke(&["resume", "-e", "dev", "--unit", "network"]);
    assert_eq!(resumed.exit, 0, "{}", resumed.stderr);
    let finished = harness.reconcile();
    assert_eq!(
        finished.report["environments"][0]["units"]["network"]["result"], "removed",
        "{}",
        finished.stdout
    );
    let held = harness.show(DESIRED, "desired/units/network.yaml");
    assert!(held.contains("state: held") && !held.contains("uid:"), "{held}");

    let next = harness.reconcile();
    assert_eq!(next.exit, 2, "{}", next.stdout);
    assert_eq!(next.report["environments"][0]["units"]["network"]["result"], "held");
}
