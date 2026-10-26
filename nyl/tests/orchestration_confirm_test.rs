//! Signals to a running run: `nyl teardown --confirm-removed` and `nyl hold`
//! delivered through the run's signal ref, or acted on directly.
//!
//! Contract: [Kubernetes publication unit](../../design/orchestration-core.md#kubernetes-publication-unit),
//! the signal path, and [Deletion](../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit" (a hold delivered to a running run).

mod scenario;

use std::collections::BTreeMap;

use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, ExecutionKey, RunId, Uid, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Committed, Coordination, EnvironmentSnapshot, StateResult, StateStore, StateTips, Transition,
    TransitionChanges,
};
use nyl_core::orchestration::state::{
    ApprovalIdentity, DesiredUnit, DesiredUnitKind, Lease, LeaseKind, Lifecycle, ObservedUnit, ObservedUnitKind,
    Operation, Readiness, Run, RunKind, Signal, SignalAction, StateLocation, StateV1, TeardownWaiting, UnitIdentity,
};
use nyl_core::orchestration::time::Timestamp;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::run_report::ExitCategory;
use nyl_orchestration::shell::run::Cancellation;
use nyl_orchestration::shell::signal::{deliver, Delivery};
use nyl_orchestration::shell::{Context, Settings};
use nyl_orchestration::testing::{FakeClock, SequentialIds};
use nyl_state::memory::MemoryStateStore;
use scenario::runner::{invoke_in, AUTHOR};
use scenario::shell::{self as harness, shared_project};
use scenario::world::git_output;

const RUN: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
const PHASE1: &str = "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a";

fn time(minutes: i64) -> Timestamp {
    chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
        .unwrap()
        .to_utc()
        + chrono::Duration::minutes(minutes)
}

fn dev() -> EnvironmentName {
    EnvironmentName::new("dev").unwrap()
}

fn name(unit: &str) -> UnitName {
    UnitName::new(unit).unwrap()
}

fn run_id() -> RunId {
    RunId::new(RUN).unwrap()
}

fn location() -> StateLocation {
    StateLocation {
        repository: "file:///state.git".to_owned(),
        desired_ref: "nyl/dev/desired".to_owned(),
        observed_ref: "nyl/dev/observed".to_owned(),
        path: String::new(),
        coordination_ref_prefix: "refs/nyl/".to_owned(),
    }
}

/// The invocation sending the signal, at 10:05.
struct Sender {
    clock: FakeClock,
    ids: SequentialIds,
}

impl Sender {
    fn new() -> Self {
        Self {
            clock: FakeClock::new(time(5)),
            ids: SequentialIds::new(),
        }
    }

    fn context<'a>(&'a self, store: &'a dyn StateStore) -> Context<'a> {
        Context {
            store,
            clock: &self.clock,
            ids: &self.ids,
            faults: &NoFaults,
            runner: "alice@workstation".to_owned(),
            requested_by: "Alice <alice@example.invalid>".to_owned(),
            version: "0.0.0-test".to_owned(),
            settings: Settings::default(),
        }
    }

    fn deliver(&self, store: &dyn StateStore, unit: &str, action: SignalAction) -> Result<Delivery, Finding> {
        deliver(
            &self.context(store),
            &location(),
            &dev(),
            &name(unit),
            action,
            Some("checked the cluster by hand"),
        )
    }
}

fn desired(unit: &str) -> DesiredUnit {
    DesiredUnit {
        api_version: StateV1,
        kind: DesiredUnitKind,
        unit: UnitIdentity {
            environment: dev(),
            name: name(unit),
            uid: Some(Uid::new("0192f0c4-8d0a-7c3e-9a51-000000000001").unwrap()),
            api_version: "units.gitops.nyl/v1".to_owned(),
            kind: "KubernetesPublication".to_owned(),
        },
        lifecycle: Lifecycle::Active,
        source_commit: Some(CommitId::new(PHASE1).unwrap()),
        source_revision: Some(CommitId::new(PHASE1).unwrap()),
        spec: serde_json::json!({"target": "dev"}),
        resolved_spec: Some(serde_json::json!({"target": "dev"})),
        provenance: BTreeMap::new(),
        execution_key: Some(ExecutionKey::from_digest_of(b"key")),
        fingerprint: None,
        attestations: Vec::new(),
        readiness: Readiness::Ready,
    }
}

