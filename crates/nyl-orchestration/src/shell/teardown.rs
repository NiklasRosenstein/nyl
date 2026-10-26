//! `nyl teardown -e <env> (--unit <u>… | --all) [--hold] [--confirm-removed]`
//! and `nyl state delete -e <env> [--teardown]`.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Explicit teardown" and holds, [Instances](../../../../design/orchestration-core.md#instances)
//! (`teardown --all`, `state delete`, decommissioning a declared
//! environment), [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (`--confirm-removed`, the signal path), and [Commands](../../../../design/orchestration-core.md#commands).
//!
//! `teardown` runs on the shared run path ([`super::run`]) with
//! [`RunMode::Teardown`](super::run::RunMode::Teardown): it takes the lease
//! (and takes a broken lease over, importing its run), acts on the stored
//! desired documents, writes one transition commit per state ref with
//! operation `teardown`, and reports a `TeardownReport`. A held unit is not
//! torn down: the run records the request on its held file and reports it
//! held with a pending teardown (exit 0); after `nyl resume` the next
//! teardown-capable run carries it out. `--all` and `state delete
//! --teardown` honour `deletionPolicy: Retain`: Retain units and retained
//! tombstones are released and listed in the commit.
//!
//! `state delete` refuses while a unit has an incarnation
//! (`NYL-STATE-DELETE-INCARNATIONS`) and otherwise writes one commit per
//! state ref removing the environment's other state files
//! ([`TransitionChanges::clear`](nyl_core::orchestration::ports::TransitionChanges::clear))
//! and leaving `state.yaml` with a `state-deleted` event as the deleted
//! marker; with `--teardown` it runs `teardown --all` first, in the same
//! command. A plain `nyl state init` on a deleted location refuses with
//! `NYL-STATE-DELETED`, read from the marker.

use std::collections::BTreeSet;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, TransitionChanges};
use nyl_core::orchestration::state::{Lifecycle, Operation, SignalAction, StateEvent};
use serde_json::json;

use super::local::{check_record, check_remote_tips};
use super::run::{RunContext, RunMode, RunRequest, TeardownTarget};
use super::signal::{deliver, Delivery};
use super::state::{commit, read, summary, with_lease, Requested};
use super::{store_error, Context};
use crate::report::{Outcome, StateOperationReport};
use crate::rules::lease::{self as lease_rule, BrokenLease, LeaseDecision};
use crate::run_report::{EnvironmentRunReport, ExitCategory, ReportKind, RunReport};

/// A teardown report of `environment` that holds only `errors` and `warnings`.
fn teardown_report(environment: &EnvironmentName, errors: Vec<Finding>, warnings: Vec<Finding>) -> RunReport {
    let mut environment = EnvironmentRunReport::new(environment.clone());
    environment.errors = errors;
    environment.warnings = warnings;
    environment.finish(ReportKind::TeardownReport);
    let mut report = RunReport::new(ReportKind::TeardownReport);
    report.environments.push(environment);
    report
}

/// The units a teardown request names that state does not hold.
fn unknown_units(
    environment: &EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    named: &BTreeSet<UnitName>,
) -> Vec<Finding> {
    named
        .iter()
        .filter(|unit| !snapshot.desired.contains_key(*unit))
        .map(|unit| {
            Finding::new(
                Code::UnitUnknown,
                format!("{environment} has no unit {unit} in its state"),
                format!("list the units with `nyl get units -e {environment}`; a unit that never ran has nothing to tear down"),
            )
        })
        .collect()
}

/// Whether a run holds the lease of the request's environment (a live lease
/// or run ref): a signal goes to it instead of a run of this invocation.
fn run_holds_lease(context: &Context<'_>, request: &RunRequest) -> Result<bool, Finding> {
    let coordination = context
        .store
        .coordination(&request.location, &request.environment)
        .map_err(|error| store_error(&request.environment, &error))?;
    Ok(matches!(
        lease_rule::decide(&coordination, context.clock.now(), BrokenLease::Refuse),
        LeaseDecision::Held(_)
    ))
}

