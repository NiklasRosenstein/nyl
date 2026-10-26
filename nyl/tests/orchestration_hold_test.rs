//! `nyl hold` and `nyl resume` in-process, in the scenario test world with
//! the walkthroughs' shared project: Git state in `project.git` and the fake
//! kinds standing in for the OpenTofu, image, and publication units.
//!
//! Contract: design/orchestration-core.md, Deletion ("Holds freeze a
//! unit"), Recovery ("Holds on uncertain units"), and Transition commits
//! (operations `hold` and `resume` with `Nyl-Requested-By`/`Nyl-Reason`),
//! as amended by the part 5 decisions (Q16, Q19, Q25, Q26, Q28).

mod scenario;

use std::collections::BTreeMap;

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::ports::{Clock as _, Transition, TransitionChanges};
use nyl_core::orchestration::state::{
    Condition, Deletion, DeletionIntent, DeletionReason, Lease, LeaseKind, Lifecycle, Operation, SignalAction, StateV1,
    TeardownProgress,
};
use nyl_core::orchestration::time::Duration;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self, dev, shared_project, unit};
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

    fn hold(&self, unit: &str) -> CommandOutcome {
        self.invoke(&[
            "hold",
            "-e",
            "dev",
            "--unit",
            unit,
            "--reason",
            "incident 4711",
            "-o",
            "json",
        ])
    }

    fn resume(&self, unit: &str) -> CommandOutcome {
        self.invoke(&["resume", "-e", "dev", "--unit", unit, "-o", "json"])
    }

    fn tip(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["rev-parse", reference])
    }

    fn message(&self, reference: &str) -> String {
        git_output(&self.world.bare(), &["log", "-1", "--format=%B", reference])
    }

    /// A lease of another runner's run, live for `minutes`.
    fn other_lease(&self, minutes: i64) -> Lease {
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

    /// Write `changes` as another run would, under a lease of its own that
    /// is released afterwards.
    fn stage(&self, changes: TransitionChanges) {
        let (store, location) = (self.store.as_ref(), &self.location);
        let snapshot = store.snapshot(location).unwrap();
        let version = store.take_lease(location, &dev(), &self.other_lease(30), None).unwrap();
        let committed = store
            .commit(
                location,
                &dev(),
                &snapshot.tips,
                &Transition {
                    message: "nyl: reconcile dev (staged)\n\nNyl-Operation: reconcile\n".to_owned(),
                    changes,
                },
                &version,
            )
            .unwrap();
        store.release_lease(location, &dev(), &committed.lease_version).unwrap();
    }
}

/// Q19, Q25, Transition commits: holding a unit never reconciled writes a
/// held marker in one desired commit with the operation and the trailers;
/// holding it again and resuming an unheld unit write nothing and exit 0;
/// resume writes one desired commit and keeps the file without a uid.
/// Catches a hold that renders source, invents a uid, or commits a no-op.
#[test]
fn test_hold_and_resume_write_one_desired_commit_each_and_nothing_when_unchanged() {
    let harness = Harness::new();
    harness.init();
    let observed = harness.tip(OBSERVED);
    let desired = harness.tip(DESIRED);

    let held = harness.hold("network");
    assert_eq!(held.exit, 0, "{}", held.stderr);
    assert_eq!(held.report["outcome"], "done");
    assert_eq!(held.report["details"]["marker"], true);
    assert_eq!(harness.tip(OBSERVED), observed, "a hold writes desired state only");
    assert_eq!(
        git_output(
            &harness.world.bare(),
            &["rev-list", "--count", &format!("{desired}..{DESIRED}")]
        ),
        "1"
    );
    let message = harness.message(DESIRED);
    for expected in [
        "nyl: hold dev (network)",
        "network: {\"action\":\"hold\",\"reason\":\"incident 4711\"}",
        "Nyl-Operation: hold",
        &format!("Nyl-Requested-By: {AUTHOR}"),
        "Nyl-Reason: incident 4711",
    ] {
        assert!(message.contains(expected), "{expected:?} missing in\n{message}");
    }
    let marker = &harness.snapshot().desired[&unit("network")];
    assert!(marker.is_held_marker());
    assert_eq!(marker.unit.uid, None);
    assert!(
        matches!(&marker.lifecycle, Lifecycle::Held { hold, .. } if hold.reason.as_deref() == Some("incident 4711"))
    );

    let tip = harness.tip(DESIRED);
    let again = harness.hold("network");
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_eq!(again.report["outcome"], "unchanged");
    assert_eq!(harness.tip(DESIRED), tip);

    let resumed = harness.resume("network");
    assert_eq!(resumed.exit, 0, "{}", resumed.stderr);
    assert!(harness.message(DESIRED).contains("Nyl-Operation: resume"));
    let resumed_unit = &harness.snapshot().desired[&unit("network")];
    assert_eq!(resumed_unit.lifecycle, Lifecycle::Active);
    assert_eq!(resumed_unit.unit.uid, None, "resume lifts the hold and nothing else");

    let tip = harness.tip(DESIRED);
    let not_held = harness.resume("network");
    assert_eq!(not_held.exit, 0);
    assert_eq!(not_held.report["outcome"], "unchanged");
    assert_eq!(harness.tip(DESIRED), tip);
}

