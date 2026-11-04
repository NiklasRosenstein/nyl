//! `nyl recover`: clearing a condition, or adopting a checkpointed result.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery)
//! and [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (late checkpoints).
//!
//! - `--retry` clears an uncertain condition or a failure; it never
//!   executes, so the unit becomes ready for the next `reconcile`.
//! - `--accept <run>` adopts the receipt, outputs, and artifacts `<run>`
//!   checkpointed for the unit, allowed only when the checkpoint's execution
//!   key and uid equal the unit's current desired document's.
//! - Both refuse a unit without a condition (`NYL-RECOVER-NOTHING`); accept
//!   refuses a run without such a checkpoint (`NYL-RECOVER-CHECKPOINT-MISSING`)
//!   or with another key (`NYL-RECOVER-KEY-MISMATCH`).
//! - An accepted checkpoint becomes the unit's receipt, so its observed
//!   document carries the copy of the desired document it was produced for
//!   (`ObservedUnit.desired`), which a later hold reverts to.
//! - Holds on uncertain units: `--retry` on a held unit clears only its
//!   condition, and the unit stays held; `--accept` on a held unit is
//!   refused (`NYL-RECOVER-KEY-MISMATCH`, a held unit runs nothing a
//!   checkpoint could match), naming `nyl resume` first.
//! - Changes go through [`crate::rules::transitions::apply`].

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId, UnitName};
use nyl_core::orchestration::ports::{Checkpoint, RunState};
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Lifecycle, ObservedUnit};
use serde::Serialize;

use crate::rules::freshness::same_incarnation;
use crate::rules::transitions::{apply, Applied, Event, Refusal};

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoverAction {
    /// `--retry`.
    Retry,
    /// `--accept <run>`.
    Accept { run: RunId },
}

impl RecoverAction {
    /// `retry` or `accept`, as the transition summary names the action.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Accept { .. } => "accept",
        }
    }
}

/// What `recover` writes for one unit.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovery {
    pub observed: ObservedUnit,
    /// The accepted checkpoint's artifact set; `None` leaves artifacts as they are.
    pub artifacts: Option<BTreeMap<String, ArtifactDocument>>,
}