/// The observed file of `unit`, waiting after phase 1 when `waiting`.
fn observed(unit: &str, waiting: bool) -> ObservedUnit {
    ObservedUnit {
        api_version: StateV1,
        kind: ObservedUnitKind,
        unit: desired(unit).unit,
        receipt: None,
        condition: None,
        desired: None,
        waiting: waiting.then(|| TeardownWaiting {
            phase: "applications-removed".to_owned(),
            reason: "phase 1 published".to_owned(),
            since: time(0),
            until: None,
            phase1_commit: Some(CommitId::new(PHASE1).unwrap()),
            run: run_id(),
        }),
    }
}

/// A live run of `operation` by `ci-runner` holding the lease until 11:00
/// on `units`, with a checkpoint for each of `checkpoints` (unit, waiting).
fn running(store: &dyn StateStore, operation: Operation, units: &[&str], checkpoints: &[(&str, bool)]) {
    let units = units.iter().map(|unit| name(unit)).collect::<Vec<_>>();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: run_id(),
        runner: "ci-runner".to_owned(),
        operation,
        units: units.clone(),
        deadline: time(60),
        taken_at: time(0),
        broken: None,
    };
    store.take_lease(&location(), &dev(), &lease, None).unwrap();
    let run = run_record(operation, units, false);
    store.write_run(&location(), &dev(), &run, None).unwrap();
    for (unit, waiting) in checkpoints {
        let checkpoint = Checkpoint {
            desired: desired(unit),
            observed: observed(unit, *waiting),
            artifacts: BTreeMap::new(),
        };
        store.write_run(&location(), &dev(), &run, Some(&checkpoint)).unwrap();
    }
}

fn run_record(operation: Operation, units: Vec<UnitName>, finished: bool) -> Run {
    Run {
        api_version: StateV1,
        kind: RunKind,
        run: run_id(),
        runner: "ci-runner".to_owned(),
        operation,
        source_commit: None,
        read_desired: None,
        read_observed: None,
        deadline: time(60),
        units,
        finished,
    }
}

/// Store `kubernetes`'s observed file with a teardown wait, as a run that
/// stopped waiting left it, and release that run's lease.
fn stored_wait(store: &dyn StateStore) {
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: run_id(),
        runner: "ci-runner".to_owned(),
        operation: Operation::Teardown,
        units: Vec::new(),
        deadline: time(60),
        taken_at: time(0),
        broken: None,
    };
    let version = store.take_lease(&location(), &dev(), &lease, None).unwrap();
    let transition = Transition {
        message: "nyl: teardown dev".to_owned(),
        changes: TransitionChanges {
            desired: BTreeMap::from([(name("kubernetes"), Some(desired("kubernetes")))]),
            observed: BTreeMap::from([(name("kubernetes"), Some(observed("kubernetes", true)))]),
            ..TransitionChanges::default()
        },
    };
    let committed = store
        .commit(&location(), &dev(), &StateTips::default(), &transition, &version)
        .unwrap();
    store
        .release_lease(&location(), &dev(), &committed.lease_version)
        .unwrap();
}

fn signals(store: &dyn StateStore) -> Vec<Signal> {
    store.signals(&location(), &dev(), &run_id()).unwrap()
}

fn exit(finding: &Finding) -> u8 {
    ExitCategory::of_finding(finding).code()
}

/// Catches `--confirm-removed` without a running run failing, or sending a
/// signal nobody reads, when the stored wait is what it confirms.
#[test]
fn test_confirm_without_a_lease_confirms_a_stored_wait_directly() {
    let store = MemoryStateStore::new();
    stored_wait(&store);
    let delivery = Sender::new().deliver(&store, "kubernetes", SignalAction::ConfirmRemoved);
    assert_eq!(delivery, Ok(Delivery::Direct));
    assert!(signals(&store).is_empty());
}