/// Q19, Q26: reconcile excludes a held marker (its dependents stay
/// blocked), and after `resume` the next reconcile creates the unit as a
/// new incarnation.
#[test]
#[ignore = "needs lifecycle: the run path keeps held markers out of the units"]
fn test_a_held_marker_is_excluded_until_resumed() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.hold("network").exit, 0);
    let run = harness.reconcile();
    let units = &run.report["environments"][0]["units"];
    assert_eq!(units["network"]["result"], "held", "{:#}", run.report);
    assert_eq!(units["database"]["result"], "blocked", "{:#}", run.report);
    assert!(harness.snapshot().desired[&unit("network")].unit.uid.is_none());

    assert_eq!(harness.resume("network").exit, 0);
    let run = harness.reconcile();
    assert_eq!(run.exit, 0, "{:#}", run.report);
    assert_eq!(run.report["environments"][0]["units"]["network"]["result"], "executed");
    assert!(harness.snapshot().desired[&unit("network")].unit.uid.is_some());
}

/// Recovery, "Holds on uncertain units" (Q16): a hold on an uncertain unit
/// writes the document and uid of its last receipt, keeps the condition
/// (status: held and uncertain, exit 4), and `resume` keeps it too, so the
/// unit still needs a run under its recovery policy or `recover`.
#[test]
#[ignore = "needs lifecycle: receipts carry the desired document they were produced for"]
fn test_a_hold_on_an_uncertain_unit_keeps_the_receipts_document_and_the_condition() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    let network = unit("network");
    let before = harness.snapshot();
    let mut uncertain = before.observed[&network].clone();
    uncertain.condition = Some(Condition::Uncertain {
        since: harness.clock.now(),
        run: RunId::new(OTHER_RUN).unwrap(),
        reason: "run lost its lease".to_owned(),
    });
    harness.stage(TransitionChanges {
        observed: BTreeMap::from([(network.clone(), Some(uncertain))]),
        ..TransitionChanges::default()
    });

    let held = harness.hold("network");
    assert_eq!(held.exit, 0, "{}", held.stderr);
    let after = harness.snapshot();
    let document = &after.desired[&network];
    assert_eq!(document.unit.uid, before.desired[&network].unit.uid);
    assert_eq!(document.execution_key, before.desired[&network].execution_key);
    assert!(after.observed[&network].condition.is_some(), "the condition stays");

    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 4, "{:#}", status.report);
    let shown = &status.report["environments"][0]["units"]["network"];
    assert_eq!(shown["state"], "held");
    assert_eq!(shown["condition"], "uncertain");

    // Consumers keep resolving from the receipt.
    let run = harness.reconcile();
    assert_eq!(run.report["environments"][0]["units"]["database"]["result"], "current");

    assert_eq!(harness.resume("network").exit, 0);
    assert!(harness.snapshot().observed[&network].condition.is_some());
}

