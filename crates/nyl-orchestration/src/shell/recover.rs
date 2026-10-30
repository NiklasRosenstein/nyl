//! `nyl recover -e <env> [--unit <u> (--retry | --accept <run>)]`.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery)
//! and [Commands](../../../../design/orchestration-core.md#commands)
//! (`recover` writes one observed commit). It never executes. Exit: 0
//! done (also once an import is recorded with units left `uncertain`), 1
//! error or refusal, 2 the lease is held.
//!
//! - **Importing a broken run** ([`recover_environment`]): when the lease
//!   is broken and its run not imported
//!   ([`crate::rules::lease::unimported_break`]), `recover` takes the lease
//!   over and imports that run through the shared run path with execution
//!   switched off, applies the `--unit` action after the import
//!   ([`after_import`]), and writes one transition commit recording the
//!   import and the break (`leaseBroken`), even when nothing else changed.
//!   It writes no run record of its own, consumes the broken run's refs as
//!   a takeover does, and exits 0 once the import is recorded. If it ends
//!   before its commit lands, the broken lease is put back and the broken
//!   run's refs stay for the next import.
//! - **The way out** (Q40): where `recover` would otherwise refuse, because
//!   `state.yaml` is missing (a broken `state init`), the state moved
//!   (`NYL-STATE-MOVED`), `recover -e` without `--unit` still takes the
//!   broken lease over, records the break with an unchanged commit where
//!   that is allowed, and releases the lease, so `state init`, `state
//!   move`, or `state restore` can proceed.
//! - **A unit action without a break** ([`recover`]): takes the lease,
//!   decides with [`crate::rules::recovery`], and writes one observed commit
//!   with operation `recover`, the unit under the summary's `recovered`, and
//!   the `Nyl-Requested-By` and `Nyl-Reason` trailers. The report's
//!   `details` show the unit's condition and the checkpoints the run and
//!   evidence refs hold for it ([`crate::rules::recovery::evidence`]).
//!   Without `--unit` and without a broken run, `NYL-RECOVER-NOTHING`.
//!
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases):
//! apart from importing a broken run, `recover` never deletes or retires
//! run refs: a dead run's ref that no reconcile took over yet is the only
//! record of its checkpoints and of the units it was executing, and
//! consuming it is the takeover's job. After its commit lands, `recover`
//! deletes the evidence refs none of whose checkpoints is for a unit that
//! is still `uncertain`.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{RunState, TransitionChanges};
use nyl_core::orchestration::state::{
    ArtifactDocument, Condition, DesiredUnit, ObservedUnit, Operation, StateLocation,
};
use serde_json::{json, Value};

use super::local::{check_record, check_remote_tips};
use super::run::{RunContext, RunMode, RunRequest};
use super::state::{commit, read, summary, with_lease, Requested};
use super::{lease, store_error, Context};
use crate::report::{Outcome, StateOperationReport};
use crate::rules::lease::{unimported_break, BrokenLease};
use crate::rules::recovery::{decide, evidence, RecoverAction};
use crate::transition::TransitionSummary;

/// `--unit <u> --retry` or `--accept <run>` of `nyl recover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverUnit {
    pub unit: UnitName,
    pub action: RecoverAction,
}

/// `nyl recover -e <env> [--unit <u> (--retry | --accept <run>)]`;
/// `request.mode` is [`RunMode::Recover`].
///
/// When the lease is broken and its run not imported, it takes the lease
/// over and imports that run through the shared run path with execution
/// switched off, applying the unit's action after the import in the same
/// commit ([`after_import`]). It writes no run record: its one transition
/// commit (import, `leaseBroken`, the optional action) is all it writes,
/// and it consumes the broken run's refs as a takeover does; if it crashes,
/// those refs stay for the next import. Where the state is missing or moved,
/// it clears the break without importing (see the
/// module documentation). Otherwise it applies the action as [`recover`]
/// does, and without `--unit` refuses with `NYL-RECOVER-NOTHING`. Contract:
/// [Recovery](../../../../design/orchestration-core.md#recovery), "Importing a broken run".
pub fn recover_environment(context: &RunContext<'_>, request: &RunRequest) -> StateOperationReport {
    let base = context.base;
    let environment = &request.environment;
    let location = &request.location;
    let report = StateOperationReport::new(environment.clone(), Operation::Recover.as_str());
    let RunMode::Recover { unit } = &request.mode else {
        unreachable!("recover_environment runs only in RunMode::Recover");
    };
    let requested = Requested {
        reason: request.reason.clone(),
        wait: request.wait,
        cancellation: request.cancellation.clone(),
    };
    let coordination = match base.store.coordination(location, environment) {
        Ok(coordination) => coordination,
        Err(error) => return report.refused(store_error(environment, &error)),
    };
    if unimported_break(&coordination).is_none() {
        return match unit {
            Some(recover_unit) => recover(
                base,
                environment,
                location,
                &recover_unit.unit,
                &recover_unit.action,
                &requested,
            ),
            None => report.refused(nothing_to_import(environment)),
        };
    }
    let snapshot = match read(base, location, environment) {
        Ok(snapshot) => snapshot,
        Err(finding) => return report.refused(finding),
    };
    let checked = check_record(base.store, snapshot.record.as_ref(), environment, location)
        .and_then(|()| check_remote_tips(base.store, location, environment));
    match checked {
        Ok(()) => import(context, request, report),
        Err(finding) if way_out(&finding) && unit.is_none() => clear_break(base, request, &requested, report, finding),
        Err(finding) => report.refused(finding),
    }
}

