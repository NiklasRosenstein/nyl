//! `nyl state init`, `forget`, `move`, `reset`, and `restore`, and `nyl lease break`.
//!
//! Contract: [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves),
//! [Deletion](../../../../design/orchestration-core.md#deletion) for forgetting,
//! and [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! for breaking a lease. Each takes the environment's lease and writes at
//! most one transition commit per state ref.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateError, StateSide, TransitionChanges};
use nyl_core::orchestration::state::{
    Lease, LeaseKind, Lifecycle, Operation, Request, StateEvent, StateLocation, StateRecord, StateRecordKind, StateV1,
    TeardownProgress,
};
use serde_json::{json, Value};

use super::lease::{self, HeldLease};
use super::local::{check_record, check_remote_tips, sides, TIP_HISTORY};
use super::{store_error, Context};
use crate::report::{Outcome, StateOperationReport};
use crate::rules::lease::{self as lease_rule, Holder, LeaseDecision};
use crate::rules::local as local_rule;
use crate::rules::state_location::describe;
use crate::transition::{Trailers, TransitionSummary};

/// Who asked for an operator action and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requested {
    pub reason: Option<String>,
    /// `--wait-lease`: how long to wait for a held lease.
    pub wait: Option<nyl_core::orchestration::time::Duration>,
    /// Set on SIGINT or SIGTERM: stops waiting for a held lease.
    pub cancellation: super::run::Cancellation,
}

/// Run `body` on `report`, refusing it with the finding `body` fails with.
fn refusing(
    mut report: StateOperationReport,
    body: impl FnOnce(&mut StateOperationReport) -> Result<(), Finding>,
) -> StateOperationReport {
    match body(&mut report) {
        Ok(()) => report,
        Err(finding) => report.refused(finding),
    }
}

/// Take the lease for `operation`, as [`lease::acquire`] does for every
/// operator action: with the resolution timeout and the request's wait.
fn acquire(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    operation: Operation,
    requested: &Requested,
) -> Result<HeldLease, Finding> {
    lease::acquire(
        context,
        location,
        environment,
        &context.ids.run_id(),
        operation,
        // State operations and `recover` without an import never take a
        // broken lease over ("Runs and leases" item 1).
        lease_rule::BrokenLease::Refuse,
        context.settings.resolution_timeout,
        requested.wait,
        &requested.cancellation,
    )
}

/// Run `body` holding the lease for `operation`, and release it afterwards
/// whatever happened; a finding `body` fails with refuses the operation.
pub(crate) fn with_lease(
    context: &Context<'_>,
    environment: &EnvironmentName,
    operation: Operation,
    location: &StateLocation,
    requested: &Requested,
    body: impl FnOnce(&mut HeldLease, &mut StateOperationReport) -> Result<(), Finding>,
) -> StateOperationReport {
    let report = StateOperationReport::new(environment.clone(), operation.as_str());
    let mut held = match acquire(context, location, environment, operation, requested) {
        Ok(held) => held,
        Err(finding) => return report.refused(finding),
    };
    let mut report = refusing(report, |report| body(&mut held, report));
    if let Err(error) = lease::release(context, &held, true) {
        report.warnings.push(store_error(environment, &error));
    }
    report
}

/// Write one transition with the request's trailers and record its commits
/// in the report, which is done once any commit was written. The fenced
/// push moves the lease, so `held` carries its new version afterwards.
#[allow(clippy::too_many_arguments)] // The transition, where it goes, the lease fencing it, and the request.
pub(crate) fn commit(
    context: &Context<'_>,
    location: &StateLocation,
    held: &mut HeldLease,
    snapshot: &EnvironmentSnapshot,
    summary: TransitionSummary,
    changes: TransitionChanges,
    requested: &Requested,
    report: &mut StateOperationReport,
) -> Result<(), Finding> {
    let trailers = Trailers {
        requested_by: Some(context.requested_by.clone()),
        reason: requested.reason.clone(),
        ..super::trailers(context, held, &snapshot.tips)
    };
    let commits = super::commit_transition(context, location, held, &snapshot.tips, summary, changes, &trailers)
        .map_err(|error| store_error(&report.environment, &error))?;
    report.commits.extend(commits);
    if !report.commits.is_empty() {
        report.outcome = Outcome::Done;
    }
    Ok(())
}

