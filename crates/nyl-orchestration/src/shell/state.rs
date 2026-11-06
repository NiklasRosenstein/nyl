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
                    source_commit: None,
                    source_ref: None,
                    keep_id: None,
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

/// Whether `nyl state init --template` starts a deleted instance over.
///
/// The decision is made under the lease, on the record the write reads, never
/// on one read before it: two jobs that saw the instance deleted cannot both
/// restart it and drop each other's documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// A deleted instance refuses (`NYL-STATE-DELETED`).
    Never,
    /// `--fresh`: start over whatever the record holds, dropping every unit's documents.
    Always,
    /// The pull request job: a record that is still deleted when the lease is
    /// held restarts, one that another job already restarted takes the normal
    /// update path.
    IfDeleted,
}

/// What `nyl state init --template` records for an instance.
#[derive(Clone)]
pub struct InstanceInit<'a> {
    /// The EnvironmentTemplate.
    pub template: &'a str,
    /// The instance's parameters, bound against the template.
    pub params: &'a BTreeMap<String, Value>,
    /// The entry worktree's commit: the source of the instance's later runs that are not entered from its own source.
    pub source_commit: &'a nyl_core::orchestration::ids::CommitId,
    /// `--source-ref`: where the host serves `source_commit` from when it will
    /// not serve it by its id, recorded next to it. `None` keeps what the
    /// record already holds; a hint only, validated by name and never fetched here.
    pub source_ref: Option<&'a str>,
    /// The template's `ttl`: activity moves `expiresAt` to now plus this.
    pub ttl: Option<nyl_core::orchestration::time::Duration>,
    /// Keep `source_commit` fetchable before it is recorded, through this
    /// source repository's keep refs; `None` unless the template sets `keepSource: true`, and for
    /// a run without network access.
    pub keep: Option<&'a dyn nyl_core::orchestration::ports::SourceResolver>,
}

/// `nyl state init -e <instance> --template <template> --param …`: create the
/// instance's `state.yaml`, or update the parameters, source commit, and
/// expiry of an existing one. Like a declared environment's initialization it
/// writes one transition commit and none when nothing changed, except that
/// activity always extends the expiry.
///
/// Contract: [Instances](../../../../design/orchestration-core.md#instances).
/// A deleted instance refuses (`NYL-STATE-DELETED`) until `--fresh` starts it
/// over, which drops every unit's documents like it does for a declared
/// environment, or the pull request job's [`Restart::IfDeleted`] does when the
/// record is still deleted under the lease.
#[allow(clippy::too_many_lines)] // One initialization: refusals, keep ref, record, renewal of the units, commit.
pub fn init_instance(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    instance: &InstanceInit<'_>,
    restart: Restart,
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
            report.details.insert("template".to_owned(), json!(instance.template));
            let snapshot = read(context, location, environment)?;
            let fresh = match restart {
                Restart::Never => false,
                Restart::Always => true,
                Restart::IfDeleted => snapshot
                    .record
                    .as_ref()
                    .is_some_and(|record| record.deleted().is_some()),
            };
            let now = held.lease.taken_at;
            let expires_at = crate::rules::expiry::extended(instance.ttl, now);
            let event = StateEvent::StateInitialized {
                at: now,
                by: context.requested_by.clone(),
                reason: requested.reason.clone(),
                fresh,
            };
            let mut changes = TransitionChanges::default();
            let headline;
            let keep_id: Option<nyl_core::orchestration::ids::Uid>;
            let record = match &snapshot.record {
                None => {
                    keep_id = keep_source(context, instance, environment, None)?;
                    headline = "instance created";
                    StateRecord {
                        api_version: StateV1,
                        kind: StateRecordKind,
                        environment: environment.clone(),
                        template: Some(instance.template.to_owned()),
                        params: instance.params.clone(),
                        expires_at,
                        source_commit: Some(instance.source_commit.clone()),
                        source_ref: instance.source_ref.map(str::to_owned),
                        keep_id: keep_id.clone(),
                        location: location.clone(),
                        events: vec![event],
                    }
                }
                Some(existing) => {
                    if fresh {
                        crate::rules::state_location::check_location(Some(existing), environment, location)?;
                    } else {
                        check_record(context.store, Some(existing), environment, location)?;
                    }
                    if existing.template.as_deref() != Some(instance.template) {
                        return Err(Finding::new(
                            Code::InstanceNameTaken,
                            format!(
                                "{environment} is {}, not an instance of EnvironmentTemplate {}",
                                existing.template.as_deref().map_or_else(
                                    || "a declared environment".to_owned(),
                                    |template| format!("an instance of EnvironmentTemplate {template}")
                                ),
                                instance.template
                            ),
                            "environment names are unique across declared Environments and instances; pick another name",
                        ));
                    }
                    check_remote_tips(context.store, location, environment)?;
                    // Past every refusal: the commit the record will name is kept before it is recorded.
                    // A restart is a new life, with keep refs of its own.
                    keep_id = keep_source(
                        context,
                        instance,
                        environment,
                        existing.keep_id.as_ref().filter(|_| !fresh),
                    )?;
                    let mut record = existing.clone();
                    record.keep_id.clone_from(&keep_id);
                    if fresh {
                        changes.desired = dropped(&snapshot.desired);
                        changes.observed = dropped(&snapshot.observed);
                        changes.artifacts = dropped(&snapshot.artifacts);
                        record.events.push(event);
                    } else {
                        // Activity renews: a removal recorded but not started is undone with the expiry.
                        changes.desired = crate::rules::expiry::renew_units(&snapshot.desired);
                    }
                    record.params = instance.params.clone();
                    record.source_commit = Some(instance.source_commit.clone());
                    if let Some(source_ref) = instance.source_ref {
                        record.source_ref = Some(source_ref.to_owned());
                    }
                    record.expires_at = expires_at;
                    if record == *existing && changes.desired.is_empty() {
                        report.expires_at = record.expires_at;
                        report
                            .details
                            .insert("note".to_owned(), json!("the instance is up to date"));
                        return Ok(());
                    }
                    headline = if fresh {
                        "instance restarted"
                    } else {
                        "instance updated"
                    };
                    record
                }
            };
            report.details.insert("params".to_owned(), json!(instance.params));
            report.expires_at = record.expires_at;
            let kept = keep_id.as_ref().map(crate::rules::keep::keep_prefix);
            changes.record = Some(record);
            let mut summary = summary(Operation::StateInit, environment, headline);
            summary.details.insert("template".to_owned(), json!(instance.template));
            summary.details.insert("params".to_owned(), json!(instance.params));
            commit(context, location, held, &snapshot, summary, changes, requested, report)?;
            if let Some(prefix) = kept {
                release_older_keep_refs(instance, &prefix, report);
            }
            Ok(())
        },
    )
}