/// `nyl teardown`: `request.mode` is [`RunMode::Teardown`](super::run::RunMode::Teardown).
pub fn teardown(context: &RunContext<'_>, request: &RunRequest) -> RunReport {
    let environment = &request.environment;
    let base = context.base;
    let snapshot = match read(base, &request.location, environment) {
        Ok(snapshot) => snapshot,
        Err(finding) => return teardown_report(environment, vec![finding], Vec::new()),
    };
    if let Err(finding) = check_record(base.store, snapshot.record.as_ref(), environment, &request.location) {
        return teardown_report(environment, vec![finding], Vec::new());
    }
    let target = match &request.mode {
        RunMode::Teardown { target } => target.clone(),
        _ => TeardownTarget::All,
    };
    if let TeardownTarget::Units(named) = &target {
        let unknown = unknown_units(environment, &snapshot, named);
        if !unknown.is_empty() {
            return teardown_report(environment, unknown, Vec::new());
        }
    }
    if request.lifecycle.confirm_removed {
        // The units whose removal is confirmed: the named ones, or with
        // `--all` every unit whose teardown waits.
        let confirmed: Vec<UnitName> = match &target {
            TeardownTarget::Units(named) => named.iter().cloned().collect(),
            TeardownTarget::All => snapshot
                .observed
                .iter()
                .filter(|(_, observed)| observed.waiting.is_some())
                .map(|(unit, _)| unit.clone())
                .collect(),
        };
        let running = match run_holds_lease(base, request) {
            Ok(running) => running,
            Err(finding) => return teardown_report(environment, vec![finding], Vec::new()),
        };
        if running {
            let mut delivered = Vec::new();
            for unit in &confirmed {
                match deliver(
                    base,
                    &request.location,
                    environment,
                    unit,
                    SignalAction::ConfirmRemoved,
                    request.reason.as_deref(),
                ) {
                    Ok(Delivery::Direct) => {}
                    Ok(Delivery::Delivered { run, runner }) => delivered.push(Finding::new(
                        Code::ConfirmationDelivered,
                        format!(
                            "the removal of {unit} was confirmed to run {run} by {runner}, which waits on its teardown"
                        ),
                        format!("the run completes the teardown; check it with `nyl status -e {environment}`"),
                    )),
                    Err(finding) => return teardown_report(environment, vec![finding], Vec::new()),
                }
            }
            if !delivered.is_empty() {
                return teardown_report(environment, Vec::new(), delivered);
            }
        }
    }
    let mut request = request.clone();
    request.mode = RunMode::Teardown { target };
    super::run::reconcile(context, &request)
}

/// The units that keep `state delete --teardown` from its deletion: a uid
/// (an incarnation, including retained tombstones and held units that
/// still have one), a deletion in progress, or a recorded teardown request
/// on a held unit. Held files without a uid and markers do not count.
fn remaining(snapshot: &EnvironmentSnapshot) -> Vec<UnitName> {
    snapshot
        .desired
        .iter()
        .filter(|(_, document)| {
            document.unit.uid.is_some()
                || matches!(
                    document.lifecycle,
                    Lifecycle::Deleting { .. } | Lifecycle::Held { deletion: Some(_), .. }
                )
        })
        .map(|(unit, _)| unit.clone())
        .collect()
}

fn incarnations(environment: &EnvironmentName, units: &[UnitName]) -> Finding {
    let list = units.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    Finding::new(
        Code::StateDeleteIncarnations,
        format!("units of {environment} still have an incarnation or a teardown in progress: {list}"),
        format!(
            "tear them down first with `nyl state delete -e {environment} --teardown` or `nyl teardown -e {environment} --unit <u>`, or drop a retained or pending-teardown unit without touching its resources with `nyl state forget -e {environment} --unit <u>`"
        ),
    )
}

/// `nyl state delete`, with `teardown` for `--teardown`. `request` carries
/// the lifecycle options, `--source`, `--reason`, `--wait-lease`, and the
/// cancellation.
pub fn state_delete(context: &RunContext<'_>, request: &RunRequest, teardown: bool) -> StateOperationReport {
    let base = context.base;
    let environment = &request.environment;
    let location = &request.location;
    let new_report = || StateOperationReport::new(environment.clone(), Operation::StateDelete.as_str());
    let mut torn_down = None;
    if teardown {
        // Q23: a broken lease whose run is not imported refuses before the
        // teardown; the teardown would import it, but `state delete` is not
        // an importer.
        match base.store.coordination(location, environment) {
            Ok(coordination) => {
                if let Some(broken) = lease_rule::unimported_break(&coordination) {
                    return new_report().refused(Finding::new(
                        Code::LeaseBroken,
                        format!(
                            "the lease of {environment} was broken and run {} by {} is not imported yet",
                            broken.run, broken.runner
                        ),
                        format!("import it with `nyl recover -e {environment}`, then run `nyl state delete` again"),
                    ));
                }
            }
            Err(error) => return new_report().refused(store_error(environment, &error)),
        }
        let mut teardown_request = request.clone();
        teardown_request.mode = RunMode::Teardown {
            target: TeardownTarget::All,
        };
        let report = self::teardown(context, &teardown_request);
        // The gate is state, not the report's exit.
        let snapshot = match read(base, location, environment) {
            Ok(snapshot) => snapshot,
            Err(finding) => return new_report().refused(finding),
        };
        let left = remaining(&snapshot);
        if !left.is_empty() {
            return stopped(new_report(), environment, &report, &left);
        }
        torn_down = Some(report);
    }
    let requested = Requested {
        reason: request.reason.clone(),
        wait: request.wait,
        cancellation: request.cancellation.clone(),
    };
    let mut report = with_lease(
        base,
        environment,
        Operation::StateDelete,
        location,
        &requested,
        |held, report| delete_holding(base, environment, location, held, &requested, report),
    );
    if let Some(teardown) = torn_down {
        report.details.insert(
            "teardown".to_owned(),
            serde_json::to_value(&teardown).expect("reports serialize"),
        );
    }
    report
}