/// The snapshot of `location`, or the finding for the store failure.
pub(crate) fn read(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
) -> Result<EnvironmentSnapshot, Finding> {
    context
        .store
        .snapshot(location)
        .map_err(|error| store_error(environment, &error))
}

/// Every key of `map`, mapped to `None`: the change dropping each document.
fn dropped<V>(map: &BTreeMap<UnitName, V>) -> BTreeMap<UnitName, Option<V>> {
    map.keys().map(|unit| (unit.clone(), None)).collect()
}

/// Every document of `map`, as changes writing it.
fn written<V: Clone>(map: &BTreeMap<UnitName, V>) -> BTreeMap<UnitName, Option<V>> {
    map.iter()
        .map(|(unit, document)| (unit.clone(), Some(document.clone())))
        .collect()
}

/// Append the commits of `more` that `into` does not list yet.
fn extend_unique<T: PartialEq>(into: &mut Vec<T>, more: Vec<T>) {
    for item in more {
        if !into.contains(&item) {
            into.push(item);
        }
    }
}

pub(crate) fn summary(operation: Operation, environment: &EnvironmentName, headline: &str) -> TransitionSummary {
    TransitionSummary {
        operation,
        environment: environment.clone(),
        headline: headline.to_owned(),
        units: BTreeMap::new(),
        recovered: Vec::new(),
        imported: Vec::new(),
        lease_broken: None,
        drift_repaired: Vec::new(),
        reapplied: None,
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
    if let Err(error) = context.store.prepare_init(location) {
        return StateOperationReport::new(environment.clone(), Operation::StateInit.as_str())
            .refused(store_error(environment, &error));
    }
    with_lease(
        context,
        environment,
        Operation::StateInit,
        location,
        requested,
        |held, report| {
            report.details.insert("location".to_owned(), json!(describe(location)));
            let snapshot = read(context, location, environment)?;
            let event = StateEvent::StateInitialized {
                at: held.lease.taken_at,
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
                    if fresh {
                        // `--fresh` starts a deleted location over (Q15).
                        crate::rules::state_location::check_location(Some(existing), environment, location)?;
                    } else {
                        // A plain init refuses a deleted location (`NYL-STATE-DELETED`).
                        check_record(context.store, Some(existing), environment, location)?;
                    }
                    if !fresh {
                        report
                            .details
                            .insert("note".to_owned(), json!("state already exists at this location"));
                        return Ok(());
                    }
                    check_remote_tips(context.store, location, environment)?;
                    // A fresh start drops every unit's documents.
                    changes.desired = dropped(&snapshot.desired);
                    changes.observed = dropped(&snapshot.observed);
                    changes.artifacts = dropped(&snapshot.artifacts);
                    let mut record = existing.clone();
                    record.events.push(event);
                    record
                }
            };
            changes.record = Some(record);
            let mut summary = summary(Operation::StateInit, environment, if fresh { "fresh" } else { "" });
            summary.details.insert("location".to_owned(), json!(describe(location)));
            if fresh {
                report.details.insert(
                    "fresh".to_owned(),
                    json!("every unit becomes a new incarnation and executes again at the next reconcile"),
                );
            }
            commit(context, location, held, &snapshot, summary, changes, requested, report)
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
    with_lease(
        context,
        environment,
        Operation::StateForget,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            let Some(desired) = snapshot.desired.get(unit) else {
                return Err(Finding::new(
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
                return Err(Finding::new(
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
            commit(context, location, held, &snapshot, summary, changes, requested, report)
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
    if from == to {
        return StateOperationReport::new(environment.clone(), Operation::StateMove.as_str()).refused(Finding::new(
            Code::StateLocationMismatch,
            format!("the state of {environment} is already at {}", describe(to)),
            "pass the old location's options: --from, --desired-ref, --observed-ref, --path, --coordination-refs",
        ));
    }
    let same_coordination =
        from.repository == to.repository && from.coordination_ref_prefix == to.coordination_ref_prefix;
    with_lease(
        context,
        environment,
        Operation::StateMove,
        to,
        requested,
        |held_to, report| {
            // The lease at the old location too, unless both are one lease.
            let mut old_lease = if same_coordination {
                None
            } else {
                Some(acquire(context, from, environment, Operation::StateMove, requested)?)
            };
            let moved = move_holding(
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
                let _ = lease::release(context, old, true);
            }
            moved
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
    report: &mut StateOperationReport,
) -> Result<(), Finding> {
    let target = read(context, to, environment)?;
    // Already moved: the new location records the move from `from`.
    if let Some(record) = &target.record {
        if record.location == *to
            && matches!(record.events.last(), Some(StateEvent::StateMoved { from: moved_from, .. }) if moved_from.as_ref() == from)
        {
            report
                .details
                .insert("note".to_owned(), json!("the move already completed"));
            return Ok(());
        }
    }
    let source = read(context, from, environment)?;
    // Neither location may carry pushed local transitions at its tip.
    check_remote_tips(context.store, from, environment)?;
    if target.record.is_some() {
        check_remote_tips(context.store, to, environment)?;
    }
    let Some(source_record) = source.record.clone() else {
        return Err(Finding::new(
            Code::StateMissing,
            format!("{environment} has no state at {}", describe(from)),
            "name the location the state is at with --from and the ref options",
        ));
    };
    if source_record.environment != *environment {
        return Err(Finding::new(
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
        let coordination = context
            .store
            .coordination(from, environment)
            .map_err(|error| store_error(environment, &error))?;
        if !coordination.runs.is_empty() {
            return Err(Finding::new(
                Code::LeaseHeld,
                format!("{environment} has run refs at {}", from.coordination_ref_prefix),
                "let the next run at the old prefix import them, then move the coordination refs",
            ));
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

    // A copy from an interrupted move is the source record itself; any other
    // record at the new location is someone else's state.
    if !already_marked && target.record.as_ref().is_some_and(|record| *record != source_record) {
        return Err(Finding::new(
            Code::StateLocationMismatch,
            format!("{} already holds other state", describe(to)),
            "point the Environment at a location without state, or remove that state first",
        ));
    }
    if !only_prefix && !already_marked {
        // 1. The history, or the documents, at the new location. History
        // carries over only when both locations split desired and observed
        // state across refs alike.
        let same_layout = (from.desired_ref == from.observed_ref) == (to.desired_ref == to.observed_ref);
        if from.path.is_empty() && to.path.is_empty() && same_layout {
            if let Err(error) = context.store.import_history(from, to) {
                return Err(Finding::new(
                    Code::StateLocationMismatch,
                    format!("cannot move the state of {environment} to {}: {error}", describe(to)),
                    "the target refs must not exist yet, or hold exactly the history of the old location",
                ));
            }
        } else if target.record.is_none() {
            let changes = TransitionChanges {
                record: Some(source_record.clone()),
                environment: source.environment.clone(),
                desired: written(&source.desired),
                observed: written(&source.observed),
                artifacts: written(&source.artifacts),
                observations: written(&source.observations),
                attestations: written(&source.attestations),
                ..TransitionChanges::default()
            };
            // The copy's commits are not the move's: step 3 writes its tip.
            commit(
                context,
                to,
                held_to,
                &target,
                summary.clone(),
                changes,
                requested,
                &mut report.clone(),
            )?;
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
            let changes = TransitionChanges {
                record: Some(marked_record),
                ..TransitionChanges::default()
            };
            let mut marker = StateOperationReport::new(environment.clone(), Operation::StateMove.as_str());
            commit(
                context,
                from,
                held_from,
                &source,
                summary.clone(),
                changes,
                requested,
                &mut marker,
            )?;
        }
    }
    // 3. The new location records itself and the move, as its tip.
    let target = read(context, to, environment)?;
    let mut record = target.record.clone().unwrap_or(source_record);
    record.location = to.clone();
    record.events.push(event);
    let changes = TransitionChanges {
        record: Some(record),
        ..TransitionChanges::default()
    };
    commit(context, to, held_to, &target, summary, changes, requested, report)?;
    // A move from the clone's own branches leaves them following the
    // remote: reading it fast-forwards them to the move's tip.
    if from.is_local() && report.outcome == Outcome::Done {
        if let Err(error) = context.store.snapshot(to) {
            report.warnings.push(store_error(environment, &error));
        }
    }
    Ok(())
}

/// `nyl lease break -e <env> --reason <text>`: declare the lease holder gone.
/// The lease is replaced by a broken one, so the next importer (`reconcile`,
/// `teardown`, or `recover -e`) takes over at once, imports its run, and
/// records who broke it and why; every other state writer refuses with
/// `NYL-LEASE-BROKEN` until then. A lease that is already broken is
/// reported as such. A holder that is in fact alive may still
/// cause effects; the fenced final push keeps it from writing state.
pub fn break_lease(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    reason: Option<String>,
) -> StateOperationReport {
    refusing(
        StateOperationReport::new(environment.clone(), "lease-break"),
        |report| {
            let coordination = context
                .store
                .coordination(location, environment)
                .map_err(|error| store_error(environment, &error))?;
            let now = context.clock.now();
            // Break whatever holds the environment: its lease, or a live run whose
            // lease ref is already gone.
            // As every non-importer: a lease that is already broken, its run
            // not imported yet, is reported as such ("Runs and leases" item 1).
            let mut lease = match lease_rule::decide(&coordination, now, lease_rule::BrokenLease::Refuse) {
                LeaseDecision::Take { .. } => {
                    report.details.insert(
                        "note".to_owned(),
                        json!(format!("nothing holds the lease of {environment}")),
                    );
                    return Ok(());
                }
                LeaseDecision::Broken(broken) => {
                    report.details.insert(
                        "note".to_owned(),
                        json!(format!(
                            "the lease of {environment} is already broken (run {} by {}); only an importer takes it over: `nyl reconcile`, `nyl teardown`, or `nyl recover -e {environment}`",
                            broken.run, broken.runner
                        )),
                    );
                    return Ok(());
                }
                LeaseDecision::Held(Holder::Lease(lease)) => lease,
                LeaseDecision::Held(Holder::Run(run)) => Lease {
                    api_version: StateV1,
                    kind: LeaseKind,
                    run: run.run,
                    runner: run.runner,
                    operation: run.operation,
                    units: run.units,
                    deadline: run.deadline,
                    taken_at: now,
                    broken: None,
                },
            };
            lease.broken = Some(Request {
                by: context.requested_by.clone(),
                at: now,
                reason,
            });
            let expected = coordination.lease.as_ref().map(|held| held.version.as_str());
            match context.store.take_lease(location, environment, &lease, expected) {
                Ok(_) => {
                    report
                        .details
                        .insert("broken".to_owned(), Value::String(format!("run {}", lease.run)));
                    report.warnings.push(Finding::new(
                    Code::LeaseHeld,
                    format!(
                        "the lease of run {} by {} was broken; if that run is still alive it may still cause effects, but it can no longer write state",
                        lease.run, lease.runner
                    ),
                    format!(
                        "make sure the runner is gone; only an importer takes the break over (`nyl reconcile`, `nyl teardown`, or `nyl recover -e {environment}`), which imports its run and makes units it was executing uncertain; every other state writer refuses with NYL-LEASE-BROKEN until then"
                    ),
                ));
                    report.outcome = Outcome::Done;
                    Ok(())
                }
                Err(StateError::LeaseRace) => Err(Finding::new(
                    Code::LeaseHeld,
                    format!("the lease of {environment} changed while it was being broken"),
                    "check `nyl status` and run `nyl lease break` again if the holder is still gone",
                )),
                Err(error) => Err(store_error(environment, &error)),
            }
        },
    )
}

/// `nyl state reset -e <env> --local`: drop the clone's local transitions
/// and follow the remote state again. Lists the units whose local receipts
/// the remote does not reflect and proceeds only when `discard` covers them;
/// writes only the clone's local branches, through
/// [`StateStore::reset_local`](nyl_core::orchestration::ports::StateStore::reset_local),
/// and takes no lease. Refused for state without a remote
/// (`NYL-STATE-RESET-UNSUPPORTED`).
///
/// Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
pub fn reset(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    discard: Option<&local_rule::Discard>,
) -> StateOperationReport {
    refusing(
        StateOperationReport::new(environment.clone(), "state-reset"),
        |report| {
            if location.is_local() {
                return Err(Finding::new(
                Code::StateResetUnsupported,
                format!(
                    "the state of {environment} lives on this clone's own branches, so there is no remote state to follow"
                ),
                format!(
                    "configure a state repository and move the state there with `nyl state move -e {environment} --from local`"
                ),
            ));
            }
            let view = local_rule::view(location)?;
            // The remote first: it fetches, and fast-forwards branches that follow it.
            let remote = read(context, location, environment)?;
            let local = read(context, &view, environment)?;
            if local.tips == remote.tips {
                report
                    .details
                    .insert("note".to_owned(), json!("the local state branches follow the remote"));
                return Ok(());
            }
            let unreflected = local_rule::unreflected(&local, &remote);
            local_rule::check_discard(environment, &unreflected, discard)?;
            let mut dropped = Vec::new();
            for side in sides(location) {
                match context.store.log(&view, *side, TIP_HISTORY) {
                    Ok(log) => {
                        let remote_tip = match side {
                            StateSide::Desired => remote.tips.desired.as_ref(),
                            StateSide::Observed => remote.tips.observed.as_ref(),
                        };
                        extend_unique(
                            &mut dropped,
                            log.into_iter()
                                .take_while(|entry| Some(&entry.commit) != remote_tip)
                                .map(|entry| entry.commit)
                                .collect(),
                        );
                    }
                    // Without history the dropped commits are not listed.
                    Err(StateError::Unavailable(_)) => {}
                    Err(error) => return Err(store_error(environment, &error)),
                }
            }
            context
                .store
                .reset_local(location, &local.tips)
                .map_err(|error| store_error(environment, &error))?;
            report.details.insert("discarded".to_owned(), json!(unreflected));
            report.details.insert("dropped".to_owned(), json!(dropped));
            report.outcome = Outcome::Done;
            Ok(())
        },
    )
}

/// How far back `state restore` looks for the commit it restores.
const RESTORE_HISTORY: usize = 1000;

/// `nyl state restore -e <env> --to <commit>`: one desired and one observed
/// commit (operation `state-restore`, event `state-restored`) restoring the
/// content of `to`, on top of the current tips, so protected branches need
/// no force push; preserves current admission. Takes the lease. The other ref's commit is paired
/// through the transition trailers ([`local_rule::restore_tips`]).
///
/// Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
pub fn restore(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    to: &nyl_core::orchestration::ids::CommitId,
    requested: &Requested,
) -> StateOperationReport {
    with_lease(
        context,
        environment,
        Operation::StateRestore,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            let shared = location.desired_ref == location.observed_ref;
            let mut logs = Vec::new();
            for side in sides(location) {
                let log = context
                    .store
                    .log(location, *side, RESTORE_HISTORY)
                    .map_err(|error| store_error(environment, &error))?;
                logs.push(log);
            }
            let (desired_log, observed_log) = (&logs[0], logs.last().expect("one log per side"));
            let tips = local_rule::restore_tips(environment, to, desired_log, observed_log, shared)?;
            let target = context
                .store
                .snapshot_at(location, &tips)
                .map_err(|error| store_error(environment, &error))?;
            let mut changes = local_rule::restore_changes(&snapshot, &target);
            if changes.is_empty() {
                report.details.insert(
                    "note".to_owned(),
                    json!(format!("the state already holds the content of {to}")),
                );
                return Ok(());
            }
            let mut record = snapshot.record.clone().expect("check_record found state.yaml");
            record.events.push(StateEvent::StateRestored {
                at: held.lease.taken_at,
                by: context.requested_by.clone(),
                reason: requested.reason.clone(),
                to: to.clone(),
            });
            changes.record = Some(record);
            let mut summary = summary(Operation::StateRestore, environment, "restored");
            summary.details.insert("to".to_owned(), json!(to));
            report.details.insert("to".to_owned(), json!(to));
            commit(context, location, held, &snapshot, summary, changes, requested, report)
        },
    )
}
