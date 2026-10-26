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
use nyl_core::orchestration::ports::{RunState, TransitionChanges};
use nyl_core::orchestration::state::{
    ArtifactDocument, Condition, DesiredUnit, ObservedUnit, Operation, StateLocation,
};
use serde_json::json;

use super::local::{check_record, check_remote_tips};
use super::run::{RunContext, RunRequest};
use super::state::{commit, read, summary, with_lease, Requested};
use super::{store_error, Context};
use crate::report::{Outcome, StateOperationReport};
use crate::rules::recovery::{decide, evidence, RecoverAction};

/// `--unit <u> --retry` or `--accept <run>` of `nyl recover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverUnit {
    pub unit: UnitName,
    pub action: RecoverAction,
}

/// `nyl recover -e <env> [--unit <u> (--retry | --accept <run>)]`.
///
/// When the lease is broken and its run not imported, it takes the lease
/// over and imports that run through the shared run path with execution
/// switched off (`request.mode` is [`RunMode::Recover`](super::run::RunMode::Recover)),
/// applying the unit's action after the import in the same commit
/// ([`after_import`]). It writes no run record: its one transition commit
/// (import, `leaseBroken`, the optional action) is all it writes, and it
/// consumes the broken run's refs as a takeover does; if it crashes, those
/// refs stay for the next import. Otherwise it applies the action as [`recover`] does,
/// and without `--unit` refuses with `NYL-RECOVER-NOTHING`. Contract:
/// [Recovery](../../../../design/orchestration-core.md#recovery), "Importing a broken run".
pub fn recover_environment(context: &RunContext<'_>, request: &RunRequest) -> StateOperationReport {
    let _ = (context, request);
    todo!("lease-break")
}

/// The `--unit` action of a `recover` that imported a broken run, applied
/// to the observed state after the import, in the same transition commit:
/// updates `observed` and `artifacts` and returns the unit's entry for the
/// summary's `units` block, such as `{"action": "retry"}`. `runs` are the
/// leftover and evidence run refs the import read, so `--accept` finds the
/// checkpoint.
pub(crate) fn after_import(
    environment: &EnvironmentName,
    recover: &RecoverUnit,
    desired: Option<&DesiredUnit>,
    observed: &mut BTreeMap<UnitName, ObservedUnit>,
    artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    runs: &[RunState],
) -> Result<serde_json::Value, Finding> {
    let _ = (environment, recover, desired, observed, artifacts, runs);
    todo!("lease-break")
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
            // The state record matches the environment, and remote state
            // carries no local tip.
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
            let recovery = decide(environment, unit, desired, observed, action, &runs)?;

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
