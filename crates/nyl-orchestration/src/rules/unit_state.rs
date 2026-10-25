//! A unit's state and receipt state, for `status`, `get units -e`, and `plan`.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection):
//! each unit is in one of `blocked`, `ready`, `awaiting-approval`,
//! `running`, `uncertain`, `failed`, `current`, `held`, `pending-teardown`,
//! `tearing-down`, or `retained`, and its receipt is shown separately as
//! current, stale, or none.
//!
//! The state is the first that applies of: running (a live run names the
//! unit); held, tearing-down (teardown in progress), pending-teardown, or
//! retained by lifecycle; uncertain or failed by condition; blocked by
//! readiness; current by receipt; else ready. `awaiting-approval` arrives
//! with approvals (part 6).
//!
//! A broken lease declares its holder gone, its run ref included, as in
//! [`crate::rules::lease::decide`], and so does a lease or run ref past its
//! deadline: its run neither holds the environment nor runs units. When
//! such a run had not finished, status reports that it lost its lease
//! (`NYL-LEASE-LOST`, exit 4) and shows the units it may have left
//! half-executed as uncertain. Uncertain conditions warn with
//! `NYL-UNIT-UNCERTAIN`; fixes name `nyl recover --retry`, and `--accept
//! <run>` where a run or evidence ref holds a checkpointed receipt of the
//! unit's incarnation.

use std::collections::BTreeSet;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{Coordination, EnvironmentSnapshot, RunState};
use nyl_core::orchestration::state::{Condition, DesiredUnit, Lifecycle, ObservedUnit, Readiness, TeardownProgress};
use nyl_core::orchestration::time::Timestamp;

use crate::rules::freshness::receipt_is_current;
use crate::rules::lease::{holders, lease_live, run_live};
use crate::rules::takeover::checkpointed_receipt;
use crate::run_report::{BlockedBy, EnvironmentRunReport, ReceiptState, ReportKind, UnitReport, UnitState};

/// The stable reason of a blocker waiting for a producer's receipt.
pub const NO_CURRENT_RECEIPT: &str = "no-current-receipt";

/// Current, stale, or none, by [`crate::rules::freshness::receipt_is_current`].
pub fn receipt_state(desired: Option<&DesiredUnit>, observed: Option<&ObservedUnit>) -> ReceiptState {
    if observed.and_then(|observed| observed.receipt.as_ref()).is_none() {
        return ReceiptState::None;
    }
    match desired {
        Some(desired) if receipt_is_current(desired, observed) => ReceiptState::Current,
        _ => ReceiptState::Stale,
    }
}

/// A unit's state from its persisted documents; `running` when a live run
/// ref names it.
pub fn unit_state(desired: &DesiredUnit, observed: Option<&ObservedUnit>, running: bool) -> UnitState {
    if running {
        return UnitState::Running;
    }
    match &desired.lifecycle {
        Lifecycle::Active => {}
        Lifecycle::Held { .. } => return UnitState::Held,
        Lifecycle::Deleting { deletion } => {
            return if deletion.teardown == Some(TeardownProgress::InProgress) {
                UnitState::TearingDown
            } else {
                UnitState::PendingTeardown
            };
        }
        Lifecycle::Retained { .. } => return UnitState::Retained,
    }
    match observed.and_then(|observed| observed.condition.as_ref()) {
        Some(Condition::Uncertain { .. }) => return UnitState::Uncertain,
        Some(Condition::Failed { .. }) => return UnitState::Failed,
        None => {}
    }
    if matches!(desired.readiness, Readiness::Blocked { .. }) {
        return UnitState::Blocked;
    }
    if receipt_is_current(desired, observed) {
        UnitState::Current
    } else {
        UnitState::Ready
    }
}

/// The units an unfinished run ref names that it has not checkpointed yet.
fn unfinished_units(state: &RunState) -> impl Iterator<Item = &UnitName> {
    let done: BTreeSet<&UnitName> = state
        .checkpoints
        .iter()
        .map(|checkpoint| &checkpoint.desired.unit.name)
        .collect();
    state.run.units.iter().filter(move |unit| !done.contains(unit))
}