/// `NYL-RECOVER-NOTHING` for `recover -e <env>` without `--unit` and without
/// a broken run to import.
fn nothing_to_import(environment: &EnvironmentName) -> Finding {
    Finding::new(
        Code::RecoverNothing,
        format!("the lease of {environment} is not broken, so there is no broken run to import"),
        format!(
            "name a unit to recover with `nyl recover -e {environment} --unit <unit> (--retry | --accept <run>)`; `nyl status -e {environment}` lists uncertain and failed units"
        ),
    )
}

/// Whether `recover -e` clears a break where it would otherwise refuse
/// (Q40): state missing, moved away, or deleted by `nyl state delete`.
fn way_out(finding: &Finding) -> bool {
    matches!(finding.code, Code::StateMissing | Code::StateMoved | Code::StateDeleted)
}

/// Import the broken run through the run path and report it as a state
/// operation: done, exit 0, once its commit landed (Q43).
fn import(context: &RunContext<'_>, request: &RunRequest, report: StateOperationReport) -> StateOperationReport {
    let broken = context
        .base
        .store
        .coordination(&request.location, &request.environment)
        .ok()
        .and_then(|coordination| unimported_break(&coordination).and_then(|lease| lease.broken.clone()));
    let run = super::run::reconcile(context, request);
    let Some(environment) = run.environments.into_iter().next() else {
        return report;
    };
    let mut report = report;
    report.warnings.extend(environment.warnings);
    if environment.commits.is_empty() {
        let mut errors = environment.errors.into_iter();
        return match errors.next() {
            Some(first) => {
                let mut report = report.refused(first);
                report.errors.extend(errors);
                report
            }
            None => report,
        };
    }
    // The import is recorded: what else went wrong is reported, but the
    // operation is done.
    report.warnings.extend(environment.errors);
    report.commits = environment.commits;
    report
        .details
        .insert("imported".to_owned(), json!(environment.imported));
    report
        .details
        .insert("recovered".to_owned(), json!(environment.recovered));
    if let Some(broken) = broken {
        report.details.insert("leaseBroken".to_owned(), json!(broken));
    }
    if let RunMode::Recover { unit: Some(recover) } = &request.mode {
        report
            .details
            .insert(recover.unit.to_string(), action_entry(&recover.action));
    }
    report.done()
}

/// The way out of a broken lease where the state cannot be imported into
/// (Q40): take the broken lease over, record the break with an unchanged
/// commit where state exists and its tip is Nyl's own, and release the
/// lease. `finding` says what still blocks the environment; it stays a
/// warning naming the command that resolves it.
fn clear_break(
    context: &Context<'_>,
    request: &RunRequest,
    requested: &Requested,
    report: StateOperationReport,
    finding: Finding,
) -> StateOperationReport {
    let environment = &request.environment;
    let location = &request.location;
    let mut held = match lease::acquire(
        context,
        location,
        environment,
        &context.ids.run_id(),
        Operation::Recover,
        BrokenLease::TakeOver,
        context.settings.resolution_timeout,
        request.wait,
        &request.cancellation,
    ) {
        Ok(held) => held,
        Err(finding) => return report.refused(finding),
    };
    let mut report = report;
    let broken = held.broken.clone();
    let committed = read(context, location, environment).and_then(|snapshot| {
        if snapshot.record.is_none() {
            return Ok(());
        }
        let mut summary = summary(Operation::Recover, environment, "");
        summarize(&mut summary, None);
        let changes = TransitionChanges {
            commit_unchanged: true,
            ..TransitionChanges::default()
        };
        commit(
            context,
            location,
            &mut held,
            &snapshot,
            summary,
            changes,
            requested,
            &mut report,
        )
    });
    let landed = committed.is_ok();
    if let Err(error) = lease::release(context, &held, landed) {
        report.warnings.push(store_error(environment, &error));
    }
    if let Err(error) = committed {
        return report.refused(error);
    }
    if let Some(broken) = broken {
        report.details.insert("leaseBroken".to_owned(), json!(broken));
    }
    report.warnings.push(finding);
    report.done()
}

