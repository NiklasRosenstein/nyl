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
use std::sync::Mutex;

use nyl::orchestration::run::CommandOutcome;
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Clock as _, Committed, Coordination, EnvironmentSnapshot, LocalDivergence, StateResult, StateStore,
    StateTips, Transition, TransitionChanges,
};
use nyl_core::orchestration::state::{
    ApprovalIdentity, Condition, Deletion, DeletionIntent, DeletionReason, Lease, LeaseKind, Lifecycle, Operation, Run,
    Signal, SignalAction, SignalKind, StateLocation, StateV1, TeardownProgress,
};
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::fault::{FaultAction, FaultInjector, FaultPoint, NoFaults};
use nyl_orchestration::shell::lifecycle::hold;
use nyl_orchestration::shell::run::Cancellation;
use nyl_orchestration::shell::state::Requested;
use nyl_orchestration::shell::{Context, Settings};
use scenario::runner::AUTHOR;
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
        hold: false,
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

/// Sends a hold of `unit` from Bob to the run holding the lease, once, when
/// the run reaches `point` (for `at`, the point concerning that unit).
struct HoldDuringRun<'h> {
    harness: &'h shell::Harness,
    point: FaultPoint,
    at: Option<&'static str>,
    unit: &'static str,
    sent: Mutex<Option<RunId>>,
}

impl<'h> HoldDuringRun<'h> {
    fn new(harness: &'h shell::Harness, point: FaultPoint, at: Option<&'static str>, unit: &'static str) -> Self {
        Self {
            harness,
            point,
            at,
            unit,
            sent: Mutex::new(None),
        }
    }

    fn sent_to(&self) -> RunId {
        self.sent.lock().unwrap().clone().expect("the hold was sent")
    }
}

impl FaultInjector for HoldDuringRun<'_> {
    fn at(&self, point: FaultPoint, unit: Option<&UnitName>) -> FaultAction {
        let mut sent = self.sent.lock().unwrap();
        if point != self.point || sent.is_some() || self.at.is_some_and(|at| unit.map(UnitName::as_str) != Some(at)) {
            return FaultAction::Continue;
        }
        let (store, location) = (self.harness.store.as_ref(), &self.harness.location);
        let lease = store
            .coordination(location, &dev())
            .unwrap()
            .lease
            .expect("the run holds the lease");
        store
            .send_signal(
                location,
                &dev(),
                &Signal {
                    api_version: StateV1,
                    kind: SignalKind,
                    run: lease.lease.run.clone(),
                    action: SignalAction::Hold,
                    unit: unit_name(self.unit),
                    phase1_commit: None,
                    by: "Bob <bob@example.invalid>".to_owned(),
                    identity: ApprovalIdentity::Asserted,
                    reason: Some("incident 4712".to_owned()),
                    at: self.harness.clock.now(),
                    from: "bob@workstation".to_owned(),
                },
            )
            .unwrap();
        *sent = Some(lease.lease.run);
        FaultAction::Continue
    }
}

fn unit_name(name: &str) -> UnitName {
    UnitName::new(name).unwrap()
}

/// Q28: a hold sent after the run's last read of its signals, while it
/// pushes its transition commit, still takes effect: the run reads its
/// signals again once it released the lease and applies the hold as
/// `nyl hold` would, requested by the sender, then consumes the signal.
/// Catches a hold reported delivered that no run ever applies.
#[test]
fn test_a_hold_sent_while_the_run_commits_is_applied_after_it() {
    let harness = Harness::new();
    harness.init();
    let faults = HoldDuringRun::new(&harness, FaultPoint::BeforePush, None, "network");
    let run = harness.invoke_with(&faults, &["reconcile", "-e", "dev", "-o", "json"]);
    // A held unit waits, as when the run reads the hold during its work.
    assert_eq!(run.exit, 2, "{:#}", run.report);
    assert_eq!(
        run.report["environments"][0]["units"]["network"]["result"], "held",
        "{:#}",
        run.report
    );

    let network = unit("network");
    let after = harness.snapshot();
    let document = &after.desired[&network];
    assert!(
        matches!(&document.lifecycle, Lifecycle::Held { hold, .. } if hold.reason.as_deref() == Some("incident 4712")),
        "{:?}",
        document.lifecycle
    );
    assert_eq!(
        document.unit.uid, after.observed[&network].unit.uid,
        "the incarnation that ran is held"
    );
    let message = harness.message(DESIRED);
    assert!(message.contains("Nyl-Operation: hold"), "{message}");
    assert!(
        message.contains("Nyl-Requested-By: Bob <bob@example.invalid>"),
        "{message}"
    );
    let signals = harness
        .store
        .signals(&harness.location, &dev(), &faults.sent_to())
        .unwrap();
    assert!(signals.is_empty(), "the signal is consumed: {signals:?}");
}