/// Deletion, holds (Q26): a deleting unit pauses with its deletion block,
/// `resume` returns it to deleting where it stopped, and resume deletes
/// nothing: the desired and observed files stay.
#[test]
fn test_resume_of_a_held_deleting_unit_returns_it_to_deleting_and_deletes_nothing() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    let kubernetes = unit("kubernetes");
    let mut deleting = harness.snapshot().desired[&kubernetes].clone();
    let deletion = Deletion {
        reason: DeletionReason::Omission,
        intent: DeletionIntent::Teardown,
        teardown: Some(TeardownProgress::Pending),
        requested: None,
    };
    deleting.lifecycle = Lifecycle::Deleting {
        deletion: deletion.clone(),
    };
    harness.stage(TransitionChanges {
        desired: BTreeMap::from([(kubernetes.clone(), Some(deleting.clone()))]),
        ..TransitionChanges::default()
    });

    let held = harness.hold("kubernetes");
    assert_eq!(held.exit, 0, "{}", held.stderr);
    let document = &harness.snapshot().desired[&kubernetes];
    assert!(
        matches!(&document.lifecycle, Lifecycle::Held { deletion: Some(kept), .. } if *kept == deletion),
        "{:?}",
        document.lifecycle
    );
    assert_eq!(document.unit.uid, deleting.unit.uid);

    let resumed = harness.resume("kubernetes");
    assert_eq!(resumed.exit, 0, "{}", resumed.stderr);
    assert_eq!(resumed.report["details"]["state"], "deleting");
    let after = harness.snapshot();
    assert_eq!(after.desired[&kubernetes].lifecycle, Lifecycle::Deleting { deletion });
    assert_eq!(after.desired[&kubernetes].unit.uid, deleting.unit.uid);
    assert!(after.observed.contains_key(&kubernetes), "resume deletes nothing");
}

/// Q28: while a run holds the lease, `nyl hold` delivers the hold to it
/// through its signal ref instead of failing with NYL-LEASE-HELD: exit 0,
/// NYL-HOLD-DELIVERED, no commit of its own.
#[test]
#[ignore = "needs publication: shell::signal::deliver"]
fn test_hold_while_a_run_holds_the_lease_is_delivered() {
    let harness = Harness::new();
    harness.init();
    let (store, location) = (harness.store.as_ref(), &harness.location);
    let lease = harness.other_lease(30);
    store.take_lease(location, &dev(), &lease, None).unwrap();
    let desired = harness.tip(DESIRED);

    let held = harness.hold("network");
    assert_eq!(held.exit, 0, "{}", held.stderr);
    assert_eq!(held.report["warnings"][0]["code"], "NYL-HOLD-DELIVERED");
    assert_eq!(held.report["details"]["deliveredTo"]["run"], OTHER_RUN);
    assert_eq!(harness.tip(DESIRED), desired, "the run writes the hold, not hold");
    let signals = store.signals(location, &dev(), &lease.run).unwrap();
    assert!(
        signals
            .iter()
            .any(|signal| signal.action == SignalAction::Hold && signal.unit == unit("network")),
        "{signals:?}"
    );
}

/// Q28: once the run that held the lease is gone (its lease expired), hold
/// applies directly under its own lease.
#[test]
fn test_hold_after_the_run_finished_applies_directly() {
    let harness = Harness::new();
    harness.init();
    let (store, location) = (harness.store.as_ref(), &harness.location);
    store
        .take_lease(location, &dev(), &harness.other_lease(30), None)
        .unwrap();
    harness.clock.advance(Duration::from_mins(60));

    let held = harness.hold("network");
    assert_eq!(held.exit, 0, "{}", held.stderr);
    assert_eq!(held.report["outcome"], "done");
    assert!(harness.message(DESIRED).contains("Nyl-Operation: hold"));
}

/// Runs and leases, item 1: `nyl hold` is not an importer, so a broken
/// lease whose run is not imported refuses it with NYL-LEASE-BROKEN, exit
/// 2, naming `nyl recover -e`.
#[test]
#[ignore = "needs lease-break: rules::lease::decide honours BrokenLease::Refuse"]
fn test_hold_refuses_a_broken_lease() {
    let harness = Harness::new();
    harness.init();
    let (store, location) = (harness.store.as_ref(), &harness.location);
    let mut lease = harness.other_lease(30);
    lease.broken = Some(nyl_core::orchestration::state::Request {
        by: "alice".to_owned(),
        at: harness.clock.now(),
        reason: None,
    });
    store.take_lease(location, &dev(), &lease, None).unwrap();
    let held = harness.hold("network");
    assert_eq!(held.exit, 2, "{}", held.stderr);
    assert_eq!(held.report["errors"][0]["code"], "NYL-LEASE-BROKEN");
    assert!(held.stderr.contains("nyl recover -e dev"), "{}", held.stderr);
}