/// Decide `action` for `unit` from its current documents and the run refs.
///
/// `runs` holds the run refs and evidence refs `--accept <run>` is looked up
/// in. When a run appears more than once (its evidence ref, and a run ref a
/// late runner wrote again after a takeover retired it), the last
/// checkpoint of the unit in `runs` order wins, so callers pass evidence
/// refs before run refs.
pub fn decide(
    environment: &EnvironmentName,
    unit: &UnitName,
    desired: Option<&DesiredUnit>,
    observed: Option<&ObservedUnit>,
    action: &RecoverAction,
    runs: &[RunState],
) -> Result<Recovery, Finding> {
    let Some(desired) = desired else {
        return Err(Finding::new(
            Code::UnitUnknown,
            format!("{environment} has no unit {unit} in its state"),
            format!("list the units with `nyl get units -e {environment}`"),
        ));
    };
    // A held unit runs nothing, so no checkpoint is for what it runs now:
    // resume first. `--retry` clears the condition and keeps the hold.
    if matches!(action, RecoverAction::Accept { .. }) && matches!(desired.lifecycle, Lifecycle::Held { .. }) {
        return Err(Finding::new(
            Code::RecoverKeyMismatch,
            format!("{unit} in {environment} is held; a checkpoint is accepted only for what the unit runs, and a held unit runs nothing"),
            format!(
                "run `nyl resume -e {environment} --unit {unit}` first, then accept the checkpoint; or clear the condition with --retry, which keeps the hold"
            ),
        ));
    }
    // A condition applies only to the incarnation it was recorded for.
    let observed = same_incarnation(desired, observed);
    let nothing = || {
        Finding::new(
            Code::RecoverNothing,
            format!("{unit} in {environment} has no uncertain or failed condition to recover"),
            format!(
                "check the unit with `nyl status -e {environment}`; the next `nyl reconcile` retries a retryable failure without recover"
            ),
        )
    };
    // `apply` refuses only a unit without a condition, and records no
    // recover event as evidence only.
    let recovered = |event: Event, artifacts: Option<BTreeMap<String, ArtifactDocument>>| match apply(
        &desired.unit,
        observed,
        event,
    ) {
        Ok(Applied::Changed(document)) => Ok(Recovery {
            observed: *document,
            artifacts,
        }),
        Ok(Applied::Evidence) | Err(Refusal::NothingToRecover) => Err(nothing()),
    };
    match action {
        RecoverAction::Retry => recovered(Event::RecoverRetry, None),
        RecoverAction::Accept { run } => {
            if observed.and_then(|observed| observed.condition.as_ref()).is_none() {
                return Err(nothing());
            }
            let missing = |detail: &str| {
                Finding::new(
                    Code::RecoverCheckpointMissing,
                    format!("run {run} {detail}"),
                    format!(
                        "pass a run whose checkpoint of {unit} holds a receipt, as `nyl recover` lists them under evidence, or use --retry to execute the unit again"
                    ),
                )
            };
            let checkpoint = runs
                .iter()
                .filter(|state| state.run.run == *run)
                .flat_map(|state| state.checkpoints.iter())
                .rfind(|checkpoint| checkpoint.desired.unit.name == *unit)
                .ok_or_else(|| missing(&format!("has no checkpoint of {unit}")))?;
            let receipt = match (&checkpoint.observed.condition, &checkpoint.observed.receipt) {
                (None, Some(receipt)) => receipt.clone(),
                (Some(condition), _) => {
                    return Err(missing(&format!(
                        "checkpointed {unit} as {}, not with a receipt",
                        condition.name()
                    )))
                }
                (None, None) => return Err(missing(&format!("checkpointed {unit} without a receipt"))),
            };
            if !acceptable(checkpoint, desired) {
                return Err(Finding::new(
                    Code::RecoverKeyMismatch,
                    format!(
                        "run {run} executed {unit} with execution key {} (uid {}), but its current desired document has execution key {} (uid {})",
                        receipt.execution_key,
                        uid_name(&checkpoint.desired),
                        desired
                            .execution_key
                            .as_ref()
                            .map_or_else(|| "none".to_owned(), ToString::to_string),
                        uid_name(desired),
                    ),
                    "a checkpoint is accepted only for what the unit runs now; use --retry to execute the unit again",
                ));
            }
            let mut recovery = recovered(Event::RecoverAccept { receipt }, Some(checkpoint.artifacts.clone()))?;
            // The accepted receipt was produced for the unit's desired
            // document (its uid and key equal the checkpoint's).
            recovery.observed.desired = Some(Box::new(desired.clone()));
            Ok(recovery)
        }
    }
}

/// Acknowledge a driver's recorded retry requirement, without interpreting its context.
pub fn acknowledge_retry(
    desired: &DesiredUnit,
    action: &RecoverAction,
    request: &nyl_core::orchestration::state::Request,
    recovered: &mut ObservedUnit,
) -> Result<(), Finding> {
    if !matches!(action, RecoverAction::Retry)
        || !matches!(
            recovered.retry,
            Some(nyl_core::orchestration::state::RetryAcknowledgement::Required { .. })
        )
    {
        return Ok(());
    }
    if request.reason.as_ref().is_none_or(|reason| reason.trim().is_empty()) {
        return Err(Finding::new(
            Code::RecoverReasonRequired,
            "the driver requires a reason to acknowledge this retry",
            "verify the unit's failure and pass --reason to recover --retry",
        ));
    }
    match crate::rules::transitions::apply_to(
        desired,
        Some(recovered),
        Event::AcknowledgeRetry {
            request: request.clone(),
        },
    ) {
        Ok(Applied::Changed(changed)) => *recovered = *changed,
        _ => {
            return Err(Finding::new(
                Code::RecoverNothing,
                "the retry requirement no longer applies to this unit",
                "read the unit's current state and retry recovery",
            ))
        }
    }
    Ok(())
}

/// Whether the checkpoint receipt and desired document match the current uid and key.
fn acceptable(checkpoint: &Checkpoint, desired: &DesiredUnit) -> bool {
    let Some(key) = &desired.execution_key else {
        return false;
    };
    checkpoint.observed.condition.is_none()
        && checkpoint
            .observed
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.execution_key == *key)
        && checkpoint.desired.execution_key.as_ref() == Some(key)
        && desired.unit.uid.is_some()
        && checkpoint.desired.unit.uid == desired.unit.uid
}

fn uid_name(unit: &DesiredUnit) -> String {
    unit.unit
        .uid
        .as_ref()
        .map_or_else(|| "none".to_owned(), ToString::to_string)
}