/// Q28 with Q16: a hold signal for a unit this run created and already
/// executed holds the incarnation that ran: its uid stays with its new
/// receipt, so `resume` does not create it again.
#[test]
fn test_a_hold_signal_for_a_unit_the_run_executed_keeps_its_incarnation() {
    let harness = Harness::new();
    harness.init();
    let faults = HoldDuringRun::new(&harness, FaultPoint::Executing, Some("database"), "network");
    let run = harness.invoke_with(&faults, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(run.exit, 2, "{:#}", run.report);
    assert_eq!(
        run.report["environments"][0]["units"]["network"]["result"], "held",
        "{:#}",
        run.report
    );

    let network = unit("network");
    let after = harness.snapshot();
    let document = &after.desired[&network];
    assert!(
        matches!(document.lifecycle, Lifecycle::Held { .. }),
        "{:?}",
        document.lifecycle
    );
    assert!(!document.is_held_marker(), "not a marker: the unit ran");
    assert!(document.unit.uid.is_some());
    assert_eq!(document.unit.uid, after.observed[&network].unit.uid);
    assert!(
        matches!(&document.lifecycle, Lifecycle::Held { hold, .. } if hold.pending.is_none()),
        "the run executed the current document: nothing pending"
    );

    assert_eq!(harness.resume("network").exit, 0);
    let run = harness.reconcile();
    assert_eq!(
        run.report["environments"][0]["units"]["network"]["result"], "current",
        "{:#}",
        run.report
    );
}

/// Q28: only a reconcile or teardown run reads hold signals. While another
/// operation holds the lease (here `verify`), `nyl hold` meets a held
/// lease (NYL-LEASE-HELD, exit 2) and sends no signal nothing would read.
#[test]
fn test_hold_while_a_verify_holds_the_lease_refuses_as_a_held_lease() {
    let harness = Harness::new();
    harness.init();
    let (store, location) = (harness.store.as_ref(), &harness.location);
    let mut lease = harness.other_lease(30);
    lease.operation = Operation::Verify;
    store.take_lease(location, &dev(), &lease, None).unwrap();

    let held = harness.hold("network");
    assert_eq!(held.exit, 2, "{}", held.stderr);
    assert_eq!(held.report["errors"][0]["code"], "NYL-LEASE-HELD");
    assert!(store.signals(location, &dev(), &lease.run).unwrap().is_empty());
}

/// The state store of `inner`, where the run holding the lease finishes
/// (its lease goes) right after a signal is sent to it.
struct FinishesOnSignal<'s> {
    inner: &'s dyn StateStore,
}

impl StateStore for FinishesOnSignal<'_> {
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
        self.inner.write_run(location, environment, run, checkpoint)
    }
    fn delete_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
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
        self.inner.send_signal(location, environment, signal)?;
        // The run finishes before it reads the signal.
        self.inner.delete_lease(location, environment)
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
        self.inner
            .commit(location, environment, base, transition, lease_version)
    }
    fn import_history(&self, from: &StateLocation, to: &StateLocation) -> StateResult<()> {
        self.inner.import_history(from, to)
    }
    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        self.inner.local_divergence(location)
    }
}