/// The units live runs are executing: those a live lease names, and those
/// a live, unfinished run ref names that it has not checkpointed yet.
fn running_units(coordination: &Coordination, now: Timestamp) -> BTreeSet<UnitName> {
    let mut running = BTreeSet::new();
    if let Some(held) = coordination.lease.as_ref().filter(|held| lease_live(&held.lease, now)) {
        running.extend(held.lease.units.iter().cloned());
    }
    for state in coordination
        .runs
        .iter()
        .filter(|state| run_live(coordination, state, now))
    {
        running.extend(unfinished_units(state).cloned());
    }
    running
}

/// A run that stopped before finishing: the run of a broken lease, or of a
/// lease or unfinished run ref past its deadline.
struct DeadRun<'a> {
    run: &'a RunId,
    broken: bool,
    /// The units it may have left half-executed: those its lease names and
    /// those its run ref has not checkpointed.
    units: BTreeSet<UnitName>,
}

/// The runs that lost their lease before finishing: a broken or expired
/// lease whose run ref (if any) is not finished, and unfinished run refs
/// past their deadline.
fn dead_runs(coordination: &Coordination, now: Timestamp) -> Vec<DeadRun<'_>> {
    let mut dead = Vec::new();
    let mut lease_run = None;
    if let Some(held) = coordination.lease.as_ref().filter(|held| !lease_live(&held.lease, now)) {
        let run = &held.lease.run;
        lease_run = Some(run);
        let state = coordination.runs.iter().find(|state| &state.run.run == run);
        if !state.is_some_and(|state| state.run.finished) {
            let mut units: BTreeSet<UnitName> = held.lease.units.iter().cloned().collect();
            if let Some(state) = state {
                units.extend(unfinished_units(state).cloned());
            }
            dead.push(DeadRun {
                run,
                broken: held.lease.broken.is_some(),
                units,
            });
        }
    }
    for state in &coordination.runs {
        if state.run.finished || state.run.deadline > now || Some(&state.run.run) == lease_run {
            continue;
        }
        dead.push(DeadRun {
            run: &state.run.run,
            broken: false,
            units: unfinished_units(state).cloned().collect(),
        });
    }
    dead
}

/// Finished run refs no reconcile has imported yet, cancelled or stopped
/// after losing their lease, with the units they left uncertain: those their
/// record still names (executed without a recorded outcome) and those they
/// checkpointed with an uncertain condition of their own.
fn unimported_runs(coordination: &Coordination) -> Vec<(&RunId, BTreeSet<UnitName>)> {
    coordination
        .runs
        .iter()
        .filter(|state| state.run.finished)
        .map(|state| {
            let mut units: BTreeSet<UnitName> = state.run.units.iter().cloned().collect();
            units.extend(
                state
                    .checkpoints
                    .iter()
                    .filter(|checkpoint| {
                        matches!(
                            &checkpoint.observed.condition,
                            Some(Condition::Uncertain { run, .. }) if run == &state.run.run
                        )
                    })
                    .map(|checkpoint| checkpoint.desired.unit.name.clone()),
            );
            (&state.run.run, units)
        })
        .collect()
}

/// The runs, on run refs or evidence refs, that checkpointed a receipt of
/// `observed`'s incarnation: what `nyl recover --accept <run>` may adopt.
fn acceptable_runs<'c>(coordination: &'c Coordination, observed: &ObservedUnit) -> Vec<&'c RunId> {
    coordination
        .runs
        .iter()
        .chain(&coordination.evidence)
        .filter(|state| {
            state.checkpoints.iter().any(|checkpoint| {
                checkpoint.desired.unit.name == observed.unit.name
                    && checkpoint.desired.unit.uid == observed.unit.uid
                    && checkpointed_receipt(&state.run.run, checkpoint).is_some()
            })
        })
        .map(|state| &state.run.run)
        .collect()
}