/// The record names the new commit now, so the instance's other keep refs go.
fn release_older_keep_refs(instance: &InstanceInit<'_>, prefix: &str, report: &mut StateOperationReport) {
    let Some(sources) = instance.keep else {
        return;
    };
    match sources.release(prefix, Some(instance.source_commit)) {
        Ok(foreign) => report
            .warnings
            .extend(foreign.iter().map(|reference| foreign_keep_ref(reference))),
        Err(error) => report.warnings.push(keep_release_warning(prefix, &error.to_string())),
    }
}

/// Keep `instance`'s source commit fetchable before a record names it: a
/// ref of its own under the instance's keep id (`existing`, or one minted
/// from the injected generator for an instance that has none), so the commit
/// the record names now stays kept whatever happens next. Returns the keep id
/// to record; `None` when nothing is kept. `NYL-INSTANCE-KEEP-FAILED` when it
/// cannot be pushed.
fn keep_source(
    context: &Context<'_>,
    instance: &InstanceInit<'_>,
    environment: &EnvironmentName,
    existing: Option<&nyl_core::orchestration::ids::Uid>,
) -> Result<Option<nyl_core::orchestration::ids::Uid>, Finding> {
    let Some(sources) = instance.keep else {
        return Ok(existing.cloned());
    };
    let id = existing.cloned().unwrap_or_else(|| context.ids.uid());
    let reference = crate::rules::keep::keep_ref(&crate::rules::keep::keep_prefix(&id), instance.source_commit);
    sources.keep(&reference, instance.source_commit).map_err(|error| {
        Finding::new(
            Code::InstanceKeepFailed,
            format!(
                "cannot keep the source commit {} of {environment} fetchable through {reference}: {error}",
                instance.source_commit
            ),
            format!(
                "either grant this job push access to refs/nyl-keep/* in the source repository (a read-only token, such as GitHub's default `contents: read` or a fork pull request's, cannot), or remove keepSource: true from EnvironmentTemplate {} and tear instances down in the pull request closed job, before their commits become unreachable",
                instance.template
            ),
        )
    })?;
    Ok(Some(id))
}