/// `state delete --teardown` stopped after its teardown: units remain. Exit 1
/// when a teardown failed or is uncertain (or the teardown hit an error),
/// exit 2 when every remaining unit waits (Q54).
fn stopped(
    mut report: StateOperationReport,
    environment: &EnvironmentName,
    teardown: &RunReport,
    left: &[UnitName],
) -> StateOperationReport {
    let exit = teardown.exit();
    report.outcome = Outcome::Refused;
    report.exit = match exit {
        ExitCategory::Error | ExitCategory::Failed | ExitCategory::Uncertain => 1,
        _ => 2,
    };
    for run in &teardown.environments {
        report.errors.extend(run.errors.iter().cloned());
        report.warnings.extend(run.warnings.iter().cloned());
        for (unit, result) in &run.units {
            report.warnings.extend(result.warnings.iter().map(|finding| Finding {
                message: format!("{unit}: {}", finding.message),
                ..finding.clone()
            }));
        }
    }
    if !left.is_empty() {
        report.errors.push(incarnations(environment, left));
    }
    report.details.insert(
        "teardown".to_owned(),
        serde_json::to_value(teardown).expect("reports serialize"),
    );
    report
}

/// The deletion itself, holding the lease.
fn delete_holding(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &nyl_core::orchestration::state::StateLocation,
    held: &mut super::lease::HeldLease,
    requested: &Requested,
    report: &mut StateOperationReport,
) -> Result<(), Finding> {
    let snapshot = read(context, location, environment)?;
    check_record(context.store, snapshot.record.as_ref(), environment, location)?;
    check_remote_tips(context.store, location, environment)?;
    let left = remaining(&snapshot);
    if !left.is_empty() {
        return Err(incarnations(environment, &left));
    }
    let coordination = context
        .store
        .coordination(location, environment)
        .map_err(|error| store_error(environment, &error))?;
    // Q24: a run ref is the only trace of what a crashed run created.
    let runs = coordination
        .runs
        .iter()
        .filter(|state| !state.foreign && state.run.run != held.lease.run)
        .map(|state| state.run.run.to_string())
        .collect::<Vec<_>>();
    if !runs.is_empty() {
        return Err(Finding::new(
            Code::StateDeleteIncarnations,
            format!(
                "run refs of {environment} are not imported yet ({}), and may name units their runs created",
                runs.join(", ")
            ),
            format!(
                "import them with `nyl reconcile -e {environment}` or `nyl recover -e {environment}`, then run `nyl state delete` again"
            ),
        ));
    }
    let mut record = snapshot.record.clone().expect("check_record found state.yaml");
    record.events.push(StateEvent::StateDeleted {
        at: held.lease.taken_at,
        by: context.requested_by.clone(),
        reason: requested.reason.clone(),
    });
    let removed = snapshot.desired.keys().cloned().collect::<Vec<_>>();
    let changes = TransitionChanges {
        clear: true,
        record: Some(record),
        ..TransitionChanges::default()
    };
    let mut summary = summary(Operation::StateDelete, environment, "deleted");
    if !removed.is_empty() {
        summary.details.insert("removed".to_owned(), json!(removed));
    }
    commit(context, location, held, &snapshot, summary, changes, requested, report)?;
    report.details.insert("removed".to_owned(), json!(removed));
    // The leftover records: evidence refs and every signal ref; the state
    // refs and their history stay (Q14).
    for state in &coordination.evidence {
        if let Err(error) = context.store.delete_evidence(location, environment, &state.run.run) {
            report.warnings.push(store_error(environment, &error));
        }
    }
    let mut signalled = coordination
        .evidence
        .iter()
        .chain(&coordination.runs)
        .map(|state| state.run.run.clone())
        .collect::<BTreeSet<_>>();
    signalled.insert(held.lease.run.clone());
    for run in &signalled {
        if let Err(error) = context.store.delete_signals(location, environment, run) {
            if !matches!(error, nyl_core::orchestration::ports::StateError::Unavailable(_)) {
                report.warnings.push(store_error(environment, &error));
            }
        }
    }
    Ok(())
}