/// The `units` entry of a `--unit` action, such as `{"action": "retry"}`.
fn action_entry(action: &RecoverAction) -> Value {
    match action {
        RecoverAction::Retry => json!({"action": "retry"}),
        RecoverAction::Accept { run } => json!({"action": "accept", "run": run}),
    }
}

/// The headline part of a `--unit` action, such as `database: retry`.
fn action_headline(unit: &UnitName, entry: &Value) -> String {
    let action = entry.get("action").and_then(Value::as_str).unwrap_or_default();
    match entry.get("run").and_then(Value::as_str) {
        Some(run) => format!("{unit}: {action} {run}"),
        None => format!("{unit}: {action}"),
    }
}

/// Complete the transition summary of a recover import (Q44): the headline
/// gives the import's counts, then the `--unit` action after a semicolon,
/// such as `1 imported, 2 recovered; database: retry`; the action's unit
/// is listed under `units` with its action and under `recovered`.
pub(crate) fn summarize(summary: &mut TransitionSummary, action: Option<(UnitName, Value)>) {
    let mut counts = Vec::new();
    if !summary.imported.is_empty() {
        counts.push(format!("{} imported", summary.imported.len()));
    }
    if !summary.recovered.is_empty() {
        counts.push(format!("{} recovered", summary.recovered.len()));
    }
    let mut headline = counts.join(", ");
    if let Some((unit, entry)) = action {
        let part = action_headline(&unit, &entry);
        headline = if headline.is_empty() {
            part
        } else {
            format!("{headline}; {part}")
        };
        if !summary.recovered.contains(&unit) {
            summary.recovered.push(unit.clone());
        }
        summary.units.insert(unit, entry);
    }
    summary.headline = headline;
}

/// The `--unit` action of a `recover` that imported a broken run, applied
/// to the observed state after the import, in the same transition commit:
/// updates `observed` and `artifacts` and returns the unit's entry for the
/// summary's `units` block, such as `{"action": "retry"}`. `runs` are the
/// leftover and evidence run refs the import read, so `--accept` finds the
/// checkpoint; an accepted checkpoint becomes the receipt, carrying the
/// copy of `desired` it was produced for. A refusal is the finding.
#[allow(clippy::too_many_arguments)] // Recovery result, evidence, and the operator acknowledgement.
pub(crate) fn after_import(
    environment: &EnvironmentName,
    recover: &RecoverUnit,
    desired: Option<&DesiredUnit>,
    observed: &mut BTreeMap<UnitName, ObservedUnit>,
    artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    runs: &[RunState],
    acknowledgement: &nyl_core::orchestration::state::Request,
) -> Result<Value, Finding> {
    let mut recovery = decide(
        environment,
        &recover.unit,
        desired,
        observed.get(&recover.unit),
        &recover.action,
        runs,
    )?;
    if let Some(desired) = desired {
        crate::rules::recovery::acknowledge_retry(desired, &recover.action, acknowledgement, &mut recovery.observed)?;
    }
    if let Some(set) = recovery.artifacts {
        if set.is_empty() {
            artifacts.remove(&recover.unit);
        } else {
            artifacts.insert(recover.unit.clone(), set);
        }
    }
    observed.insert(recover.unit.clone(), recovery.observed);
    Ok(action_entry(&recover.action))
}

