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
//! [`crate::rules::lease::decide`]: its run neither holds the environment
//! nor runs units. When that run had not finished, status reports that it
//! lost its lease (`NYL-LEASE-LOST`, exit 4) and shows the units it may
//! have left half-executed as uncertain.

use std::collections::BTreeSet;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{Coordination, EnvironmentSnapshot, RunState};
use nyl_core::orchestration::state::{Condition, DesiredUnit, Lifecycle, ObservedUnit, Readiness, TeardownProgress};

use crate::rules::freshness::receipt_is_current;
use crate::rules::lease::Holder;
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

/// The run whose lease `nyl lease break` broke. Like
/// [`crate::rules::lease::decide`], a broken lease declares its holder
/// gone, its run ref included.
fn broken_run(coordination: &Coordination) -> Option<&RunId> {
    coordination
        .lease
        .as_ref()
        .filter(|held| held.lease.broken.is_some())
        .map(|held| &held.lease.run)
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

/// The units live runs are executing: those the unbroken lease names, and
/// those an unfinished run ref of a live run names that it has not
/// checkpointed yet.
fn running_units(coordination: &Coordination) -> BTreeSet<UnitName> {
    let mut running = BTreeSet::new();
    if let Some(held) = &coordination.lease {
        if held.lease.broken.is_none() {
            running.extend(held.lease.units.iter().cloned());
        }
    }
    let broken = broken_run(coordination);
    for state in coordination
        .runs
        .iter()
        .filter(|state| !state.run.finished && Some(&state.run.run) != broken)
    {
        running.extend(unfinished_units(state).cloned());
    }
    running
}

/// A run that lost its lease to `nyl lease break` before finishing, and the
/// units it may have left half-executed: those the broken lease names and
/// those its unfinished run ref has not checkpointed. `None` when the lease
/// is unbroken or the broken run's ref says it finished.
fn lost_run(coordination: &Coordination) -> Option<(&RunId, BTreeSet<UnitName>)> {
    let held = coordination.lease.as_ref().filter(|held| held.lease.broken.is_some())?;
    let run = &held.lease.run;
    let state = coordination.runs.iter().find(|state| &state.run.run == run);
    if state.is_some_and(|state| state.run.finished) {
        return None;
    }
    let mut units: BTreeSet<UnitName> = held.lease.units.iter().cloned().collect();
    if let Some(state) = state {
        units.extend(unfinished_units(state).cloned());
    }
    Some((run, units))
}

/// Who holds the environment: the unbroken lease, and unfinished runs it
/// does not name, except the run of a broken lease.
fn holders(coordination: &Coordination) -> Vec<Holder> {
    let mut holders = Vec::new();
    let mut lease_run: Option<&RunId> = broken_run(coordination);
    if let Some(held) = &coordination.lease {
        if held.lease.broken.is_none() {
            lease_run = Some(&held.lease.run);
            holders.push(Holder::Lease(held.lease.clone()));
        }
    }
    for state in &coordination.runs {
        if !state.run.finished && Some(&state.run.run) != lease_run {
            holders.push(Holder::Run(state.run.clone()));
        }
    }
    holders
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

/// The suggested next action for a unit's condition.
fn condition_warning(environment: &EnvironmentName, unit: &UnitName, condition: &Condition) -> Finding {
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
            format!(
                "fix the cause, then start over with `nyl state init -e {environment} --fresh`, which runs every unit \
                 again; for a Command, set idempotent: true if re-running is safe so later failures are retried \
                 (`nyl recover --unit {unit}` arrives in a later part of M3)"
            ),
        ),
        Condition::Uncertain { reason, .. } => Finding::new(
            Code::DriverError,
            format!("unit {unit} is uncertain: {reason}"),
            format!(
                "check what the execution did; for a Command, set idempotent: true if re-running is safe and the next \
                 nyl reconcile runs it again, otherwise start over with `nyl state init -e {environment} --fresh` \
                 (`nyl recover --unit {unit}` arrives in a later part of M3)"
            ),
        ),
    }
}

