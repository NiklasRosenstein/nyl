//! Deletion, teardown, holds, and `--fail-fast` on the shared run path,
//! through the orchestration shell's API in the scenario test world: real
//! Git source resolution and unit rendering, Command units running real
//! scripts, and the in-memory state store.
//!
//! Contract: "Deletion", "Identity", "Execution", and "Instances" in
//! design/orchestration-core.md, as amended by the part 5 decisions
//! (Q3, Q4, Q6, Q7, Q8, Q11, Q16, Q19, Q26, Q28, `--fail-fast`).

mod scenario;

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::ids::{CommitId, RunId};
use nyl_core::orchestration::ports::{Clock, EnvironmentSnapshot, Transition, TransitionChanges};
use nyl_core::orchestration::state::{
    ApprovalIdentity, Condition, DeletionReason, DesiredUnit, Hold, Lease, LeaseKind, Lifecycle, Observation,
    ObservationType, Operation, Signal, SignalAction, SignalKind, StateV1, TeardownProgress,
};
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::fault::{FaultPoint, NoFaults};
use nyl_orchestration::rules::lifecycle::{transition, Event, Next};
use nyl_orchestration::rules::recovery::RecoverAction;
use nyl_orchestration::run_report::{EnvironmentRunReport, UnitResult};
use nyl_orchestration::shell::recover;
use nyl_orchestration::shell::run::{Cancellation, RunMode, TeardownTarget};
use nyl_orchestration::shell::state::Requested;
use scenario::format::CommitStep;
use scenario::runner::FaultSlot;
use scenario::shell::{codes, dev, outcome, project, set, uncertain_run, unit, Harness, Job, ENVIRONMENT, NYL_TOML};
use tempfile::TempDir;

/// A Command unit of `dev`: `script` creates, `teardown` (when given) removes.
fn command_unit(name: &str, script: &str, teardown: Option<&str>, extra: &str) -> String {
    let teardown = teardown.map_or_else(String::new, |teardown| format!("  teardown: [sh, -c, '{teardown}']\n"));
    format!(
        "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata:\n  name: {name}\n  labels: {{dev: 'true'}}\nspec:\n  command: [sh, -c, '{script}']\n{teardown}{extra}"
    )
}

/// The walkthrough "Deletion": `cache` creates a directory standing in for
/// a cache and reports it; `worker` consumes it and declares `Retain`.
fn cache_and_worker() -> String {
    format!(
        "{}---\n{}",
        command_unit(
            "cache",
            "mkdir -p %SCENARIO_TMP%/cache && printf ''{\"endpoint\": \"%SCENARIO_TMP%/cache\"}'' > \"$NYL_OUTPUTS\"",
            Some("rm -rf %SCENARIO_TMP%/cache"),
            "  idempotent: true\n  outputs:\n    endpoint: {type: string}\n",
        ),
        command_unit(
            "worker",
            "test -d %SCENARIO_TMP%/cache",
            None,
            "  values:\n    cache: {fromUnit: {unit: cache, output: endpoint}}\n  idempotent: true\n  deletionPolicy: Retain\n",
        )
    )
}

fn harness_with(units: &str) -> (TempDir, Harness) {
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/units.yaml", units),
    ]);
    let harness = Harness::new(project.path(), false);
    harness.init();
    (project, harness)
}

fn remove_units(harness: &Harness) {
    let steps = TempDir::new().unwrap();
    harness.world.commit(
        &CommitStep {
            branch: "main".to_owned(),
            message: "Remove the units".to_owned(),
            apply: None,
            delete: vec!["nyl/units.yaml".to_owned()],
        },
        steps.path(),
    );
}

fn set_units(harness: &Harness, units: &str) {
    harness
        .world
        .commit_files("main", "Change the units", &[("nyl/units.yaml", units)]);
}

/// Run `job` after `adjust` changed its request.
fn run_job(harness: &Harness, adjust: impl FnOnce(&mut Job)) -> EnvironmentRunReport {
    let mut job = harness.job(&[], Cancellation::default());
    adjust(&mut job);
    harness.run(&job).environments.remove(0)
}

fn reconcile_allowing(harness: &Harness, allow_dependents: bool) -> EnvironmentRunReport {
    run_job(harness, |job| {
        job.request.lifecycle.allow_teardown = true;
        job.request.lifecycle.allow_dependents = allow_dependents;
    })
}

fn teardown(harness: &Harness, target: TeardownTarget, adjust: impl FnOnce(&mut Job)) -> EnvironmentRunReport {
    run_job(harness, |job| {
        job.request.mode = RunMode::Teardown { target };
        job.request.reason = Some("test".to_owned());
        adjust(job);
    })
}

fn units_of(names: &[&str]) -> BTreeSet<nyl_core::orchestration::ids::UnitName> {
    names.iter().map(|name| unit(name)).collect()
}

