//! `nyl state init`, `nyl state forget`, `nyl state move`, and `nyl lease break`.
//!
//! Contract: [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves),
//! [Deletion](../../../../design/orchestration-core.md#deletion) for forgetting,
//! and [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! for breaking a lease. Each takes the environment's lease and writes at
//! most one transition commit per state ref.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateError, Transition, TransitionChanges};
use nyl_core::orchestration::state::{
    Lifecycle, Operation, Request, StateEvent, StateLocation, StateRecord, StateRecordKind, StateV1, TeardownProgress,
};
use serde_json::{json, Value};

use super::lease::{self, HeldLease, LeaseError};
use super::Context;
use crate::report::{Outcome, StateOperationReport};
use crate::rules::state_location::{check_record, describe};
use crate::transition::{self, Trailers, TransitionSummary};

/// Who asked for an operator action and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requested {
    pub reason: Option<String>,
    /// `--wait-lease`: how long to wait for a held lease.
    pub wait: Option<nyl_core::orchestration::time::Duration>,
}

fn store_error(environment: &EnvironmentName, error: &StateError) -> Finding {
    match error {
        StateError::LeaseLost => lease::lost(environment),
        StateError::NotFastForward { reference } => Finding::new(
            Code::StateNotFastForward,
            format!("state ref {reference} of {environment} was rewritten outside Nyl"),
            "restore the ref to a descendant of what Nyl last wrote; Nyl never force-pushes state",
        ),
        other => Finding::new(
            Code::StateUnavailable,
            format!("state of {environment} is unavailable: {other}"),
            "check access to the state repository and retry",
        ),
    }
}

/// Run `body` holding the lease, and release it afterwards whatever happened.
fn with_lease(
    context: &Context<'_>,
    report: StateOperationReport,
    operation: Operation,
    location: &StateLocation,
    wait: Option<nyl_core::orchestration::time::Duration>,
    body: impl FnOnce(&mut HeldLease, StateOperationReport) -> StateOperationReport,
) -> StateOperationReport {
    let environment = report.environment.clone();
    let run = context.ids.run_id();
    let mut held = match lease::acquire(
        context,
        location,
        &environment,
        &run,
        operation,
        context.settings.resolution_timeout,
        wait,
    ) {
        Ok(held) => held,
        Err(LeaseError::Held(finding)) => return report.refused(finding),
        Err(LeaseError::Store(error)) => return report.refused(store_error(&environment, &error)),
    };
    let report = body(&mut held, report);
    if let Err(error) = lease::release(context, &held) {
        let mut report = report;
        report.warnings.push(store_error(&environment, &error));
        return report;
    }
    report
}

/// Write one transition and record it in the report. The fenced push moves
/// the lease, so `held` carries its new version afterwards.
#[allow(clippy::too_many_arguments)] // The transition, where it goes, and the lease fencing it.
fn commit(
    context: &Context<'_>,
    location: &StateLocation,
    held: &mut HeldLease,
    snapshot: &EnvironmentSnapshot,
    summary: &TransitionSummary,
    changes: TransitionChanges,
    reason: Option<&String>,
    mut report: StateOperationReport,
) -> StateOperationReport {
    let trailers = Trailers {
        run: held.lease.run.clone(),
        source_commit: None,
        read_desired: snapshot.tips.desired.clone(),
        read_observed: snapshot.tips.observed.clone(),
        runner: context.runner.clone(),
        evaluated_at: held.lease.taken_at,
        version: context.version.clone(),
        requested_by: Some(context.requested_by.clone()),
        reason: reason.cloned(),
        local: false,
    };
    let mut summary = summary.clone();
    summary.lease_broken.clone_from(&held.broken);
    let transition = Transition {
        message: transition::message(&summary, &trailers),
        changes,
    };
    match context
        .store
        .commit(location, &held.environment, &snapshot.tips, &transition, &held.version)
    {
        Ok(committed) => {
            held.version = committed.lease_version;
            if let Some(desired) = committed.desired {
                report.commits.insert("desired", desired);
            }
            if let Some(observed) = committed.observed {
                report.commits.insert("observed", observed);
            }
            if report.commits.is_empty() {
                report
            } else {
                report.done()
            }
        }
        Err(error) => {
            let environment = report.environment.clone();
            report.refused(store_error(&environment, &error))
        }
    }
}

fn summary(operation: Operation, environment: &EnvironmentName, headline: &str) -> TransitionSummary {
    TransitionSummary {
        operation,
        environment: environment.clone(),
        headline: headline.to_owned(),
        units: BTreeMap::new(),
        recovered: Vec::new(),
        imported: Vec::new(),
        lease_broken: None,
        details: BTreeMap::new(),
    }
}

