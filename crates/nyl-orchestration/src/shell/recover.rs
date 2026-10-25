//! `nyl recover -e <env> --unit <u> (--retry | --accept <run>)`.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery)
//! and [Commands](../../../../design/orchestration-core.md#commands)
//! (`recover` writes one observed commit). Takes the lease, decides with
//! [`crate::rules::recovery`], and writes one observed commit with
//! operation `recover`, the unit under the summary's `recovered`, and the
//! `Nyl-Requested-By` and `Nyl-Reason` trailers. It never executes. Exit: 0
//! done, 1 error or refusal, 2 lease held. The report's `details` show the
//! unit's condition and the checkpoints the run and evidence refs hold for
//! it ([`crate::rules::recovery::evidence`]).
//!
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases):
//! `recover` never deletes or retires run refs: a dead run's ref that no
//! reconcile took over yet is the only record of its checkpoints and of the
//! units it was executing, and consuming it is the takeover's job. After
//! its commit lands, `recover` deletes the evidence refs none of whose
//! checkpoints is for a unit that is still `uncertain`.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, RunState, TransitionChanges};
use nyl_core::orchestration::state::{Condition, Operation, StateLocation};
use serde_json::json;

use super::state::{commit, store_error, summary, with_lease, Requested};
use super::Context;
use crate::report::{Outcome, StateOperationReport};
use crate::rules::recovery::{decide, evidence, RecoverAction};

/// The state record matches the environment, and remote state carries no
/// local tip.
fn check_state(
    context: &Context<'_>,
    snapshot: &EnvironmentSnapshot,
    environment: &EnvironmentName,
    location: &StateLocation,
) -> Result<(), Finding> {
    super::local::check_record(context.store, snapshot.record.as_ref(), environment, location)?;
    super::local::check_remote_tips(context.store, location, environment)
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
    let report = StateOperationReport::new(environment.clone(), Operation::Recover.as_str());
    with_lease(
        context,
        report,
        Operation::Recover,
        location,
        requested,
        |held, mut report| {
            let snapshot = match context.store.snapshot(location) {
                Ok(snapshot) => snapshot,
                Err(error) => return report.refused(store_error(environment, &error)),
            };
            if let Err(finding) = check_state(context, &snapshot, environment, location) {
                return report.refused(finding);
            }
            let coordination = match context.store.coordination(location, environment) {
                Ok(coordination) => coordination,
                Err(error) => return report.refused(store_error(environment, &error)),
            };
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
            let condition = observed
                .filter(|observed| desired.is_some_and(|desired| desired.unit.uid == observed.unit.uid))
                .and_then(|observed| observed.condition.as_ref());
            report.details.insert(
                "condition".to_owned(),
                serde_json::to_value(condition).expect("conditions serialize"),
            );
            report.details.insert(
                "evidence".to_owned(),
                serde_json::to_value(evidence(unit, desired, &runs)).expect("evidence serializes"),
            );
            let recovery = match decide(environment, unit, desired, observed, action, &runs) {
                Ok(recovery) => recovery,
                Err(finding) => return report.refused(finding),
            };

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
            let (headline, entry) = match action {
                RecoverAction::Retry => (format!("{unit}: retry"), json!({"action": "retry"})),
                RecoverAction::Accept { run } => {
                    (format!("{unit}: accept {run}"), json!({"action": "accept", "run": run}))
                }
            };
            let mut summary = summary(Operation::Recover, environment, &headline);
            summary.units.insert(unit.clone(), entry);
            summary.recovered.push(unit.clone());
            let report = commit(
                context,
                location,
                held,
                &snapshot,
                &summary,
                changes,
                requested.reason.as_ref(),
                report,
            );
            if report.outcome != Outcome::Done {
                return report;
            }

            // Evidence refs no uncertain unit needs any more.
            let mut observed_after = snapshot.observed.clone();
            observed_after.insert(unit.clone(), recovery.observed);
            let uncertain = |name: &UnitName| {
                observed_after
                    .get(name)
                    .is_some_and(|observed| matches!(observed.condition, Some(Condition::Uncertain { .. })))
            };
            let mut report = report;
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
            report
        },
    )
}