/// Write `changes` as a transition commit under a lease of the test's own,
/// as `nyl hold`, `nyl resume`, or an operator editing state would.
fn put(harness: &Harness, changes: TransitionChanges) {
    let store = harness.store.as_ref();
    let snapshot = harness.snapshot();
    let now = harness.clock.now();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("00000000-0000-7000-9000-0000000000fe").unwrap(),
        runner: "test".to_owned(),
        operation: Operation::Hold,
        units: Vec::new(),
        deadline: Duration::from_mins(30).after(now),
        taken_at: now,
        broken: None,
    };
    let version = store.take_lease(&harness.location, &dev(), &lease, None).unwrap();
    store
        .commit(
            &harness.location,
            &dev(),
            &snapshot.tips,
            &Transition {
                message: "nyl: hold dev\n\nNyl-Operation: hold\n".to_owned(),
                changes,
            },
            &version,
        )
        .unwrap();
    store.delete_lease(&harness.location, &dev()).unwrap();
}

/// Apply a lifecycle `event` to a stored unit and write it, as `nyl hold`
/// and `nyl resume` do through the same table.
fn put_lifecycle(harness: &Harness, name: &str, event: Event) {
    let stored = harness.snapshot().desired[&unit(name)].clone();
    let lifecycle = match transition(&stored.lifecycle, event).unwrap() {
        Next::Lifecycle(lifecycle) => lifecycle,
        other => panic!("{other:?}"),
    };
    let document = DesiredUnit { lifecycle, ..stored };
    put(
        harness,
        TransitionChanges {
            desired: BTreeMap::from([(unit(name), Some(document))]),
            ..TransitionChanges::default()
        },
    );
}

fn hold() -> Hold {
    Hold {
        since: "2026-09-24T12:00:00Z".parse().unwrap(),
        by: "alice".to_owned(),
        reason: Some("incident".to_owned()),
        pending: None,
    }
}

fn lifecycle_of(snapshot: &EnvironmentSnapshot, name: &str) -> Lifecycle {
    snapshot.desired[&unit(name)].lifecycle.clone()
}

/// The walkthrough "Deletion" through the shell, with the decided last step
/// (Q3): an omission destroys nothing without `--allow-teardown` and is
/// undone by restoring source; a retained tombstone keeps its receipt and
/// blocks its producer's teardown until `--allow-dependents`; after the
/// producer is recreated with a new uid and the same endpoint, the
/// returning tombstone is current at once.
#[test]
fn test_deletion_walkthrough_keeps_a_returning_tombstone_current() {
    let (_project, harness) = harness_with(&cache_and_worker());
    let first = harness.reconcile(&[]);
    assert_eq!(outcome(&first, "executed"), set(&["cache", "worker"]), "{first:#?}");
    let created = harness.snapshot();
    // Q16: every receipt carries the document it was produced for.
    assert_eq!(
        created.observed[&unit("cache")].desired.as_deref(),
        Some(&created.desired[&unit("cache")])
    );
    let cache_uid = created.desired[&unit("cache")].unit.uid.clone();
    let worker_uid = created.desired[&unit("worker")].unit.uid.clone();

    // Removing both destroys nothing without --allow-teardown.
    remove_units(&harness);
    let omitted = harness.reconcile(&[]);
    assert_eq!(omitted.exit.code(), 2, "{omitted:#?}");
    assert_eq!(outcome(&omitted, "pending-teardown"), set(&["cache"]));
    assert_eq!(outcome(&omitted, "retained"), set(&["worker"]));
    assert!(codes(&omitted.units[&unit("cache")].warnings).contains(&Code::TeardownPending));
    let snapshot = harness.snapshot();
    let Lifecycle::Deleting { deletion } = lifecycle_of(&snapshot, "cache") else {
        panic!("cache is deleting: {:?}", lifecycle_of(&snapshot, "cache"));
    };
    assert_eq!(deletion.reason, DeletionReason::Omission);
    assert_eq!(deletion.teardown, Some(TeardownProgress::Pending));
    assert_eq!(snapshot.desired[&unit("cache")].unit.uid, cache_uid);
    assert!(snapshot.observed[&unit("cache")].receipt.is_some(), "the receipt stays");
    let Lifecycle::Retained { tombstone } = lifecycle_of(&snapshot, "worker") else {
        panic!("worker is a tombstone");
    };
    assert!(tombstone.receipt.is_some());
    assert!(
        !snapshot.observed.contains_key(&unit("worker")),
        "the tombstone embeds the receipt"
    );
    assert!(harness.world.scenario_tmp().join("cache").is_dir());

    // Restoring both before any teardown returns them unchanged.
    set_units(&harness, &cache_and_worker());
    let restored = harness.reconcile(&[]);
    assert_eq!(restored.exit.code(), 0, "{restored:#?}");
    assert_eq!(outcome(&restored, "current"), set(&["cache", "worker"]));
    let snapshot = harness.snapshot();
    assert_eq!(snapshot.desired[&unit("cache")].unit.uid, cache_uid);
    assert_eq!(snapshot.desired[&unit("worker")].unit.uid, worker_uid);
    assert_eq!(lifecycle_of(&snapshot, "worker"), Lifecycle::Active);

    // Tearing cache down needs both overrides: worker's tombstone cites it.
    remove_units(&harness);
    let refused = reconcile_allowing(&harness, false);
    assert_eq!(refused.exit.code(), 2, "{refused:#?}");
    assert_eq!(outcome(&refused, "pending-teardown"), set(&["cache"]));
    assert!(codes(&refused.units[&unit("cache")].warnings).contains(&Code::TeardownHasDependents));
    let removed = reconcile_allowing(&harness, true);
    assert_eq!(removed.exit.code(), 0, "{removed:#?}");
    assert_eq!(outcome(&removed, "removed"), set(&["cache"]));
    let snapshot = harness.snapshot();
    assert!(!snapshot.desired.contains_key(&unit("cache")) && !snapshot.observed.contains_key(&unit("cache")));
    assert!(
        !harness.world.scenario_tmp().join("cache").exists(),
        "the teardown step ran"
    );

    // Restoring both starts a new cache with the same endpoint; worker's key
    // is unchanged, so it adopts its tombstone and stays current (Q3).
    set_units(&harness, &cache_and_worker());
    let returned = harness.reconcile(&[]);
    assert_eq!(returned.exit.code(), 0, "{returned:#?}");
    assert_eq!(outcome(&returned, "executed"), set(&["cache"]));
    assert_eq!(outcome(&returned, "current"), set(&["worker"]));
    let snapshot = harness.snapshot();
    assert_ne!(snapshot.desired[&unit("cache")].unit.uid, cache_uid);
    assert_eq!(snapshot.desired[&unit("worker")].unit.uid, worker_uid);
    let worker = &snapshot.observed[&unit("worker")];
    assert_eq!(
        worker
            .desired
            .as_deref()
            .and_then(|desired| desired.execution_key.clone()),
        worker.receipt.as_ref().map(|receipt| receipt.execution_key.clone()),
        "the restored receipt carries the document it was produced for"
    );
}