/// `nyl recover`.
pub fn recover(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    action: &RecoverAction,
    requested: &Requested,
) -> StateOperationReport {
    with_lease(
        context,
        environment,
        Operation::Recover,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            let coordination = context
                .store
                .coordination(location, environment)
                .map_err(|error| store_error(environment, &error))?;
            // Evidence refs first: a run ref of the same run that a late
            // runner wrote after the takeover retired it is newer.
            let runs: Vec<RunState> = coordination
                .evidence
                .iter()
                .chain(coordination.runs.iter().filter(|state| !state.foreign))
                .cloned()
                .collect();
            let desired = snapshot.desired.get(unit);
            let observed = snapshot.observed.get(unit);
            let condition = desired
                .and_then(|desired| crate::rules::freshness::same_incarnation(desired, observed))
                .and_then(|observed| observed.condition.as_ref());
            report.details.insert(
                "condition".to_owned(),
                serde_json::to_value(condition).expect("conditions serialize"),
            );
            report.details.insert(
                "evidence".to_owned(),
                serde_json::to_value(evidence(unit, desired, &runs)).expect("evidence serializes"),
            );
            let mut recovery = decide(environment, unit, desired, observed, action, &runs)?;
            if let Some(desired) = desired {
                crate::rules::recovery::acknowledge_retry(
                    desired,
                    action,
                    &nyl_core::orchestration::state::Request {
                        by: context.requested_by.clone(),
                        at: context.clock.now(),
                        reason: requested.reason.clone(),
                    },
                    &mut recovery.observed,
                )?;
            }

            let mut changes = TransitionChanges {
                observed: BTreeMap::from([(unit.clone(), Some(recovery.observed.clone()))]),
                ..TransitionChanges::default()
            };
            if let Some(set) = &recovery.artifacts {
                let after = (!set.is_empty()).then(|| set.clone());
                if snapshot.artifacts.get(unit) != after.as_ref() {
                    changes.artifacts.insert(unit.clone(), after);
                }
            }
            let mut summary = summary(Operation::Recover, environment, "");
            summarize(&mut summary, Some((unit.clone(), action_entry(action))));
            commit(context, location, held, &snapshot, summary, changes, requested, report)?;
            if report.outcome != Outcome::Done {
                return Ok(());
            }

            // Evidence refs no uncertain unit needs any more.
            let mut observed_after = snapshot.observed.clone();
            observed_after.insert(unit.clone(), recovery.observed);
            let uncertain = |name: &UnitName| {
                observed_after
                    .get(name)
                    .is_some_and(|observed| matches!(observed.condition, Some(Condition::Uncertain { .. })))
            };
            for retired in &coordination.evidence {
                if retired
                    .checkpoints
                    .iter()
                    .any(|checkpoint| uncertain(&checkpoint.desired.unit.name))
                {
                    continue;
                }
                if let Err(error) = context.store.delete_evidence(location, environment, &retired.run.run) {
                    report.warnings.push(store_error(environment, &error));
                }
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::rules::fixtures;
    use nyl_core::orchestration::state::{FailureCategory, Request, RetryAcknowledgement};

    #[test]
    fn test_imported_retry_requires_a_reason_and_returns_opaque_context() {
        let desired = fixtures::desired("service", "TestUnit", json!({}), "key");
        let mut failed = fixtures::observed("service", "TestUnit", None);
        failed.condition = Some(Condition::Failed {
            since: "2026-10-28T00:00:00Z".parse().unwrap(),
            run: fixtures::run(fixtures::RUN_1),
            category: FailureCategory::Ownership,
            retryable: false,
            message: "driver diagnostic".to_owned(),
        });
        let context = json!({"version":1,"resource":"test-scope"});
        failed.retry = Some(RetryAcknowledgement::Required {
            context: context.clone(),
        });
        let recover = RecoverUnit {
            unit: fixtures::name("service"),
            action: RecoverAction::Retry,
        };
        let mut observed = BTreeMap::from([(recover.unit.clone(), failed.clone())]);
        let mut artifacts = BTreeMap::new();
        let mut request = Request {
            by: "operator".to_owned(),
            at: "2026-10-28T00:00:00Z".parse().unwrap(),
            reason: None,
        };
        assert_eq!(
            after_import(
                &fixtures::env(),
                &recover,
                Some(&desired),
                &mut observed,
                &mut artifacts,
                &[],
                &request
            )
            .unwrap_err()
            .code,
            Code::RecoverReasonRequired
        );
        assert_eq!(observed[&recover.unit], failed);
        request.reason = Some("verified the resource".to_owned());
        after_import(
            &fixtures::env(),
            &recover,
            Some(&desired),
            &mut observed,
            &mut artifacts,
            &[],
            &request,
        )
        .unwrap();
        assert_eq!(
            observed[&recover.unit].retry,
            Some(RetryAcknowledgement::Acknowledged { context, request })
        );
        assert!(observed[&recover.unit].condition.is_none());
    }
}