/// The warning for a keep ref that does not name the commit its name says.
pub(crate) fn foreign_keep_ref(reference: &str) -> Finding {
    Finding::new(
        Code::InstanceKeepFailed,
        format!("the keep ref {reference} does not name the commit its name says, so Nyl left it alone"),
        "it is not this instance's; remove it by hand if nothing else uses it",
    )
}

/// The warning for keep refs that could not be removed.
pub(crate) fn keep_release_warning(prefix: &str, error: &str) -> Finding {
    Finding::new(
        Code::InstanceKeepFailed,
        format!("cannot remove the keep refs under {prefix}: {error}"),
        format!("remove them by hand, for example with `git push origin :<ref>` for each ref under {prefix}"),
    )
}

/// Activity on an instance: move its `expiresAt` to now plus `ttl` (cleared
/// for a template without one), the first step of every `reconcile -e
/// <instance>` that is not `--no-extend`. It is one transition commit, none
/// when the expiry already is that time, written whether or not the run that
/// follows succeeds.
///
/// Contract: [Expiry](../../../../design/orchestration-core.md#expiry).
pub fn renew_instance(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    ttl: Option<nyl_core::orchestration::time::Duration>,
    requested: &Requested,
) -> StateOperationReport {
    with_lease(
        context,
        environment,
        Operation::StateRenew,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            let mut record = snapshot.record.clone().expect("check_record found state.yaml");
            let expires_at = crate::rules::expiry::extended(ttl, held.lease.taken_at);
            if record.template.is_none() {
                return Err(Finding::new(
                    Code::InstanceOptionInvalid,
                    format!("{environment} is a declared environment, which has no expiry"),
                    "expiry applies to instances of an EnvironmentTemplate",
                ));
            }
            report.expires_at = expires_at;
            // A removal recorded but not started is undone in the same commit.
            let units = crate::rules::expiry::renew_units(&snapshot.desired);
            if record.expires_at == expires_at && units.is_empty() {
                report
                    .details
                    .insert("note".to_owned(), json!("the expiry is already that time"));
                return Ok(());
            }
            record.expires_at = expires_at;
            let mut summary = summary(Operation::StateRenew, environment, "expiry extended");
            if let Some(expires_at) = expires_at {
                summary.details.insert("expiresAt".to_owned(), json!(expires_at));
            }
            if !units.is_empty() {
                summary
                    .details
                    .insert("renewed".to_owned(), json!(units.keys().collect::<Vec<_>>()));
                report
                    .details
                    .insert("renewed".to_owned(), json!(units.keys().collect::<Vec<_>>()));
            }
            let changes = TransitionChanges {
                record: Some(record),
                desired: units,
                ..TransitionChanges::default()
            };
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
                drift: written(&source.drift),
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

/// `nyl state move --template <new> --from-template <old>`, for one
/// instance whose state stays where it is: record the instance as one of the
/// template `to`, because its template was renamed in source. One transition
/// commit, none when the instance already is `to`'s.
///
/// Contract: [Instances](../../../../design/orchestration-core.md#instances).
/// Moving the state of the instances of a template is
/// [`relocate`] for each; this is the part of the move that renames.
pub fn retemplate(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    to: &str,
    requested: &Requested,
) -> StateOperationReport {
    with_lease(
        context,
        environment,
        Operation::StateMove,
        location,
        requested,
        |held, report| {
            report.details.insert("location".to_owned(), json!(describe(location)));
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            let mut record = snapshot.record.clone().expect("check_record found state.yaml");
            let Some(from) = record.template.clone() else {
                return Err(Finding::new(
                    Code::InstanceNameTaken,
                    format!("{environment} is a declared environment, not an instance of a template"),
                    "only instances of an EnvironmentTemplate have a template to rename",
                ));
            };
            if from == to {
                report.details.insert(
                    "note".to_owned(),
                    json!(format!("the instance already is of template {to}")),
                );
                return Ok(());
            }
            record.template = Some(to.to_owned());
            report
                .details
                .insert("template".to_owned(), json!({"from": from, "to": to}));
            let mut summary = summary(Operation::StateMove, environment, "template renamed");
            summary
                .details
                .insert("template".to_owned(), json!({"from": from, "to": to}));
            let changes = TransitionChanges {
                record: Some(record),
                ..TransitionChanges::default()
            };
            commit(context, location, held, &snapshot, summary, changes, requested, report)
        },
    )
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