/// Q8: a Command teardown that fails leaves the teardown in progress with a
/// failed condition (exit 3); a non-retryable failure waits for `nyl
/// recover --retry`, after which a reconcile runs the teardown again.
#[test]
fn test_a_failed_teardown_waits_for_recover_and_then_runs_again() {
    let (_project, harness) = harness_with(&command_unit("svc", "true", Some("test ! -e %SCENARIO_TMP%/fail"), ""));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    std::fs::write(harness.world.scenario_tmp().join("fail"), "").unwrap();
    remove_units(&harness);
    let failed = reconcile_allowing(&harness, false);
    assert_eq!(failed.exit.code(), 3, "{failed:#?}");
    assert!(matches!(
        failed.units[&unit("svc")].result,
        Some(UnitResult::Failed { retryable: false, .. })
    ));
    let snapshot = harness.snapshot();
    let Lifecycle::Deleting { deletion } = lifecycle_of(&snapshot, "svc") else {
        panic!("svc is still deleting");
    };
    assert_eq!(deletion.teardown, Some(TeardownProgress::InProgress));
    assert!(matches!(
        snapshot.observed[&unit("svc")].condition,
        Some(Condition::Failed { .. })
    ));

    // Without recovery the next run does not try again.
    std::fs::remove_file(harness.world.scenario_tmp().join("fail")).unwrap();
    let waiting = harness.reconcile(&[]);
    assert_eq!(waiting.exit.code(), 3, "{waiting:#?}");
    let recovered = recover::recover(
        &harness.context(),
        &dev(),
        &harness.location,
        &unit("svc"),
        &RecoverAction::Retry,
        &Requested::default(),
    );
    assert_eq!(recovered.exit, 0, "{recovered:#?}");
    // A started teardown needs no --allow-teardown to continue.
    let removed = harness.reconcile(&[]);
    assert_eq!(removed.exit.code(), 0, "{removed:#?}");
    assert_eq!(outcome(&removed, "removed"), set(&["svc"]));
}

