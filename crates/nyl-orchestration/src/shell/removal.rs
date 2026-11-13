//! The report of the removal of an expired instance.
//!
//! Contract: [Expiry](../../../../design/orchestration-core.md#expiry). A
//! removal is `teardown --all` then `state delete` ([`state_delete_detailed`](super::teardown::state_delete_detailed));
//! this renders its two typed results as one reconcile report of the
//! environment, so a pipeline reads a removal like any other run: its units
//! are the teardown's, its exit is the teardown's, and `instance.action` says
//! what became of the instance (`rules::expiry::removal_action`).

use nyl_core::orchestration::codes::Code;
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::progress::UnitResult;

use super::teardown::Deletion;
use crate::report::Outcome;
use crate::rules::expiry::{removal_action, removal_pending_finding};
use crate::run_report::{EnvironmentRunReport, ExitCategory, InstanceAction, InstanceReport, ReportKind};

/// The reconcile report of the removal of `environment`, an instance of
/// `template`, which `deletion` records. `authorized` says whether the
/// teardown was allowed to start, and `instance` is the instance's lifecycle
/// as the command saw it.
pub fn removal_report(
    environment: &EnvironmentName,
    template: &str,
    deletion: Deletion,
    authorized: bool,
    instance: Option<InstanceReport>,
) -> EnvironmentRunReport {
    let removed = deletion.report.outcome == Outcome::Done;
    let mut report = deletion
        .teardown
        .and_then(|teardown| teardown.environments.into_iter().next())
        .unwrap_or_else(|| EnvironmentRunReport::new(environment.clone()));
    // The units the teardown could not finish are the reason state stays; the
    // deletion's own refusal repeats them, and its errors carry the rest.
    for finding in deletion.report.errors {
        if finding.code == Code::StateDeleteIncarnations {
            report.warnings.push(finding);
        } else if !report.errors.contains(&finding) {
            report.errors.push(finding);
        }
    }
    report.warnings.extend(deletion.report.warnings);
    let codes = report.errors.iter().map(|finding| finding.code).collect::<Vec<_>>();
    let pending_units = report
        .units
        .values()
        .filter(|unit| matches!(unit.result, Some(UnitResult::PendingTeardown { .. })))
        .count();
    let action = removal_action(removed, authorized, &codes, pending_units);
    if action == InstanceAction::RemovalPending {
        report.errors.push(removal_pending_finding(environment, template));
    }
    report.finish(ReportKind::ReconcileReport);
    if !removed && report.exit == ExitCategory::Ok {
        // State and resources remain, so the removal is not complete, whatever the units say.
        report.exit = ExitCategory::Incomplete;
        report.complete = false;
    }
    report.instance = instance.map(|mut instance| {
        instance.expired = true;
        instance.action = Some(action);
        instance
    });
    report
}
