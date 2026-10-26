//! Run takeover, lost leases, recovery by policy, and cancellation in the
//! orchestration shell, through its API, in the scenario test world: real
//! Git source resolution and unit rendering, the built-in Command kind and
//! the fake kinds, and the in-memory or Git state store.
//!
//! Contract: "Runs and leases" items 3 to 8 and "Recovery" in
//! design/orchestration-core.md, and the walkthroughs "Effects without a
//! receipt" and "Competing runners".

mod scenario;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::driver::{DriverOperation, RecoveryPolicy};
use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::ports::{Clock, StateSide, StateStore};
use nyl_core::orchestration::state::{ApprovalIdentity, ApprovalRecord, FailureCategory};
use nyl_core::orchestration::time::Duration;
use nyl_drivers::fake::Scripted;
use nyl_orchestration::fault::FaultPoint;
use nyl_orchestration::shell::local::{LeaseScope, LocalRun};
use nyl_orchestration::shell::run::{self, Cancellation};
use nyl_orchestration::shell::state as state_ops;
use nyl_state::memory::MemoryStateStore;
use scenario::format::CommitStep;
use scenario::runner::{FaultSlot, AUTHOR};
use scenario::shell::{
    codes, command, dev, names, outcome, project, set, shared_project, uncertain_run, unit, FaultyStore, Harness,
    Interleaved, ENVIRONMENT, NYL_TOML,
};
use scenario::world::git_output;
use serde_json::json;
use tempfile::TempDir;

/// A run killed after `database`'s effect, with network and web-image
/// checkpointed, as in the walkthrough "Effects without a receipt".
fn crashed_after_database(harness: &mut Harness) -> RunId {
    harness.faults = Box::new(FaultSlot::kill_once(FaultPoint::AfterEffect, Some("database")));
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

/// A takeover whose run ref deletion fails leaves the run ref behind; the
/// next run reads it again, changes nothing, writes no commit, and does not
/// mark the recovered unit again, then deletes the run ref.
#[test]
fn test_takeover_is_idempotent_when_a_leftover_is_read_again() {
    let mut harness = Harness::new(&shared_project(), false);
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    harness.store = Box::new(FaultyStore {
        delete_run_failures: Arc::clone(&failures),
        ..FaultyStore::default()
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
    harness.store = Box::new(FaultyStore {
        before_commit: before_commit.clone(),
        ..FaultyStore::default()
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
    harness.store = Box::new(FaultyStore {
        checkpoint_failures: Arc::new(1.into()),
        ..FaultyStore::default()
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
    harness.world.commit_files(
        "main",
        "Change the web page",
        &[("services/web/index.html", "<h1>Changed</h1>\n")],
    );
    // The run dies while executing web-image, its run ref live for an hour.
    harness.faults = Box::new(FaultSlot::kill_once(FaultPoint::AfterEffect, Some("web-image")));
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
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/alpha.yaml", &command("alpha", "true", "")),
        ("nyl/beta.yaml", &command("beta", "true", "")),
    ]);
    let mut harness = Harness::new(project.path(), false);
    harness.faults = Box::new(FaultSlot::kill_once(FaultPoint::AfterEffect, Some("beta")));
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

/// A harness whose network unit recovers by `inspect`, whose first
/// execution is uncertain, and whose second would fail, so a test sees an
/// execution where it expects an inspection; `inspected` is the scripted
/// inspection.
fn inspecting(inspected: &Scripted) -> Harness {
    let harness = Harness::new(&shared_project(), false);
    harness.script.recover_by(RecoveryPolicy::Inspect);
    let failure = Scripted::Failed {
        category: FailureCategory::ToolError,
        retryable: true,
    };
    for (operation, result) in [
        (DriverOperation::Reconcile, &Scripted::Uncertain),
        (DriverOperation::Reconcile, &failure),
        (DriverOperation::Inspect, inspected),
    ] {
        harness.script.push(unit("network"), operation, result, 1);
    }
    harness.init();
    harness
}

/// An uncertain unit whose recovery policy is `inspect` is inspected, not
/// executed: applied effects record its receipt with the inspected outputs,
/// and the unit reports `recovered`.
#[test]
fn test_inspect_policy_records_the_receipt_of_applied_effects_without_executing() {
    let harness = inspecting(&Scripted::Succeeded {
        outputs: Some(json!({"vpcId": "vpc-inspected"})),
    });
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
    let harness = inspecting(&Scripted::Uncertain);
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
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/quick.yaml", &command("quick", "true", "")),
        // After quick, so quick's checkpoint never races the signal.
        (
            "nyl/slow.yaml",
            &command("slow", &slow, "  idempotent: true\n  dependsOn: [quick]\n"),
        ),
    ]);
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

    // Until a reconcile imports the run ref, status reports what it left
    // uncertain rather than the last transition's current receipts.
    let status = harness.status();
    assert_eq!(status.exit.code(), 4, "{status:#?}");
    assert_eq!(codes(&status.errors), [Code::RunUnimported], "{status:#?}");
    // slow is new, so no transition records it yet; the finding names it.
    assert!(status.errors[0].message.contains("left slow uncertain"), "{status:#?}");

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
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/manual.yaml", &command("manual", "true", "")),
    ]);
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
        by: vec![AUTHOR.to_owned()],
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
        line.contains("approval") && line.contains(AUTHOR) && line.contains("local"),
        "{line}"
    );
}