/// Recovery: an uncertain teardown of an idempotent unit runs again by
/// itself; a teardown that passes the unit's timeout is uncertain. A
/// teardown killed by a signal is uncertain too, where processes die of
/// signals (unix).
#[test]
fn test_uncertain_teardowns_converge_and_a_timed_out_teardown_is_uncertain() {
    let slow = command_unit("slow", "true", Some("sleep 5"), "  idempotent: true\n  timeout: 1s\n");
    let flaky = command_unit(
        "flaky",
        "true",
        Some("if [ -e %SCENARIO_TMP%/die ]; then kill -9 $$; fi"),
        "  idempotent: true\n",
    );
    let units = if cfg!(unix) {
        format!("{flaky}---\n{slow}")
    } else {
        slow
    };
    let (_project, harness) = harness_with(&units);
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    std::fs::write(harness.world.scenario_tmp().join("die"), "").unwrap();
    remove_units(&harness);
    let uncertain = reconcile_allowing(&harness, false);
    assert_eq!(uncertain.exit.code(), 4, "{uncertain:#?}");
    let expected: &[&str] = if cfg!(unix) { &["flaky", "slow"] } else { &["slow"] };
    assert_eq!(outcome(&uncertain, "uncertain"), set(expected));
    assert!(uncertain_run(harness.snapshot().observed.get(&unit("slow"))).is_some());

    if cfg!(unix) {
        std::fs::remove_file(harness.world.scenario_tmp().join("die")).unwrap();
        let converged = harness.reconcile(&[]);
        assert_eq!(outcome(&converged, "removed"), set(&["flaky"]), "{converged:#?}");
    }
}

/// Replace (`nyl teardown --unit` of a unit in the ownership set) and Q3:
/// the next reconcile starts a new incarnation; a dependent whose key is
/// unchanged stays current, one consuming an output that changed runs.
#[test]
fn test_replace_keeps_dependents_with_an_unchanged_key_current() {
    let units = format!(
        "{}---\n{}---\n{}",
        command_unit(
            "base",
            "n=$(cat %SCENARIO_TMP%/count 2>/dev/null || echo 0); n=$((n+1)); echo $n > %SCENARIO_TMP%/count; printf ''{\"value\": \"v1\", \"stamp\": \"%s\"}'' $n > \"$NYL_OUTPUTS\"",
            Some("true"),
            "  outputs:\n    value: {type: string}\n    stamp: {type: string}\n",
        ),
        command_unit(
            "fixed",
            "true",
            None,
            "  values:\n    value: {fromUnit: {unit: base, output: value}}\n",
        ),
        command_unit(
            "stamped",
            "true",
            None,
            "  values:\n    stamp: {fromUnit: {unit: base, output: stamp}}\n",
        ),
    );
    let (_project, harness) = harness_with(&units);
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let before = harness.snapshot().desired[&unit("base")].unit.uid.clone();

    let replaced = teardown(&harness, TeardownTarget::Units(units_of(&["base"])), |_| {});
    assert_eq!(replaced.exit.code(), 0, "{replaced:#?}");
    assert_eq!(outcome(&replaced, "removed"), set(&["base"]));
    assert!(!harness.snapshot().desired.contains_key(&unit("base")));

    let next = harness.reconcile(&[]);
    assert_eq!(next.exit.code(), 0, "{next:#?}");
    assert_eq!(outcome(&next, "executed"), set(&["base", "stamped"]));
    assert_eq!(outcome(&next, "current"), set(&["fixed"]));
    assert_ne!(harness.snapshot().desired[&unit("base")].unit.uid, before);
}

/// "Tear down, then hold": `teardown --unit X --hold` leaves a held file
/// without a uid, so no later run recreates the unit.
#[test]
fn test_teardown_with_hold_leaves_a_held_file_without_a_uid() {
    let (_project, harness) = harness_with(&command_unit("svc", "true", Some("true"), ""));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let report = teardown(&harness, TeardownTarget::Units(units_of(&["svc"])), |job| {
        job.request.lifecycle.hold = true;
    });
    assert_eq!(outcome(&report, "removed"), set(&["svc"]), "{report:#?}");
    let snapshot = harness.snapshot();
    let svc = &snapshot.desired[&unit("svc")];
    assert!(matches!(svc.lifecycle, Lifecycle::Held { deletion: None, .. }));
    assert_eq!(svc.unit.uid, None);
    assert!(!snapshot.observed.contains_key(&unit("svc")));

    let next = harness.reconcile(&[]);
    assert_eq!(outcome(&next, "held"), set(&["svc"]), "{next:#?}");
    assert_eq!(harness.snapshot().desired[&unit("svc")].unit.uid, None);
}