/// A stored blocked reason in the report's form: `no-current-receipt` with
/// the producer when the reason says the producer has no current receipt.
fn blocked_by(pointer: &str, reason: &str) -> BlockedBy {
    let producer = reason
        .strip_suffix(" has no current receipt")
        .and_then(|unit| UnitName::new(unit).ok());
    match producer {
        Some(unit) => BlockedBy {
            pointer: pointer.to_owned(),
            unit: Some(unit),
            reason: NO_CURRENT_RECEIPT.to_owned(),
        },
        None => BlockedBy {
            pointer: pointer.to_owned(),
            unit: None,
            reason: reason.to_owned(),
        },
    }
}

/// The suggested next action for a unit's condition: `accept` lists the
/// runs whose checkpointed receipt `nyl recover --accept` may adopt.
fn condition_warning(
    environment: &EnvironmentName,
    unit: &UnitName,
    condition: &Condition,
    accept: &[&RunId],
) -> Finding {
    let retry = format!("`nyl recover -e {environment} --unit {unit} --retry --reason <text>`");
    match condition {
        Condition::Failed {
            retryable: true,
            message,
            ..
        } => Finding::new(
            Code::DriverError,
            format!("unit {unit} failed and will be retried: {message}"),
            format!("run nyl reconcile to retry {unit}"),
        ),
        Condition::Failed {
            retryable: false,
            message,
            ..
        } => Finding::new(
            Code::DriverError,
            format!("unit {unit} failed and is not retried automatically: {message}"),
            format!("fix the cause, then run {retry} so that the next nyl reconcile runs it again"),
        ),
        Condition::Uncertain { reason, .. } => {
            let accept = accept
                .iter()
                .map(|run| format!("`nyl recover -e {environment} --unit {unit} --accept {run} --reason <text>`"))
                .collect::<Vec<_>>();
            let accept = if accept.is_empty() {
                String::new()
            } else {
                format!(
                    "; or, if the checkpointed result is what happened, adopt it with {}",
                    accept.join(" or ")
                )
            };
            Finding::new(
                Code::UnitUncertain,
                format!("unit {unit} is uncertain: {reason}"),
                format!(
                    "check what the execution did; a unit whose recovery policy converges runs again in the next nyl \
                     reconcile, otherwise run {retry} once it is safe to run it again{accept}"
                ),
            )
        }
    }
}