/// A checkpoint of a unit on a run ref, shown next to the unit's condition
/// as evidence for `--accept`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointEvidence {
    pub run: RunId,
    /// `receipt`, `failed`, or `uncertain`.
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_key: Option<ExecutionKey>,
    /// Whether `--accept` would adopt it: its key and uid equal the current
    /// desired document's.
    pub acceptable: bool,
}

/// Every checkpoint of `unit` on the run refs, newest run first.
pub fn evidence(unit: &UnitName, desired: Option<&DesiredUnit>, runs: &[RunState]) -> Vec<CheckpointEvidence> {
    let mut found: Vec<CheckpointEvidence> = runs
        .iter()
        .flat_map(|state| {
            state
                .checkpoints
                .iter()
                .filter(|checkpoint| checkpoint.desired.unit.name == *unit)
                .map(|checkpoint| {
                    let (result, execution_key) = match &checkpoint.observed.condition {
                        Some(condition) => (condition.name(), checkpoint.desired.execution_key.clone()),
                        None => (
                            "receipt",
                            checkpoint
                                .observed
                                .receipt
                                .as_ref()
                                .map(|receipt| receipt.execution_key.clone()),
                        ),
                    };
                    CheckpointEvidence {
                        run: state.run.run.clone(),
                        result,
                        execution_key,
                        acceptable: desired.is_some_and(|desired| acceptable(checkpoint, desired)),
                    }
                })
        })
        .collect();
    // Run IDs are UUIDv7: their order is the order the runs started. The
    // sort is stable, so a run's checkpoints keep their order.
    found.sort_by(|left, right| right.run.as_str().cmp(left.run.as_str()));
    found
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::state::{Condition, FailureCategory, Run};

    use super::*;
    use crate::rules::fixtures::{
        desired_numbered, env, hex_key, observed_numbered, run_record, run_state, uid_numbered,
    };

    fn database() -> UnitName {
        UnitName::new("database").unwrap()
    }

    fn run_id(number: u64) -> RunId {
        RunId::new(format!("00000000-0000-7000-9000-{number:012x}")).unwrap()
    }

    fn uncertain(mut observed: ObservedUnit) -> ObservedUnit {
        observed.condition = Some(Condition::Uncertain {
            since: "2026-01-01T01:00:00Z".parse().unwrap(),
            run: run_id(1),
            reason: "run lost its lease".to_owned(),
        });
        observed
    }

    fn failed(mut observed: ObservedUnit, retryable: bool) -> ObservedUnit {
        observed.condition = Some(Condition::Failed {
            since: "2026-01-01T01:00:00Z".parse().unwrap(),
            run: run_id(1),
            category: FailureCategory::ToolError,
            retryable,
            message: "exit 1".to_owned(),
        });
        observed
    }

    /// A run ref of run `number` holding one checkpoint of `database`,
    /// executed as incarnation `uid_number` with `key_digit`, ending with
    /// `observed`.
    fn run_ref(number: u64, uid_number: u64, key_digit: char, observed: ObservedUnit) -> RunState {
        let artifact: ArtifactDocument = serde_json::from_value(serde_json::json!({
            "apiVersion": "artifacts.gitops.nyl/v1",
            "kind": "ContainerImage",
            "metadata": {"name": "image", "unit": {"environment": "dev", "name": "database", "uid": uid_numbered(uid_number)}},
            "executionKey": hex_key(key_digit),
            "spec": {"reference": "registry.example/web@sha256:1"}
        }))
        .unwrap();
        RunState {
            checkpoints: vec![Checkpoint {
                desired: desired_numbered("database", uid_number, Some(hex_key(key_digit))),
                observed,
                artifacts: BTreeMap::from([("image".to_owned(), artifact)]),
            }],
            version: format!("v{number}"),
            ..run_state(Run {
                finished: true,
                ..run_record(run_id(number).as_str(), "2026-01-01T02:00:00Z".parse().unwrap())
            })
        }
    }

    fn decide_for(observed: &ObservedUnit, action: &RecoverAction, runs: &[RunState]) -> Result<Recovery, Finding> {
        decide(
            &env(),
            &database(),
            Some(&desired_numbered("database", 1, Some(hex_key('b')))),
            Some(observed),
            action,
            runs,
        )
    }

    /// Catches `--retry` leaving the condition (the unit would stay stuck)
    /// or dropping the last receipt, which documents what ran. A retryable
    /// failure is recoverable too, so an operator need not wait for the next
    /// reconcile to clear it.
    #[test]
    fn test_retry_clears_uncertain_and_failed_keeping_the_receipt() {
        for current in [
            uncertain(observed_numbered("database", 1, Some(hex_key('a')))),
            failed(observed_numbered("database", 1, Some(hex_key('a'))), false),
            failed(observed_numbered("database", 1, Some(hex_key('a'))), true),
        ] {
            let recovery = decide_for(&current, &RecoverAction::Retry, &[]).unwrap();
            assert_eq!(recovery.observed.condition, None);
            assert_eq!(recovery.observed.receipt, current.receipt);
            assert_eq!(recovery.artifacts, None, "a retry leaves artifacts as they are");
        }
    }

    /// Catches `recover` writing a commit for a unit with nothing to
    /// recover, or treating an unknown unit as recoverable.
    #[test]
    fn test_recover_refuses_without_a_condition_and_for_unknown_units() {
        let current = observed_numbered("database", 1, Some(hex_key('a')));
        let refusal = decide_for(&current, &RecoverAction::Retry, &[]).unwrap_err();
        assert_eq!(refusal.code, Code::RecoverNothing);
        assert!(refusal.fix.contains("nyl status"), "{}", refusal.fix);
        let accept = RecoverAction::Accept { run: run_id(2) };
        assert_eq!(
            decide_for(&current, &accept, &[]).unwrap_err().code,
            Code::RecoverNothing
        );
        let unknown = decide(&env(), &database(), None, Some(&current), &RecoverAction::Retry, &[]);
        assert_eq!(unknown.unwrap_err().code, Code::UnitUnknown);
    }

    /// Catches a condition of a former incarnation being recovered onto the
    /// current one: it does not apply to the new uid.
    #[test]
    fn test_condition_of_another_incarnation_is_not_recoverable() {
        let former = uncertain(observed_numbered("database", 9, Some(hex_key('a'))));
        assert_eq!(
            decide_for(&former, &RecoverAction::Retry, &[]).unwrap_err().code,
            Code::RecoverNothing
        );
    }

    /// Catches `--accept` not adopting a late checkpoint that matches what
    /// the unit runs now, or losing its artifacts.
    #[test]
    fn test_accept_adopts_a_matching_checkpoint_with_its_artifacts() {
        let current = uncertain(observed_numbered("database", 1, Some(hex_key('a'))));
        let late = run_ref(2, 1, 'b', observed_numbered("database", 1, Some(hex_key('b'))));
        let recovery = decide_for(
            &current,
            &RecoverAction::Accept { run: run_id(2) },
            std::slice::from_ref(&late),
        )
        .unwrap();
        assert_eq!(recovery.observed.condition, None);
        assert_eq!(recovery.observed.receipt, late.checkpoints[0].observed.receipt);
        assert_eq!(recovery.artifacts, Some(late.checkpoints[0].artifacts.clone()));
        assert_eq!(
            recovery.observed.desired.as_deref(),
            Some(&desired_numbered("database", 1, Some(hex_key('b')))),
            "the receipt carries the desired document it was produced for"
        );
    }

    fn held(mut desired: DesiredUnit) -> DesiredUnit {
        desired.lifecycle = Lifecycle::Held {
            hold: serde_json::from_value(serde_json::json!({"since": "2026-01-01T03:00:00Z", "by": "alice"})).unwrap(),
            deletion: None,
        };
        desired
    }

    /// Q21: catches `--retry` on a held uncertain unit refused, or touching
    /// more than the condition (the hold is the desired document's, which
    /// recover never writes).
    #[test]
    fn test_retry_on_a_held_uncertain_unit_clears_only_the_condition() {
        let current = uncertain(observed_numbered("database", 1, Some(hex_key('a'))));
        let desired = held(desired_numbered("database", 1, Some(hex_key('a'))));
        let recovery = decide(
            &env(),
            &database(),
            Some(&desired),
            Some(&current),
            &RecoverAction::Retry,
            &[],
        )
        .unwrap();
        assert_eq!(recovery.observed.condition, None);
        assert_eq!(recovery.observed.receipt, current.receipt);
        assert_eq!(recovery.observed.desired, current.desired);
        assert_eq!(recovery.artifacts, None);
    }

    /// Q21: catches `--accept` adopting a checkpoint onto a held unit, whose
    /// desired document is the reverted one, with a fix naming `nyl resume`.
    #[test]
    fn test_accept_on_a_held_unit_is_refused_naming_resume() {
        let current = uncertain(observed_numbered("database", 1, Some(hex_key('a'))));
        let desired = held(desired_numbered("database", 1, Some(hex_key('b'))));
        let late = run_ref(2, 1, 'b', observed_numbered("database", 1, Some(hex_key('b'))));
        let refusal = decide(
            &env(),
            &database(),
            Some(&desired),
            Some(&current),
            &RecoverAction::Accept { run: run_id(2) },
            &[late],
        )
        .unwrap_err();
        assert_eq!(refusal.code, Code::RecoverKeyMismatch);
        assert!(
            refusal.fix.contains("nyl resume -e dev --unit database"),
            "{}",
            refusal.fix
        );
    }

    /// Catches `--accept` adopting a receipt for other inputs or another
    /// incarnation, or a run without a receipt of the unit.
    #[test]
    fn test_accept_refuses_a_mismatched_key_and_a_missing_or_failed_checkpoint() {
        let current = uncertain(observed_numbered("database", 1, Some(hex_key('a'))));
        let accept = RecoverAction::Accept { run: run_id(2) };
        let other_key = run_ref(2, 1, 'c', observed_numbered("database", 1, Some(hex_key('c'))));
        let refusal = decide_for(&current, &accept, &[other_key]).unwrap_err();
        assert_eq!(refusal.code, Code::RecoverKeyMismatch);
        assert!(
            refusal.message.contains(hex_key('c').as_str()) && refusal.message.contains(hex_key('b').as_str()),
            "{}",
            refusal.message
        );
        let other_uid = run_ref(2, 5, 'b', observed_numbered("database", 5, Some(hex_key('b'))));
        assert_eq!(
            decide_for(&current, &accept, &[other_uid]).unwrap_err().code,
            Code::RecoverKeyMismatch
        );
        assert_eq!(
            decide_for(&current, &accept, &[]).unwrap_err().code,
            Code::RecoverCheckpointMissing
        );
        let failure = run_ref(2, 1, 'b', failed(observed_numbered("database", 1, None), true));
        assert_eq!(
            decide_for(&current, &accept, &[failure]).unwrap_err().code,
            Code::RecoverCheckpointMissing
        );
    }

    /// Catches a run that appears as an evidence ref and again as a run ref
    /// (a late runner wrote after the takeover) accepting the older
    /// checkpoint instead of the newer one.
    #[test]
    fn test_accept_takes_the_last_checkpoint_of_a_run_in_order() {
        let current = uncertain(observed_numbered("database", 1, Some(hex_key('a'))));
        let older = run_ref(2, 1, 'b', failed(observed_numbered("database", 1, None), true));
        let newer = run_ref(2, 1, 'b', observed_numbered("database", 1, Some(hex_key('b'))));
        let accept = RecoverAction::Accept { run: run_id(2) };
        assert!(decide_for(&current, &accept, &[older, newer]).is_ok());
    }

    /// Catches an operator seeing checkpoints out of order or without
    /// whether `--accept` would take them.
    #[test]
    fn test_evidence_lists_checkpoints_newest_first_with_acceptability() {
        let runs = [
            run_ref(2, 1, 'c', observed_numbered("database", 1, Some(hex_key('c')))),
            run_ref(3, 1, 'b', observed_numbered("database", 1, Some(hex_key('b')))),
            run_ref(4, 1, 'b', uncertain(observed_numbered("database", 1, None))),
        ];
        let desired = desired_numbered("database", 1, Some(hex_key('b')));
        let found = evidence(&database(), Some(&desired), &runs);
        let summary: Vec<(RunId, &str, bool)> = found
            .iter()
            .map(|entry| (entry.run.clone(), entry.result, entry.acceptable))
            .collect();
        assert_eq!(
            summary,
            vec![
                (run_id(4), "uncertain", false),
                (run_id(3), "receipt", true),
                (run_id(2), "receipt", false),
            ]
        );
        assert!(evidence(&UnitName::new("network").unwrap(), Some(&desired), &runs).is_empty());
    }
}