/// Catches a confirmation for a unit whose teardown does not wait being
/// accepted, where it would claim a removal nothing waits for.
#[test]
fn test_confirm_of_a_unit_nothing_waits_on_is_rejected() {
    let store = MemoryStateStore::new();
    stored_wait(&store);
    let finding = Sender::new()
        .deliver(&store, "database", SignalAction::ConfirmRemoved)
        .unwrap_err();
    assert_eq!(finding.code, Code::ConfirmationRejected);
    assert_eq!(exit(&finding), 1);

    // Nor is anything confirmed in an environment without state.
    let store = MemoryStateStore::new();
    let finding = Sender::new()
        .deliver(&store, "kubernetes", SignalAction::ConfirmRemoved)
        .unwrap_err();
    assert_eq!(finding.code, Code::ConfirmationRejected);
}

/// Catches a confirmation for a run waiting on the unit not reaching it,
/// or reaching it unbound to the phase 1 commit it confirms.
#[test]
fn test_confirm_to_a_waiting_run_is_delivered_bound_to_its_phase1_commit() {
    let store = MemoryStateStore::new();
    running(&store, Operation::Teardown, &["kubernetes"], &[("kubernetes", true)]);
    let delivery = Sender::new().deliver(&store, "kubernetes", SignalAction::ConfirmRemoved);
    assert_eq!(
        delivery,
        Ok(Delivery::Delivered {
            run: run_id(),
            runner: "ci-runner".to_owned()
        })
    );
    assert_eq!(
        signals(&store),
        vec![Signal {
            api_version: StateV1,
            kind: nyl_core::orchestration::state::SignalKind,
            run: run_id(),
            action: SignalAction::ConfirmRemoved,
            unit: name("kubernetes"),
            phase1_commit: Some(CommitId::new(PHASE1).unwrap()),
            by: "Alice <alice@example.invalid>".to_owned(),
            identity: ApprovalIdentity::Asserted,
            reason: Some("checked the cluster by hand".to_owned()),
            at: time(5),
            from: "alice@workstation".to_owned(),
        }]
    );
}

/// Catches a confirmation sent to a run that is busy with something else,
/// which would never read it; the lease is held as for any operation.
#[test]
fn test_confirm_while_the_holder_does_something_else_exits_2() {
    let store = MemoryStateStore::new();
    running(&store, Operation::Reconcile, &["network"], &[("network", false)]);
    let finding = Sender::new()
        .deliver(&store, "kubernetes", SignalAction::ConfirmRemoved)
        .unwrap_err();
    assert_eq!(finding.code, Code::LeaseHeld);
    assert_eq!(exit(&finding), 2);
    assert!(finding.message.contains("ci-runner"), "{}", finding.message);
    assert!(signals(&store).is_empty());

    // A run working on the unit without waiting on its teardown is busy too.
    let store = MemoryStateStore::new();
    running(&store, Operation::Teardown, &["kubernetes"], &[("kubernetes", false)]);
    let finding = Sender::new()
        .deliver(&store, "kubernetes", SignalAction::ConfirmRemoved)
        .unwrap_err();
    assert_eq!(finding.code, Code::LeaseHeld);
}

/// Catches a hold failing with `NYL-LEASE-HELD` while a run holds the
/// lease, instead of reaching that run (Q28).
#[test]
fn test_hold_is_delivered_to_any_live_run() {
    let store = MemoryStateStore::new();
    running(&store, Operation::Reconcile, &["network"], &[]);
    let delivery = Sender::new().deliver(&store, "database", SignalAction::Hold);
    assert_eq!(
        delivery,
        Ok(Delivery::Delivered {
            run: run_id(),
            runner: "ci-runner".to_owned()
        })
    );
    let [signal] = signals(&store).try_into().unwrap();
    assert_eq!(signal.action, SignalAction::Hold);
    assert_eq!(signal.unit, name("database"));
    assert_eq!(signal.phase1_commit, None);
    assert_eq!(signal.by, "Alice <alice@example.invalid>");

    // Without a running run, the hold applies directly.
    let store = MemoryStateStore::new();
    assert_eq!(
        Sender::new().deliver(&store, "database", SignalAction::Hold),
        Ok(Delivery::Direct)
    );
}

/// A memory store whose running run finishes as soon as a signal is pushed
/// to it, before reading the signal.
struct FinishingStore {
    inner: MemoryStateStore,
}