/// Q28: when the run holding the lease finishes after the signal is pushed
/// but before it read it, `nyl hold` deletes the signal and applies the
/// hold directly under its own lease, instead of reporting it delivered.
#[test]
fn test_hold_applies_directly_when_the_run_finishes_before_reading_the_signal() {
    let harness = Harness::new();
    harness.init();
    let lease = harness.other_lease(30);
    harness
        .store
        .take_lease(&harness.location, &dev(), &lease, None)
        .unwrap();
    let store = FinishesOnSignal {
        inner: harness.store.as_ref(),
    };
    let context = Context {
        store: &store,
        clock: harness.clock.as_ref(),
        ids: &harness.ids,
        faults: &NoFaults,
        runner: "bob@workstation".to_owned(),
        requested_by: AUTHOR.to_owned(),
        version: "test".to_owned(),
        settings: Settings::default(),
    };
    let report = hold(
        &context,
        &dev(),
        &harness.location,
        &unit("network"),
        &Requested {
            reason: Some("incident 4711".to_owned()),
            wait: None,
            cancellation: Cancellation::default(),
        },
    );
    assert_eq!(report.exit, 0, "{report:?}");
    assert!(report.warnings.is_empty(), "not delivered: {report:?}");
    assert!(harness.message(DESIRED).contains("Nyl-Operation: hold"));
    assert!(matches!(
        harness.snapshot().desired[&unit("network")].lifecycle,
        Lifecycle::Held { .. }
    ));
    assert!(harness
        .store
        .signals(&harness.location, &dev(), &lease.run)
        .unwrap()
        .is_empty());
}

/// A hold delivered during an earlier teardown must protect the next batch.
#[test]
fn test_a_hold_between_teardown_batches_keeps_the_resource() {
    let units: String = ["first", "second"].iter().map(|name| format!(
        "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: {name}\n  labels: {{dev: 'true'}}\nspec:\n  command: [sh, -c, 'touch %SCENARIO_TMP%/{name}']\n  teardown: [sh, -c, 'rm %SCENARIO_TMP%/{name}']\n---\n"
    )).collect();
    let project = shell::project(&[
        shell::NYL_TOML,
        ("nyl/environment.yaml", shell::ENVIRONMENT),
        ("nyl/units.yaml", &units),
    ]);
    let harness = Harness {
        shell: shell::Harness::new(project.path(), true),
    };
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    harness
        .world
        .commit_files("main", "Remove both units", &[("nyl/units.yaml", "")]);
    let faults = HoldDuringRun::new(&harness, FaultPoint::AfterEffect, Some("first"), "second");
    let report = harness.invoke_with(
        &faults,
        &[
            "reconcile",
            "-e",
            "dev",
            "--allow-teardown",
            "--concurrency",
            "1",
            "-o",
            "json",
        ],
    );
    assert_eq!(
        report.report["environments"][0]["units"]["second"]["result"], "held",
        "{:#}",
        report.report
    );
    assert!(!harness.world.scenario_tmp().join("first").exists());
    assert!(harness.world.scenario_tmp().join("second").exists());
    let snapshot = harness.snapshot();
    let second = &snapshot.desired[&unit("second")];
    assert!(matches!(second.lifecycle, Lifecycle::Held { deletion: Some(_), .. }));
    assert!(second.unit.uid.is_some());
}

/// A publication held after phase 1 keeps its incarnation and wait, so
/// phase 2 can only run after resume.
#[test]
fn test_a_hold_after_publication_phase_one_prevents_phase_two() {
    let harness = Harness::new();
    harness.init();
    assert_eq!(harness.reconcile().exit, 0);
    let mut publication = harness.snapshot().desired[&unit("kubernetes")].clone();
    publication.spec["teardownWait"] = serde_json::json!({"strategy": "delay", "duration": "15s"});
    publication.resolved_spec.as_mut().unwrap()["teardownWait"] = publication.spec["teardownWait"].clone();
    harness.stage(TransitionChanges {
        desired: BTreeMap::from([(unit("kubernetes"), Some(publication))]),
        ..TransitionChanges::default()
    });
    let faults = HoldDuringRun::new(&harness, FaultPoint::AfterEffect, Some("kubernetes"), "kubernetes");
    let report = harness.invoke_with(
        &faults,
        &["teardown", "-e", "dev", "--unit", "kubernetes", "-o", "json"],
    );
    assert_eq!(
        report.report["environments"][0]["units"]["kubernetes"]["result"], "held",
        "{:#}",
        report.report
    );
    let snapshot = harness.snapshot();
    let publication = &snapshot.desired[&unit("kubernetes")];
    assert!(matches!(
        publication.lifecycle,
        Lifecycle::Held { deletion: Some(_), .. }
    ));
    assert!(publication.unit.uid.is_some());
    assert!(snapshot.observed[&unit("kubernetes")].waiting.is_some());
}