/// Q26: a teardown of a held unit records the request and tears nothing
/// down (exit 0); after `resume` the next reconcile carries it out.
#[test]
fn test_a_teardown_of_a_held_unit_is_carried_out_after_resume() {
    let (_project, harness) = harness_with(&command_unit(
        "svc",
        "touch %SCENARIO_TMP%/svc",
        Some("rm %SCENARIO_TMP%/svc"),
        "",
    ));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    put_lifecycle(&harness, "svc", Event::Hold { hold: hold() });

    let recorded = teardown(&harness, TeardownTarget::Units(units_of(&["svc"])), |_| {});
    assert_eq!(recorded.exit.code(), 0, "{recorded:#?}");
    assert_eq!(
        recorded.units[&unit("svc")].result,
        Some(UnitResult::Held {
            pending: None,
            teardown: true,
            recorded: true
        })
    );
    assert!(
        harness.world.scenario_tmp().join("svc").exists(),
        "nothing was torn down"
    );
    let Lifecycle::Held {
        deletion: Some(deletion),
        ..
    } = lifecycle_of(&harness.snapshot(), "svc")
    else {
        panic!("the request is recorded on the held file");
    };
    assert_eq!(deletion.reason, DeletionReason::Replace);

    put_lifecycle(&harness, "svc", Event::Resume);
    let removed = harness.reconcile(&[]);
    assert_eq!(outcome(&removed, "removed"), set(&["svc"]), "{removed:#?}");
    assert!(!harness.world.scenario_tmp().join("svc").exists());
}

/// Q7: `teardown --all` tears down what can be torn down, releases kinds
/// without teardown and declared `Retain` units (their resources stay,
/// listed under `details.released`), and holds the units of the ownership
/// set without an incarnation.
#[test]
fn test_teardown_all_honours_retain_and_holds_the_ownership_set() {
    let (_project, harness) = harness_with(&format!(
        "{}---\n{}---\n{}",
        command_unit("svc", "touch %SCENARIO_TMP%/svc", Some("rm %SCENARIO_TMP%/svc"), ""),
        command_unit("img", "touch %SCENARIO_TMP%/img", None, ""),
        command_unit(
            "keep",
            "touch %SCENARIO_TMP%/keep",
            Some("rm %SCENARIO_TMP%/keep"),
            "  deletionPolicy: Retain\n",
        ),
    ));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let report = teardown(&harness, TeardownTarget::All, |_| {});
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    assert_eq!(outcome(&report, "removed"), set(&["svc"]));
    assert_eq!(outcome(&report, "released"), set(&["img", "keep"]));
    let tmp = harness.world.scenario_tmp();
    assert!(!tmp.join("svc").exists());
    assert!(
        tmp.join("img").exists() && tmp.join("keep").exists(),
        "released resources stay"
    );
    let snapshot = harness.snapshot();
    for name in ["svc", "img", "keep"] {
        let document = &snapshot.desired[&unit(name)];
        assert!(matches!(document.lifecycle, Lifecycle::Held { .. }), "{name}");
        assert_eq!(document.unit.uid, None, "{name}");
    }
    assert!(snapshot.observed.is_empty());
}

/// Holds: a held unit's consumers resolve against its receipt, so a
/// consumer change runs while the producer's frozen state keeps running.
#[test]
fn test_a_held_units_consumers_resolve_its_receipt() {
    let producer = command_unit(
        "producer",
        "printf ''{\"value\": \"one\"}'' > \"$NYL_OUTPUTS\"",
        None,
        "  outputs:\n    value: {type: string}\n",
    );
    let consumer = |extra: &str| {
        command_unit(
            "consumer",
            "true",
            None,
            &format!("  values:\n    value: {{fromUnit: {{unit: producer, output: value}}}}\n{extra}"),
        )
    };
    let (_project, harness) = harness_with(&format!("{producer}---\n{}", consumer("")));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    put_lifecycle(&harness, "producer", Event::Hold { hold: hold() });
    set_units(&harness, &format!("{producer}---\n{}", consumer("  workingDir: /\n")));
    let report = harness.reconcile(&[]);
    assert_eq!(outcome(&report, "executed"), set(&["consumer"]), "{report:#?}");
    assert_eq!(outcome(&report, "held"), set(&["producer"]));
}

/// Holds: a run records what source would change under `hold.pending`, and
/// writes nothing when that did not change since the last run.
#[test]
fn test_hold_pending_is_recorded_only_when_it_changes() {
    let (_project, harness) = harness_with(&command_unit("svc", "true", None, ""));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    put_lifecycle(&harness, "svc", Event::Hold { hold: hold() });
    set_units(&harness, &command_unit("svc", "true", None, "  workingDir: /\n"));
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit.code(), 2, "{first:#?}");
    assert!(first.commits.contains_key("desired"), "{first:#?}");
    let Lifecycle::Held { hold, .. } = lifecycle_of(&harness.snapshot(), "svc") else {
        panic!("svc is held");
    };
    let pending = hold.pending.expect("the pending change is recorded");
    assert!(pending.execution_key.is_some() && !pending.deletion);
    assert!(codes(&first.units[&unit("svc")].warnings).contains(&Code::UnitHeld));

    let second = harness.reconcile(&[]);
    assert!(second.commits.is_empty(), "{second:#?}");
}