/// The `status -e` report of one environment from one snapshot of both refs
/// plus the lease and run refs: per-unit state, receipt, blockers, the
/// running unit and holder, and suggested next actions as warnings with fixes.
/// `now` decides which leases and run refs are past their deadline (the
/// units a dead run was executing read `uncertain`).
pub fn status(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    coordination: &Coordination,
    now: Timestamp,
) -> EnvironmentRunReport {
    let mut report = EnvironmentRunReport::new(environment.clone());
    report.source_commit = snapshot.environment.as_ref().map(|record| record.source_commit.clone());
    let running = running_units(coordination, now);
    let mut half_executed = BTreeSet::new();
    for lost in dead_runs(coordination, now) {
        let how = if lost.broken {
            "lost its lease to nyl lease break"
        } else {
            "passed its lease deadline"
        };
        report.errors.push(Finding::new(
            Code::LeaseLost,
            format!("run {} {how} before it finished", lost.run),
            format!(
                "run nyl reconcile to take over the environment and import the run's checkpoints; a unit left \
                 uncertain runs again when its recovery policy converges, otherwise check it and run \
                 `nyl recover -e {environment} --unit <unit> --retry --reason <text>`"
            ),
        ));
        half_executed.extend(lost.units);
    }
    for (run, units) in unimported_runs(coordination) {
        let fix = "run nyl reconcile: it imports the run's checkpoints; a unit left uncertain runs again when its \
                   recovery policy converges";
        if units.is_empty() {
            report.warnings.push(Finding::new(
                Code::RunUnimported,
                format!("run {run} ended without a transition commit; its checkpoints wait for the next reconcile"),
                fix,
            ));
        } else {
            let names = units.iter().map(UnitName::as_str).collect::<Vec<_>>().join(", ");
            report.errors.push(Finding::new(
                Code::RunUnimported,
                format!("run {run} ended without a transition commit and left {names} uncertain"),
                format!(
                    "{fix}, otherwise check it and run `nyl recover -e {environment} --unit <unit> --retry --reason \
                     <text>` after that reconcile"
                ),
            ));
            half_executed.extend(units);
        }
    }
    for (name, desired) in &snapshot.desired {
        let observed = snapshot.observed.get(name);
        let mut unit = UnitReport::new(desired.unit.kind.clone());
        unit.state = Some(if half_executed.contains(name) {
            // The run may have executed it without recording the outcome.
            UnitState::Uncertain
        } else {
            unit_state(desired, observed, running.contains(name))
        });
        unit.receipt = Some(receipt_state(Some(desired), observed));
        let condition = observed.and_then(|observed| observed.condition.as_ref());
        unit.last_run = observed.and_then(ObservedUnit::last_run).cloned();
        if let Readiness::Blocked { reasons } = &desired.readiness {
            unit.blocked_by = reasons
                .iter()
                .map(|reason| blocked_by(&reason.pointer, &reason.reason))
                .collect();
        }
        if let Some(condition) = condition {
            unit.message = Some(condition.message().to_owned());
            if desired.lifecycle == Lifecycle::Active {
                let accept = observed
                    .map(|observed| acceptable_runs(coordination, observed))
                    .unwrap_or_default();
                unit.warnings
                    .push(condition_warning(environment, name, condition, &accept));
            }
        }
        report.units.insert(name.clone(), unit);
    }
    for holder in holders(coordination, now) {
        report.warnings.push(Finding::new(
            Code::LeaseHeld,
            format!("the environment is held by {}", holder.describe()),
            "wait for the run to finish, or run nyl lease break if its runner is gone",
        ));
    }
    report.finish(ReportKind::StatusReport);
    report
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nyl_core::orchestration::ports::{Checkpoint, HeldLease};
    use nyl_core::orchestration::state::{Lease, Run};
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{
        desired_numbered, hex_key, lease_record, name, observed_numbered, run_record, run_state, RUN,
    };
    use crate::run_report::ExitCategory;

    fn condition(value: serde_json::Value) -> Condition {
        serde_json::from_value(value).unwrap()
    }

    fn failed(retryable: bool) -> Condition {
        condition(json!({
            "state": "failed", "since": "2026-01-01T00:00:00Z", "run": RUN,
            "category": "tool-error", "retryable": retryable, "message": "exit 1"
        }))
    }

    /// Catches a receipt shown as current after its inputs changed, or as
    /// stale when there is none.
    #[test]
    fn test_receipt_state_is_current_stale_or_none() {
        let current = desired_numbered("network", 1, Some(hex_key('a')));
        assert_eq!(
            receipt_state(
                Some(&current),
                Some(&observed_numbered("network", 1, Some(hex_key('a'))))
            ),
            ReceiptState::Current
        );
        assert_eq!(
            receipt_state(
                Some(&current),
                Some(&observed_numbered("network", 1, Some(hex_key('b'))))
            ),
            ReceiptState::Stale
        );
        assert_eq!(
            receipt_state(Some(&current), Some(&observed_numbered("network", 1, None))),
            ReceiptState::None
        );
        assert_eq!(receipt_state(Some(&current), None), ReceiptState::None);
    }

    /// Catches a state hiding a more urgent one, such as a failed unit
    /// reported as current because an older receipt still matches.
    #[test]
    fn test_unit_state_follows_the_priority_order() {
        let current = desired_numbered("network", 1, Some(hex_key('a')));
        let mut failing = observed_numbered("network", 1, Some(hex_key('a')));
        failing.condition = Some(failed(true));
        assert_eq!(unit_state(&current, Some(&failing), true), UnitState::Running);
        assert_eq!(unit_state(&current, Some(&failing), false), UnitState::Failed);

        let mut uncertain = observed_numbered("network", 1, Some(hex_key('a')));
        uncertain.condition = Some(condition(json!({
            "state": "uncertain", "since": "2026-01-01T00:00:00Z", "run": RUN, "reason": "lost its lease"
        })));
        assert_eq!(unit_state(&current, Some(&uncertain), false), UnitState::Uncertain);

        let mut held = current.clone();
        held.lifecycle = serde_json::from_value(json!({
            "state": "held", "hold": {"since": "2026-01-01T00:00:00Z", "by": "jane"}
        }))
        .unwrap();
        assert_eq!(unit_state(&held, Some(&failing), false), UnitState::Held);

        let deleting = |teardown: &str| {
            let mut unit = current.clone();
            unit.lifecycle = serde_json::from_value(json!({
                "state": "deleting",
                "deletion": {"reason": "omission", "intent": "teardown", "teardown": teardown}
            }))
            .unwrap();
            unit
        };
        assert_eq!(
            unit_state(&deleting("in-progress"), None, false),
            UnitState::TearingDown
        );
        assert_eq!(
            unit_state(&deleting("pending"), None, false),
            UnitState::PendingTeardown
        );

        let mut retained = current.clone();
        retained.lifecycle = serde_json::from_value(json!({
            "state": "retained", "tombstone": {"since": "2026-01-01T00:00:00Z"}
        }))
        .unwrap();
        assert_eq!(unit_state(&retained, None, false), UnitState::Retained);

        let mut blocked = desired_numbered("network", 1, None);
        blocked.readiness = Readiness::Blocked { reasons: Vec::new() };
        assert_eq!(
            unit_state(
                &blocked,
                Some(&observed_numbered("network", 1, Some(hex_key('a')))),
                false
            ),
            UnitState::Blocked
        );
        assert_eq!(
            unit_state(
                &current,
                Some(&observed_numbered("network", 1, Some(hex_key('a')))),
                false
            ),
            UnitState::Current
        );
        assert_eq!(
            unit_state(
                &current,
                Some(&observed_numbered("network", 1, Some(hex_key('b')))),
                false
            ),
            UnitState::Ready
        );
    }

    /// Units of the given names and incarnations, each with a current receipt.
    fn snapshot(units: &[(&str, u64)]) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            desired: units
                .iter()
                .map(|(unit, uid)| (name(unit), desired_numbered(unit, *uid, Some(hex_key('a')))))
                .collect(),
            observed: units
                .iter()
                .map(|(unit, uid)| (name(unit), observed_numbered(unit, *uid, Some(hex_key('a')))))
                .collect(),
            ..EnvironmentSnapshot::default()
        }
    }

    /// A time on 2026-01-01, as `HH:MM`.
    fn at(time: &str) -> Timestamp {
        format!("2026-01-01T{time}:00Z").parse().unwrap()
    }

    fn held(lease: Lease) -> HeldLease {
        HeldLease {
            lease,
            version: "1".to_owned(),
        }
    }

    /// Run [`RUN`]'s record, due at 01:00, naming `units`.
    fn run_naming(units: &[&str]) -> Run {
        Run {
            units: units.iter().map(|unit| name(unit)).collect(),
            ..run_record(RUN, at("01:00"))
        }
    }

    fn status_at(snapshot: &EnvironmentSnapshot, coordination: &Coordination, now: Timestamp) -> EnvironmentRunReport {
        status(&EnvironmentName::new("dev").unwrap(), snapshot, coordination, now)
    }

    /// Catches status losing what the operator needs next: the blocker's
    /// producer, the suggested command for a failure, and the lease holder;
    /// and status exiting 0 while units are not current.
    #[test]
    fn test_status_reports_blockers_suggestions_and_the_holder() {
        let mut snapshot = snapshot(&[("network", 1), ("web", 3)]);
        let mut blocked = desired_numbered("database", 2, None);
        blocked.readiness = serde_json::from_value(json!({
            "state": "blocked",
            "reasons": [{"pointer": "/values/vpc", "reason": "network has no current receipt"}]
        }))
        .unwrap();
        snapshot.desired.insert(name("database"), blocked);
        snapshot.observed.get_mut(&name("web")).unwrap().condition = Some(failed(false));
        let coordination = Coordination {
            lease: Some(held(Lease {
                units: vec![name("network")],
                ..lease_record(RUN, at("01:00"))
            })),
            runs: Vec::new(),
            evidence: Vec::new(),
        };
        let report = status_at(&snapshot, &coordination, Timestamp::UNIX_EPOCH);

        let unit = |unit: &str| &report.units[&name(unit)];
        assert_eq!(unit("network").state, Some(UnitState::Running));
        assert_eq!(unit("database").state, Some(UnitState::Blocked));
        assert_eq!(
            unit("database").blocked_by,
            [BlockedBy {
                pointer: "/values/vpc".to_owned(),
                unit: Some(name("network")),
                reason: NO_CURRENT_RECEIPT.to_owned(),
            }]
        );
        let web = unit("web");
        assert_eq!(web.state, Some(UnitState::Failed));
        assert_eq!(web.receipt, Some(ReceiptState::Current));
        assert!(
            web.warnings[0].fix.contains("nyl recover -e dev --unit web --retry"),
            "{:?}",
            web.warnings
        );
        assert!(web.last_run.is_some());
        assert!(report.warnings[0].message.contains("by ci"), "{:?}", report.warnings);
        assert_eq!(report.exit, ExitCategory::Failed);
        assert!(!report.complete);
    }

    /// Catches status still treating a run whose lease `nyl lease break`
    /// broke as running and holding the environment (exit 2), where the
    /// contract says a run that lost its lease exits 4 and its half-executed
    /// units need recovery.
    #[test]
    fn test_status_reports_a_broken_lease_as_lost_not_running() {
        let snapshot = snapshot(&[("network", 1), ("web", 3)]);
        let coordination = Coordination {
            lease: Some(held(Lease {
                broken: Some(serde_json::from_value(json!({"by": "alice", "at": "2026-01-01T00:10:00Z"})).unwrap()),
                ..lease_record(RUN, at("01:00"))
            })),
            runs: vec![run_state(run_naming(&["web"]))],
            evidence: Vec::new(),
        };
        let report = status_at(&snapshot, &coordination, Timestamp::UNIX_EPOCH);

        assert_eq!(report.units[&name("web")].state, Some(UnitState::Uncertain));
        assert_eq!(report.units[&name("network")].state, Some(UnitState::Current));
        assert!(
            report.warnings.iter().all(|warning| warning.code != Code::LeaseHeld),
            "{:?}",
            report.warnings
        );
        assert_eq!(report.errors[0].code, Code::LeaseLost);
        assert_eq!(report.exit, ExitCategory::Uncertain);

        // Once the broken run's ref says it finished with every unit
        // recorded, nothing was lost; it only waits to be imported.
        let mut finished = coordination.clone();
        finished.runs[0].run.finished = true;
        finished.runs[0].run.units.clear();
        let report = status_at(&snapshot, &finished, Timestamp::UNIX_EPOCH);
        assert_eq!(report.units[&name("web")].state, Some(UnitState::Current));
        assert!(report.errors.is_empty());
        assert_eq!(codes(&report.warnings), [Code::RunUnimported]);
        assert_eq!(report.exit, ExitCategory::Ok);
    }

    /// Catches status reporting units as current while a cancelled run's
    /// ref, not yet imported, holds them uncertain: the ones it was running
    /// at the signal (checkpointed uncertain) and the ones it executed
    /// without recording an outcome (still named by its finished record).
    #[test]
    fn test_status_reports_units_an_unimported_run_left_uncertain() {
        let snapshot = snapshot(&[("network", 1), ("web", 3), ("db", 4)]);
        let mut cancelled = observed_numbered("web", 3, Some(hex_key('a')));
        cancelled.condition = Some(condition(json!({
            "state": "uncertain", "since": "2026-01-01T00:00:00Z", "run": RUN, "reason": "cancelled"
        })));
        let coordination = Coordination {
            lease: None,
            runs: vec![RunState {
                checkpoints: vec![Checkpoint {
                    desired: desired_numbered("web", 3, Some(hex_key('a'))),
                    observed: cancelled,
                    artifacts: BTreeMap::new(),
                }],
                ..run_state(Run {
                    finished: true,
                    ..run_naming(&["db"])
                })
            }],
            evidence: Vec::new(),
        };
        let report = status_at(&snapshot, &coordination, Timestamp::UNIX_EPOCH);

        let unit = |unit: &str| &report.units[&name(unit)];
        assert_eq!(unit("web").state, Some(UnitState::Uncertain));
        assert_eq!(unit("db").state, Some(UnitState::Uncertain));
        assert_eq!(unit("network").state, Some(UnitState::Current));
        assert_eq!(codes(&report.errors), [Code::RunUnimported]);
        assert!(
            report.errors[0].message.contains("left db, web uncertain"),
            "{:?}",
            report.errors
        );
        assert_eq!(report.exit, ExitCategory::Uncertain);
    }

    /// Catches status reporting a crashed run, whose lease expired without
    /// a break, as still holding the environment and running its units,
    /// and fixes that do not name the `nyl recover` commands, including
    /// `--accept` for a checkpointed receipt on an evidence ref.
    #[test]
    fn test_status_reports_an_expired_lease_as_lost_and_names_recover() {
        let mut snapshot = snapshot(&[("database", 2), ("web", 3)]);
        snapshot.observed.get_mut(&name("database")).unwrap().condition = Some(condition(json!({
            "state": "uncertain", "since": "2026-01-01T03:00:00Z", "run": RUN, "reason": "superseded"
        })));
        let dead = "00000000-0000-7000-8000-00000000000b";
        let coordination = Coordination {
            lease: Some(held(Lease {
                units: vec![name("web")],
                ..lease_record(dead, at("01:00"))
            })),
            runs: vec![run_state(Run {
                run: RunId::new(dead).unwrap(),
                ..run_naming(&["web"])
            })],
            evidence: vec![RunState {
                checkpoints: vec![Checkpoint {
                    desired: desired_numbered("database", 2, Some(hex_key('a'))),
                    observed: observed_numbered("database", 2, Some(hex_key('a'))),
                    artifacts: BTreeMap::new(),
                }],
                ..run_state(Run {
                    finished: true,
                    ..run_naming(&["web"])
                })
            }],
        };
        let report = status_at(&snapshot, &coordination, at("02:00"));

        let unit = |unit: &str| &report.units[&name(unit)];
        assert_eq!(unit("web").state, Some(UnitState::Uncertain));
        assert_eq!(codes(&report.errors), [Code::LeaseLost]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let warning = &unit("database").warnings[0];
        assert_eq!(warning.code, Code::UnitUncertain);
        assert!(
            warning.fix.contains("nyl recover -e dev --unit database --retry"),
            "{}",
            warning.fix
        );
        assert!(
            warning
                .fix
                .contains(&format!("nyl recover -e dev --unit database --accept {RUN}")),
            "{}",
            warning.fix
        );
        assert_eq!(report.exit, ExitCategory::Uncertain);

        // Before the deadline the same run holds the environment and runs web.
        let report = status_at(&snapshot, &coordination, at("00:30"));
        assert_eq!(report.units[&name("web")].state, Some(UnitState::Running));
        assert!(report.errors.is_empty());
    }

    fn codes(findings: &[Finding]) -> Vec<Code> {
        findings.iter().map(|finding| finding.code).collect()
    }
}