/// The `status -e` report of one environment from one snapshot of both refs
/// plus the lease and run refs: per-unit state, receipt, blockers, the
/// running unit and holder, and suggested next actions as warnings with fixes.
pub fn status(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    coordination: &Coordination,
) -> EnvironmentRunReport {
    let mut report = EnvironmentRunReport::new(environment.clone());
    report.source_commit = snapshot.environment.as_ref().map(|record| record.source_commit.clone());
    let running = running_units(coordination);
    let lost = lost_run(coordination);
    if let Some((run, _)) = &lost {
        report.errors.push(Finding::new(
            Code::LeaseLost,
            format!("run {run} lost its lease to nyl lease break before it finished"),
            format!(
                "run nyl reconcile to take over the environment and import the run's checkpoints, then check any \
                 unit left uncertain: it runs again only when its recovery policy converges (a Command with \
                 idempotent: true), otherwise start over with `nyl state init -e {environment} --fresh` \
                 (`nyl recover` arrives in a later part of M3)"
            ),
        ));
    }
    for (name, desired) in &snapshot.desired {
        let observed = snapshot.observed.get(name);
        let mut unit = UnitReport::new(desired.unit.kind.clone());
        let half_executed = lost.as_ref().is_some_and(|(_, units)| units.contains(name));
        unit.state = Some(if half_executed {
            // The run may have executed it without recording the outcome.
            UnitState::Uncertain
        } else {
            unit_state(desired, observed, running.contains(name))
        });
        unit.receipt = Some(receipt_state(Some(desired), observed));
        let condition = observed.and_then(|observed| observed.condition.as_ref());
        unit.last_run = match condition {
            Some(Condition::Failed { run, .. } | Condition::Uncertain { run, .. }) => Some(run.clone()),
            None => observed
                .and_then(|observed| observed.receipt.as_ref())
                .map(|receipt| receipt.run.clone()),
        };
        if let Readiness::Blocked { reasons } = &desired.readiness {
            unit.blocked_by = reasons
                .iter()
                .map(|reason| blocked_by(&reason.pointer, &reason.reason))
                .collect();
        }
        if let Some(condition) = condition {
            unit.message = Some(match condition {
                Condition::Failed { message, .. } => message.clone(),
                Condition::Uncertain { reason, .. } => reason.clone(),
            });
            if desired.lifecycle == Lifecycle::Active {
                unit.warnings.push(condition_warning(environment, name, condition));
            }
        }
        report.units.insert(name.clone(), unit);
    }
    for holder in holders(coordination) {
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

    use nyl_core::orchestration::ports::{HeldLease, RunState};
    use serde_json::json;

    use super::*;
    use crate::rules::freshness::fixtures::{desired, desired_named, key, observed, observed_named, RUN};
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
        let current = desired(1, Some(key('a')));
        assert_eq!(
            receipt_state(Some(&current), Some(&observed(1, Some(key('a'))))),
            ReceiptState::Current
        );
        assert_eq!(
            receipt_state(Some(&current), Some(&observed(1, Some(key('b'))))),
            ReceiptState::Stale
        );
        assert_eq!(
            receipt_state(Some(&current), Some(&observed(1, None))),
            ReceiptState::None
        );
        assert_eq!(receipt_state(Some(&current), None), ReceiptState::None);
    }

    /// Catches a state hiding a more urgent one, such as a failed unit
    /// reported as current because an older receipt still matches.
    #[test]
    fn test_unit_state_follows_the_priority_order() {
        let current = desired(1, Some(key('a')));
        let mut failing = observed(1, Some(key('a')));
        failing.condition = Some(failed(true));
        assert_eq!(unit_state(&current, Some(&failing), true), UnitState::Running);
        assert_eq!(unit_state(&current, Some(&failing), false), UnitState::Failed);

        let mut uncertain = observed(1, Some(key('a')));
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

        let mut blocked = desired(1, None);
        blocked.readiness = Readiness::Blocked { reasons: Vec::new() };
        assert_eq!(
            unit_state(&blocked, Some(&observed(1, Some(key('a')))), false),
            UnitState::Blocked
        );
        assert_eq!(
            unit_state(&current, Some(&observed(1, Some(key('a')))), false),
            UnitState::Current
        );
        assert_eq!(
            unit_state(&current, Some(&observed(1, Some(key('b')))), false),
            UnitState::Ready
        );
    }

    /// Catches status losing what the operator needs next: the blocker's
    /// producer, the suggested command for a failure, and the lease holder;
    /// and status exiting 0 while units are not current.
    #[test]
    fn test_status_reports_blockers_suggestions_and_the_holder() {
        let mut blocked = desired_named("database", 2, None);
        blocked.readiness = serde_json::from_value(json!({
            "state": "blocked",
            "reasons": [{"pointer": "/values/vpc", "reason": "network has no current receipt"}]
        }))
        .unwrap();
        let mut failing = observed_named("web", 3, Some(key('a')));
        failing.condition = Some(failed(false));
        let snapshot = EnvironmentSnapshot {
            desired: BTreeMap::from([
                (UnitName::new("network").unwrap(), desired(1, Some(key('a')))),
                (UnitName::new("database").unwrap(), blocked),
                (UnitName::new("web").unwrap(), desired_named("web", 3, Some(key('a')))),
            ]),
            observed: BTreeMap::from([
                (UnitName::new("network").unwrap(), observed(1, Some(key('a')))),
                (UnitName::new("web").unwrap(), failing),
            ]),
            ..EnvironmentSnapshot::default()
        };
        let lease = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Lease", "run": RUN, "runner": "ci",
            "operation": "reconcile", "units": ["network"],
            "deadline": "2026-01-01T01:00:00Z", "takenAt": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let coordination = Coordination {
            lease: Some(HeldLease {
                lease,
                version: "1".to_owned(),
            }),
            runs: Vec::<RunState>::new(),
        };
        let report = status(&EnvironmentName::new("dev").unwrap(), &snapshot, &coordination);

        let unit = |name: &str| &report.units[&UnitName::new(name).unwrap()];
        assert_eq!(unit("network").state, Some(UnitState::Running));
        assert_eq!(unit("database").state, Some(UnitState::Blocked));
        assert_eq!(
            unit("database").blocked_by,
            [BlockedBy {
                pointer: "/values/vpc".to_owned(),
                unit: Some(UnitName::new("network").unwrap()),
                reason: NO_CURRENT_RECEIPT.to_owned(),
            }]
        );
        let web = unit("web");
        assert_eq!(web.state, Some(UnitState::Failed));
        assert_eq!(web.receipt, Some(ReceiptState::Current));
        assert!(
            web.warnings[0].fix.contains("nyl state init -e dev --fresh"),
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
        let snapshot = EnvironmentSnapshot {
            desired: BTreeMap::from([
                (UnitName::new("network").unwrap(), desired(1, Some(key('a')))),
                (UnitName::new("web").unwrap(), desired_named("web", 3, Some(key('a')))),
            ]),
            observed: BTreeMap::from([
                (UnitName::new("network").unwrap(), observed(1, Some(key('a')))),
                (UnitName::new("web").unwrap(), observed_named("web", 3, Some(key('a')))),
            ]),
            ..EnvironmentSnapshot::default()
        };
        let lease = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Lease", "run": RUN, "runner": "ci",
            "operation": "reconcile", "units": [],
            "deadline": "2026-01-01T01:00:00Z", "takenAt": "2026-01-01T00:00:00Z",
            "broken": {"by": "alice", "at": "2026-01-01T00:10:00Z"}
        }))
        .unwrap();
        let run = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Run", "run": RUN, "runner": "ci",
            "operation": "reconcile", "deadline": "2026-01-01T01:00:00Z", "units": ["web"]
        }))
        .unwrap();
        let coordination = Coordination {
            lease: Some(HeldLease {
                lease,
                version: "1".to_owned(),
            }),
            runs: vec![RunState {
                run,
                checkpoints: Vec::new(),
                version: "r1".to_owned(),
            }],
        };
        let report = status(&EnvironmentName::new("dev").unwrap(), &snapshot, &coordination);

        let unit = |name: &str| &report.units[&UnitName::new(name).unwrap()];
        assert_eq!(unit("web").state, Some(UnitState::Uncertain));
        assert_eq!(unit("network").state, Some(UnitState::Current));
        assert!(
            report.warnings.iter().all(|warning| warning.code != Code::LeaseHeld),
            "{:?}",
            report.warnings
        );
        assert_eq!(report.errors[0].code, Code::LeaseLost);
        assert_eq!(report.exit, ExitCategory::Uncertain);

        // Once the broken run's ref says it finished, nothing was lost.
        let mut finished = coordination.clone();
        finished.runs[0].run.finished = true;
        let report = status(&EnvironmentName::new("dev").unwrap(), &snapshot, &finished);
        assert_eq!(
            report.units[&UnitName::new("web").unwrap()].state,
            Some(UnitState::Current)
        );
        assert!(report.errors.is_empty());
        assert_eq!(report.exit, ExitCategory::Ok);
    }
}