/// Q26: a uid-less file left outside the ownership set after `resume` is
/// removed by the next reconcile.
#[test]
fn test_a_resumed_marker_outside_the_ownership_set_is_removed() {
    let (_project, harness) = harness_with(&command_unit("svc", "true", None, ""));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let marker = DesiredUnit::held_marker(dev(), unit("ghost"), hold());
    put(
        &harness,
        TransitionChanges {
            desired: BTreeMap::from([(unit("ghost"), Some(marker))]),
            ..TransitionChanges::default()
        },
    );
    let held = harness.reconcile(&[]);
    assert_eq!(outcome(&held, "held"), set(&["ghost"]), "{held:#?}");
    assert!(harness.snapshot().desired.contains_key(&unit("ghost")));
    put_lifecycle(&harness, "ghost", Event::Resume);
    let report = harness.reconcile(&[]);
    assert_eq!(report.exit.code(), 0, "{report:#?}");
    assert!(!harness.snapshot().desired.contains_key(&unit("ghost")));
}

/// "Runs and leases" item 4: a unit a dead run created and the next run no
/// longer selects leaves like any other unit: pending its teardown.
#[test]
fn test_an_adopted_unit_no_longer_selected_is_pending_teardown() {
    let (_project, mut harness) = {
        let units = format!(
            "{}---\n{}",
            command_unit("alpha", "true", None, ""),
            command_unit("beta", "true", Some("true"), "  idempotent: true\n"),
        );
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", ENVIRONMENT),
            ("nyl/units.yaml", &units),
        ]);
        let harness = Harness::new(project.path(), false);
        (project, harness)
    };
    harness.faults = Box::new(FaultSlot::kill_once(
        nyl_orchestration::fault::FaultPoint::AfterEffect,
        Some("beta"),
    ));
    harness.init();
    let _crashed = harness.reconcile(&[]);
    set_units(&harness, &command_unit("alpha", "true", None, ""));
    harness.advance(41);
    let report = harness.reconcile(&[]);
    assert_eq!(outcome(&report, "pending-teardown"), set(&["beta"]), "{report:#?}");
    let snapshot = harness.snapshot();
    assert!(matches!(lifecycle_of(&snapshot, "beta"), Lifecycle::Deleting { .. }));
    assert!(snapshot.desired[&unit("beta")].unit.uid.is_some());
}

/// Removing a unit drops its observations and attestations with its
/// observed file, so nothing of the old incarnation is left behind.
#[test]
fn test_removing_a_unit_drops_its_observations() {
    let (_project, harness) = harness_with(&command_unit("svc", "true", Some("true"), ""));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let snapshot = harness.snapshot();
    let observed = &snapshot.observed[&unit("svc")];
    let observation = Observation {
        api_version: StateV1,
        kind: nyl_core::orchestration::state::ObservationKind,
        unit: observed.unit.clone(),
        observation_type: ObservationType::Drift,
        execution_key: observed.receipt.as_ref().unwrap().execution_key.clone(),
        at: harness.clock.now(),
        result: serde_json::from_value(serde_json::json!({"drift": "drifted", "summary": "changed"})).unwrap(),
        details: serde_json::Value::Null,
    };
    put(
        &harness,
        TransitionChanges {
            observations: BTreeMap::from([(
                unit("svc"),
                Some(BTreeMap::from([(ObservationType::Drift, observation)])),
            )]),
            ..TransitionChanges::default()
        },
    );
    assert!(harness.snapshot().observations.contains_key(&unit("svc")));
    remove_units(&harness);
    let report = reconcile_allowing(&harness, false);
    assert_eq!(outcome(&report, "removed"), set(&["svc"]), "{report:#?}");
    let snapshot = harness.snapshot();
    assert!(!snapshot.observations.contains_key(&unit("svc")));
    assert!(!snapshot.observed.contains_key(&unit("svc")));
}