/// `nyl state init -e <env> [--fresh]`: create state at the configured
/// location. Run again, it changes nothing; `--fresh` starts over, so every
/// unit becomes a new incarnation.
pub fn init(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    fresh: bool,
    requested: &Requested,
) -> StateOperationReport {
    let report = StateOperationReport::new(environment.clone(), Operation::StateInit.as_str());
    with_lease(
        context,
        report,
        Operation::StateInit,
        location,
        requested.wait,
        |held, report| {
            let snapshot = match context.store.snapshot(location) {
                Ok(snapshot) => snapshot,
                Err(error) => return report.refused(store_error(environment, &error)),
            };
            let now = held.lease.taken_at;
            let event = StateEvent::StateInitialized {
                at: now,
                by: context.requested_by.clone(),
                reason: requested.reason.clone(),
                fresh,
            };
            let mut changes = TransitionChanges::default();
            let record = match &snapshot.record {
                None => StateRecord {
                    api_version: StateV1,
                    kind: StateRecordKind,
                    environment: environment.clone(),
                    template: None,
                    params: BTreeMap::new(),
                    expires_at: None,
                    location: location.clone(),
                    events: vec![event],
                },
                Some(existing) => {
                    if let Err(finding) = check_record(Some(existing), environment, location) {
                        if finding.code != Code::StateMissing {
                            return report.refused(finding);
                        }
                    }
                    if !fresh {
                        let mut report = report;
                        report.outcome = Outcome::Unchanged;
                        report
                            .details
                            .insert("note".to_owned(), json!("state already exists at this location"));
                        return report;
                    }
                    // A fresh start drops every unit's documents.
                    changes.desired = snapshot.desired.keys().map(|unit| (unit.clone(), None)).collect();
                    changes.observed = snapshot.observed.keys().map(|unit| (unit.clone(), None)).collect();
                    changes.artifacts = snapshot.artifacts.keys().map(|unit| (unit.clone(), None)).collect();
                    let mut record = existing.clone();
                    record.events.push(event);
                    record
                }
            };
            changes.record = Some(record);
            let mut summary = summary(Operation::StateInit, environment, if fresh { "fresh" } else { "" });
            summary.details.insert("location".to_owned(), json!(describe(location)));
            let mut report = report;
            if fresh {
                report.details.insert(
                    "fresh".to_owned(),
                    json!("every unit becomes a new incarnation and executes again at the next reconcile"),
                );
            }
            commit(
                context,
                location,
                held,
                &snapshot,
                &summary,
                changes,
                requested.reason.as_ref(),
                report,
            )
        },
    )
}

/// `nyl state forget -e <env> --unit <u>`: drop a `retained` tombstone or a
/// `pending-teardown` unit from state without touching its resources. A
/// returning unit then starts a new incarnation.
pub fn forget(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    requested: &Requested,
) -> StateOperationReport {
    let report = StateOperationReport::new(environment.clone(), Operation::StateForget.as_str());
    with_lease(
        context,
        report,
        Operation::StateForget,
        location,
        requested.wait,
        |held, report| {
            let snapshot = match context.store.snapshot(location) {
                Ok(snapshot) => snapshot,
                Err(error) => return report.refused(store_error(environment, &error)),
            };
            if let Err(finding) = check_record(snapshot.record.as_ref(), environment, location) {
                return report.refused(finding);
            }
            let Some(desired) = snapshot.desired.get(unit) else {
                return report.refused(Finding::new(
                    Code::UnitUnknown,
                    format!("{environment} has no unit {unit} in its state"),
                    format!("list the units with `nyl get units -e {environment}`"),
                ));
            };
            let forgettable = match &desired.lifecycle {
                Lifecycle::Retained { .. } => true,
                Lifecycle::Deleting { deletion } => deletion.teardown == Some(TeardownProgress::Pending),
                Lifecycle::Active | Lifecycle::Held { .. } => false,
            };
            if !forgettable {
                return report.refused(Finding::new(
                Code::StateForgetRefused,
                format!("{unit} in {environment} is neither retained nor pending teardown"),
                format!(
                    "remove the unit from source, or tear it down with `nyl teardown -e {environment} --unit {unit}`; forgetting applies only to units whose resources Nyl would otherwise keep or destroy"
                ),
            ));
            }
            let changes = TransitionChanges {
                desired: BTreeMap::from([(unit.clone(), None)]),
                observed: BTreeMap::from([(unit.clone(), None)]),
                artifacts: BTreeMap::from([(unit.clone(), None)]),
                ..TransitionChanges::default()
            };
            let mut summary = summary(Operation::StateForget, environment, "1 forgotten");
            summary
                .units
                .insert(unit.clone(), json!({"result": "forgotten", "resourcesKept": true}));
            commit(
                context,
                location,
                held,
                &snapshot,
                &summary,
                changes,
                requested.reason.as_ref(),
                report,
            )
        },
    )
}