impl StateStore for FinishingStore {
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
        // The run ends: it marks its record finished and releases the lease.
        let coordination = self.inner.coordination(location, environment)?;
        let state = coordination
            .runs
            .iter()
            .find(|state| state.run.run == signal.run)
            .expect("the signalled run has a run ref");
        let finished = Run {
            finished: true,
            ..state.run.clone()
        };
        self.inner.write_run(location, environment, &finished, None)?;
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
}

/// Catches a signal left on the ref of a run that finished before reading
/// it, while the invocation reports a delivery that never took effect.
#[test]
fn test_signals_to_a_run_that_finished_meanwhile_are_deleted_and_act_directly() {
    for (action, unit) in [
        (SignalAction::ConfirmRemoved, "kubernetes"),
        (SignalAction::Hold, "database"),
    ] {
        let store = FinishingStore {
            inner: MemoryStateStore::new(),
        };
        running(&store, Operation::Teardown, &["kubernetes"], &[("kubernetes", true)]);
        let delivery = Sender::new().deliver(&store, unit, action);
        assert_eq!(delivery, Ok(Delivery::Direct), "{action:?}");
        assert!(signals(&store).is_empty(), "{action:?}: the signal is deleted");
        assert!(store.coordination(&location(), &dev()).unwrap().lease.is_none());
    }
}

/// `nyl <args>` in-process in a fresh clone of the shared project's `main`.
fn invoke(harness: &harness::Harness, args: &[&str]) -> CommandOutcome {
    let tools = ProcessRunner::new();
    let wiring = Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools: &tools,
        faults: &NoFaults,
        runner: "test-runner".to_owned(),
        author: Some(AUTHOR.parse().unwrap()),
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        observer: None,
    };
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    invoke_in(&harness.world, &wiring, &args, "main", "").unwrap()
}

fn result<'a>(outcome: &'a CommandOutcome, unit: &str) -> &'a serde_json::Value {
    &outcome.report["environments"][0]["units"][unit]["result"]
}

/// End to end through the run path: a publication with the default
/// `manual` teardown wait publishes phase 1 and exits 2 with its
/// dependencies waiting; `--confirm-removed` runs phase 2 directly, and the
/// next teardown of everything proceeds to the dependencies. Catches the
/// confirmation not reaching the stored wait, or the wait not gating the
/// units the publication depends on.
#[test]
#[ignore = "needs lifecycle and operator: the teardown run mode and `nyl teardown`"]
fn test_manual_wait_exits_2_until_confirm_removed_runs_phase_2() {
    let harness = harness::Harness::new(&shared_project(), true);
    let init = invoke(&harness, &["state", "init", "-e", "dev"]);
    assert_eq!(init.exit, 0, "{}", init.stderr);
    let reconcile = invoke(&harness, &["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(reconcile.exit, 0, "{}", reconcile.stderr);

    let waiting = invoke(&harness, &["teardown", "-e", "dev", "--all", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(result(&waiting, "kubernetes"), "tearing-down");
    assert_ne!(result(&waiting, "database"), "removed");
    let files = git_output(
        &harness.world.bare(),
        &["ls-tree", "-r", "--name-only", "refs/heads/deploy"],
    );
    assert!(files.lines().any(|path| path.starts_with("dev/web/")), "{files}");

    let confirmed = invoke(
        &harness,
        &[
            "teardown",
            "-e",
            "dev",
            "--unit",
            "kubernetes",
            "--confirm-removed",
            "--reason",
            "checked the cluster by hand",
            "-o",
            "json",
        ],
    );
    assert_eq!(confirmed.exit, 0, "{}", confirmed.stderr);
    assert_eq!(result(&confirmed, "kubernetes"), "removed");
    let files = git_output(
        &harness.world.bare(),
        &["ls-tree", "-r", "--name-only", "refs/heads/deploy"],
    );
    assert!(!files.lines().any(|path| path.starts_with("dev/")), "{files}");
    let message = git_output(
        &harness.world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/observed"],
    );
    assert!(message.contains("confirmedRemoval"), "{message}");

    let rest = invoke(&harness, &["teardown", "-e", "dev", "--all", "-o", "json"]);
    assert_eq!(rest.exit, 0, "{}", rest.stderr);
    assert_eq!(result(&rest, "database"), "removed");
}