/// Q4: a teardown runs in the unit's recorded `sourceRevision`; when that
/// commit cannot be fetched it fails before effects with a fix naming
/// `--source`, and `--source` tears it down with that revision's files.
#[test]
fn test_teardown_of_an_unfetchable_revision_needs_source() {
    let (_project, harness) = harness_with(&command_unit(
        "svc",
        "touch %SCENARIO_TMP%/svc",
        Some("rm %SCENARIO_TMP%/svc"),
        "",
    ));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let mut stored = harness.snapshot().desired[&unit("svc")].clone();
    stored.source_revision = Some(CommitId::new("1".repeat(40)).unwrap());
    put(
        &harness,
        TransitionChanges {
            desired: BTreeMap::from([(unit("svc"), Some(stored))]),
            ..TransitionChanges::default()
        },
    );
    let target = || TeardownTarget::Units(units_of(&["svc"]));
    let failed = teardown(&harness, target(), |_| {});
    assert_eq!(failed.exit.code(), 1, "{failed:#?}");
    assert!(
        failed.errors.iter().any(|error| error.fix.contains("--source")),
        "{failed:#?}"
    );
    assert!(harness.world.scenario_tmp().join("svc").exists());

    let removed = teardown(&harness, target(), |job| {
        job.request.source_override = Some("main".to_owned());
    });
    assert_eq!(outcome(&removed, "removed"), set(&["svc"]), "{removed:#?}");
    assert!(!harness.world.scenario_tmp().join("svc").exists());
}

/// Runs and leases, item 4, for a fenced name: a run killed during the
/// teardown of a unit whose kind changed leaves that teardown without a
/// checkpoint. The next reconcile, with the new kind in source, takes the
/// run over and marks the deleting incarnation uncertain, so the
/// interrupted teardown is not lost with the run ref.
#[test]
fn test_a_takeover_marks_an_interrupted_kind_change_teardown_uncertain() {
    let (_project, mut harness) = harness_with(&command_unit(
        "db",
        "touch %SCENARIO_TMP%/db",
        Some("rm %SCENARIO_TMP%/db"),
        "",
    ));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let old_uid = harness.snapshot().desired[&unit("db")].unit.uid.clone();
    harness.world.commit_files(
        "main",
        "Build db as an image",
        &[
            (
                "nyl/units.yaml",
                "apiVersion: units.gitops.nyl/v1\nkind: OciImage\nmetadata:\n  name: db\n  labels: {dev: 'true'}\nspec:\n  context: {path: ../image}\n  repository: '%REGISTRY%/db'\n",
            ),
            ("image/Dockerfile", "FROM scratch\n"),
        ],
    );
    harness.faults = Box::new(FaultSlot::kill_once(FaultPoint::AfterEffect, Some("db")));
    reconcile_allowing(&harness, false);
    assert!(
        !harness.world.scenario_tmp().join("db").exists(),
        "the teardown's effect happened"
    );
    harness.faults = Box::new(NoFaults);
    harness.advance(24 * 60);

    let takeover = harness.reconcile(&[]);
    assert!(takeover.recovered.contains(&unit("db")), "{takeover:#?}");
    let snapshot = harness.snapshot();
    let observed = &snapshot.observed[&unit("db")];
    assert_eq!(observed.unit.uid, old_uid, "the deleting incarnation");
    assert!(
        matches!(observed.condition, Some(Condition::Uncertain { .. })),
        "{:?}",
        observed.condition
    );
}

/// `--fail-fast`: after a failure the run starts no further unit, still
/// commits the finished ones, and says why; without it the independent
/// unit runs.
#[test]
fn test_fail_fast_starts_no_unit_after_a_failure() {
    let units = format!(
        "{}---\n{}",
        command_unit("bad", "exit 1", None, ""),
        command_unit("good", "true", None, ""),
    );
    let (_project, harness) = harness_with(&units);
    let stopped = run_job(&harness, |job| {
        job.request.fail_fast = true;
        job.request.concurrency = 1;
    });
    assert_eq!(stopped.exit.code(), 3, "{stopped:#?}");
    assert_eq!(outcome(&stopped, "failed"), set(&["bad"]));
    assert!(matches!(
        &stopped.units[&unit("good")].result,
        Some(UnitResult::Blocked { reason }) if reason.contains("--fail-fast")
    ));
    assert_eq!(codes(&stopped.warnings), [Code::RunStopped]);
    let snapshot = harness.snapshot();
    assert!(
        snapshot.observed[&unit("bad")].condition.is_some(),
        "the failure is committed"
    );
    assert!(!snapshot.observed.contains_key(&unit("good")));

    let continued = run_job(&harness, |job| job.request.concurrency = 1);
    assert_eq!(outcome(&continued, "executed"), set(&["good"]), "{continued:#?}");
}