/// `nyl state move -e <env> --from …`: give the configured location the
/// state of `from`, then retire `from`. Units keep their uids and receipts.
///
/// History moves as refs where neither location shares its refs through
/// `path`, and as one commit of the documents otherwise. The new location
/// records the move as its tip last, so it is usable only once the old one
/// carries its marker; an interrupted move completes when run again. A move
/// from the clone's own branches leaves no marker there, because both
/// locations share one history.
pub fn relocate(
    context: &Context<'_>,
    environment: &EnvironmentName,
    to: &StateLocation,
    from: &StateLocation,
    requested: &Requested,
) -> StateOperationReport {
    let report = StateOperationReport::new(environment.clone(), Operation::StateMove.as_str());
    if from == to {
        return report.refused(Finding::new(
            Code::StateLocationMismatch,
            format!("the state of {environment} is already at {}", describe(to)),
            "pass the old location's options: --from, --desired-ref, --observed-ref, --path, --coordination-refs",
        ));
    }
    let same_coordination =
        from.repository == to.repository && from.coordination_ref_prefix == to.coordination_ref_prefix;
    with_lease(
        context,
        report,
        Operation::StateMove,
        to,
        requested.wait,
        |held_to, report| {
            // The lease at the old location too, unless both are one lease.
            let mut old_lease = if same_coordination {
                None
            } else {
                let run = context.ids.run_id();
                match lease::acquire(
                    context,
                    from,
                    environment,
                    &run,
                    Operation::StateMove,
                    context.settings.resolution_timeout,
                    requested.wait,
                ) {
                    Ok(held) => Some(held),
                    Err(LeaseError::Held(finding)) => return report.refused(finding),
                    Err(LeaseError::Store(error)) => return report.refused(store_error(environment, &error)),
                }
            };
            let report = move_holding(
                context,
                environment,
                to,
                from,
                held_to,
                old_lease.as_mut(),
                requested,
                report,
            );
            if let Some(old) = &old_lease {
                let _ = lease::release(context, old);
            }
            report
        },
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One move, its three steps in order: both locations, both leases, and the request.
fn move_holding(
    context: &Context<'_>,
    environment: &EnvironmentName,
    to: &StateLocation,
    from: &StateLocation,
    held_to: &mut HeldLease,
    held_from: Option<&mut HeldLease>,
    requested: &Requested,
    mut report: StateOperationReport,
) -> StateOperationReport {
    let target = match context.store.snapshot(to) {
        Ok(snapshot) => snapshot,
        Err(error) => return report.refused(store_error(environment, &error)),
    };
    // Already moved: the new location records the move from `from`.
    if let Some(record) = &target.record {
        if record.location == *to
            && matches!(record.events.last(), Some(StateEvent::StateMoved { from: moved_from, .. }) if moved_from.as_ref() == from)
        {
            report.outcome = Outcome::Unchanged;
            report
                .details
                .insert("note".to_owned(), json!("the move already completed"));
            return report;
        }
    }
    let source = match context.store.snapshot(from) {
        Ok(snapshot) => snapshot,
        Err(error) => return report.refused(store_error(environment, &error)),
    };
    let Some(source_record) = source.record.clone() else {
        return report.refused(Finding::new(
            Code::StateMissing,
            format!("{environment} has no state at {}", describe(from)),
            "name the location the state is at with --from and the ref options",
        ));
    };
    if source_record.environment != *environment {
        return report.refused(Finding::new(
            Code::StateEnvironmentMismatch,
            format!(
                "the state at {} belongs to {}",
                describe(from),
                source_record.environment
            ),
            "name this environment's old location",
        ));
    }
    let already_marked = matches!(source_record.events.last(), Some(StateEvent::StateMoved { to: moved_to, .. }) if moved_to.as_ref() == to);
    let only_prefix = from.repository == to.repository
        && from.desired_ref == to.desired_ref
        && from.observed_ref == to.observed_ref
        && from.path == to.path;
    if only_prefix {
        match context.store.coordination(from, environment) {
            Ok(coordination) if !coordination.runs.is_empty() => {
                return report.refused(Finding::new(
                    Code::LeaseHeld,
                    format!("{environment} has run refs at {}", from.coordination_ref_prefix),
                    "let the next run at the old prefix import them, then move the coordination refs",
                ))
            }
            Ok(_) => {}
            Err(error) => return report.refused(store_error(environment, &error)),
        }
    }
    let event = StateEvent::StateMoved {
        at: held_to.lease.taken_at,
        by: context.requested_by.clone(),
        reason: requested.reason.clone(),
        from: Box::new(from.clone()),
        to: Box::new(to.clone()),
    };
    let mut summary = summary(Operation::StateMove, environment, "");
    summary.details.insert("from".to_owned(), json!(describe(from)));
    summary.details.insert("to".to_owned(), json!(describe(to)));
    report.details.insert("from".to_owned(), json!(describe(from)));
    report.details.insert("to".to_owned(), json!(describe(to)));

    if !only_prefix && !already_marked {
        // 1. The history, or the documents, at the new location.
        if from.path.is_empty() && to.path.is_empty() {
            if let Err(error) = context.store.import_history(from, to) {
                return report.refused(Finding::new(
                    Code::StateLocationMismatch,
                    format!("cannot move the state of {environment} to {}: {error}", describe(to)),
                    "the target refs must not exist yet, or hold exactly the history of the old location",
                ));
            }
        } else if target.record.is_none() {
            let changes = TransitionChanges {
                record: Some(source_record.clone()),
                environment: source.environment.clone(),
                desired: source
                    .desired
                    .iter()
                    .map(|(unit, document)| (unit.clone(), Some(document.clone())))
                    .collect(),
                observed: source
                    .observed
                    .iter()
                    .map(|(unit, document)| (unit.clone(), Some(document.clone())))
                    .collect(),
                artifacts: source
                    .artifacts
                    .iter()
                    .map(|(unit, set)| (unit.clone(), Some(set.clone())))
                    .collect(),
            };
            let copied = commit(
                context,
                to,
                held_to,
                &target,
                &summary,
                changes,
                requested.reason.as_ref(),
                report.clone(),
            );
            if copied.outcome == Outcome::Refused {
                return copied;
            }
        }
        // 2. The marker at the old location, unless both share one history.
        if !from.is_local() {
            // With one coordination prefix, one lease fences both locations.
            let held_from = match held_from {
                Some(held) => held,
                None => &mut *held_to,
            };
            let mut marked_record = source_record.clone();
            marked_record.events.push(event.clone());
            let old_marker = commit(
                context,
                from,
                held_from,
                &source,
                &summary,
                TransitionChanges {
                    record: Some(marked_record),
                    ..TransitionChanges::default()
                },
                requested.reason.as_ref(),
                StateOperationReport::new(environment.clone(), Operation::StateMove.as_str()),
            );
            if old_marker.outcome == Outcome::Refused {
                let mut report = report;
                report.errors.extend(old_marker.errors);
                report.exit = old_marker.exit;
                report.outcome = Outcome::Refused;
                return report;
            }
        }
    }
    // 3. The new location records itself and the move, as its tip.
    let target = match context.store.snapshot(to) {
        Ok(snapshot) => snapshot,
        Err(error) => return report.refused(store_error(environment, &error)),
    };
    let mut record = target.record.clone().unwrap_or(source_record);
    record.location = to.clone();
    record.events.push(event);
    let changes = TransitionChanges {
        record: Some(record),
        ..TransitionChanges::default()
    };
    commit(
        context,
        to,
        held_to,
        &target,
        &summary,
        changes,
        requested.reason.as_ref(),
        report,
    )
}

/// `nyl lease break -e <env> --reason <text>`: declare the lease holder gone.
/// The lease is replaced by a broken one, so the next run takes over at once
/// and records who broke it and why. A holder that is in fact alive may still
/// cause effects; the fenced final push keeps it from writing state.
pub fn break_lease(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    reason: Option<String>,
) -> StateOperationReport {
    let mut report = StateOperationReport::new(environment.clone(), "lease-break");
    let coordination = match context.store.coordination(location, environment) {
        Ok(coordination) => coordination,
        Err(error) => return report.refused(store_error(environment, &error)),
    };
    let Some(held) = coordination.lease else {
        report
            .details
            .insert("note".to_owned(), json!(format!("{environment} has no lease to break")));
        return report;
    };
    let mut lease = held.lease.clone();
    lease.broken = Some(Request {
        by: context.requested_by.clone(),
        at: context.clock.now(),
        reason,
    });
    match context
        .store
        .take_lease(location, environment, &lease, Some(&held.version))
    {
        Ok(_) => {
            report
                .details
                .insert("broken".to_owned(), Value::String(format!("run {}", held.lease.run)));
            report.warnings.push(Finding::new(
                Code::LeaseHeld,
                format!(
                    "the lease of run {} by {} was broken; if that run is still alive it may still cause effects, but it can no longer write state",
                    held.lease.run, held.lease.runner
                ),
                "make sure the runner is gone; units it was executing become uncertain at the next run",
            ));
            report.done()
        }
        Err(StateError::LeaseRace) => report.refused(Finding::new(
            Code::LeaseHeld,
            format!("the lease of {environment} changed while it was being broken"),
            "check `nyl status` and run `nyl lease break` again if the holder is still gone",
        )),
        Err(error) => report.refused(store_error(environment, &error)),
    }
}