/// `--fail-fast` on `nyl teardown`, which shares the run path: after a
/// failed teardown the run starts no further teardown, still commits what
/// finished, and the next teardown carries on with the rest.
#[test]
fn test_fail_fast_teardown_starts_no_teardown_after_a_failure() {
    let (_project, harness) = harness_with(&format!(
        "{}---\n{}",
        command_unit("bad", "true", Some("exit 1"), ""),
        command_unit("good", "touch %SCENARIO_TMP%/good", Some("rm %SCENARIO_TMP%/good"), ""),
    ));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let stopped = teardown(&harness, TeardownTarget::All, |job| {
        job.request.fail_fast = true;
        job.request.concurrency = 1;
    });
    assert_eq!(stopped.exit.code(), 3, "{stopped:#?}");
    assert_eq!(outcome(&stopped, "failed"), set(&["bad"]), "{stopped:#?}");
    assert!(
        harness.world.scenario_tmp().join("good").exists(),
        "no teardown starts after the failure"
    );
    assert_eq!(codes(&stopped.warnings), [Code::RunStopped], "{stopped:#?}");
    assert!(harness.snapshot().observed[&unit("bad")].condition.is_some());

    let continued = teardown(&harness, TeardownTarget::All, |job| job.request.concurrency = 1);
    assert_eq!(outcome(&continued, "removed"), set(&["good"]), "{continued:#?}");
    assert!(!harness.world.scenario_tmp().join("good").exists());
}

/// Q28: a hold sent to a running run takes effect before the run starts the
/// unit: it does not run, and the run writes the hold in its own commit.
#[test]
fn test_a_hold_signal_keeps_the_unit_from_starting() {
    let (_project, harness) = harness_with(&format!(
        "{}---\n{}",
        command_unit("first", "true", None, ""),
        command_unit("second", "touch %SCENARIO_TMP%/second", None, ""),
    ));
    // The next run's id: the identifiers are sequential.
    let probe = harness.ids_probe();
    let signal = Signal {
        api_version: StateV1,
        kind: SignalKind,
        run: probe,
        action: SignalAction::Hold,
        unit: unit("second"),
        phase1_commit: None,
        by: "alice".to_owned(),
        identity: ApprovalIdentity::Local,
        reason: Some("incident".to_owned()),
        at: harness.clock.now(),
        from: "alice@workstation".to_owned(),
    };
    harness.store.send_signal(&harness.location, &dev(), &signal).unwrap();
    let report = harness.reconcile(&[]);
    assert_eq!(outcome(&report, "held"), set(&["second"]), "{report:#?}");
    assert!(!harness.world.scenario_tmp().join("second").exists());
    assert!(matches!(
        lifecycle_of(&harness.snapshot(), "second"),
        Lifecycle::Held { .. }
    ));
}

trait Probe {
    fn ids_probe(&self) -> RunId;
}

impl Probe for Harness {
    /// The run id the next run gets, consuming one before it.
    fn ids_probe(&self) -> RunId {
        use nyl_core::orchestration::ports::IdGenerator;
        let current = self.ids.run_id();
        let number = u64::from_str_radix(current.as_str().rsplit('-').next().unwrap(), 16).unwrap();
        RunId::new(format!("00000000-0000-7000-9000-{:012x}", number + 1)).unwrap()
    }
}

/// Releasing an intermediate unit must not let a producer overtake a
/// blocked consumer, and a completed release must unblock that producer.
#[test]
fn test_teardown_all_orders_a_release_between_consumer_and_producer() {
    let consumer = |teardown: &str| command_unit("consumer", "true", Some(teardown), "  dependsOn: [release]\n");
    let release = command_unit("release", "true", None, "  dependsOn: [producer]\n");
    let producer = command_unit(
        "producer",
        "touch %SCENARIO_TMP%/producer",
        Some("rm %SCENARIO_TMP%/producer"),
        "",
    );
    let (_project, harness) = harness_with(&format!("{}---\n{}---\n{}", consumer("exit 1"), release, producer));
    assert_eq!(harness.reconcile(&[]).exit.code(), 0);
    let failed = teardown(&harness, TeardownTarget::All, |_| {});
    assert_eq!(outcome(&failed, "failed"), set(&["consumer"]), "{failed:#?}");
    assert!(harness.world.scenario_tmp().join("producer").exists());
    assert!(harness.snapshot().desired[&unit("release")].unit.uid.is_some());

    // The explicit teardown uses its stored spec; repair that definition
    // and clear the failure as an operator would before retrying.
    let snapshot = harness.snapshot();
    let mut desired = snapshot.desired[&unit("consumer")].clone();
    desired.spec["teardown"] = serde_json::json!(["sh", "-c", "true"]);
    desired.resolved_spec.as_mut().unwrap()["teardown"] = desired.spec["teardown"].clone();
    let mut observed = snapshot.observed[&unit("consumer")].clone();
    observed.condition = None;
    put(
        &harness,
        TransitionChanges {
            desired: BTreeMap::from([(unit("consumer"), Some(desired))]),
            observed: BTreeMap::from([(unit("consumer"), Some(observed))]),
            ..TransitionChanges::default()
        },
    );
    let completed = teardown(&harness, TeardownTarget::All, |_| {});
    assert_eq!(completed.exit.code(), 0, "{completed:#?}");
    assert!(!harness.world.scenario_tmp().join("producer").exists());
    assert_eq!(outcome(&completed, "released"), set(&["release"]));
}
