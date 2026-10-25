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
//! - Changes go through [`crate::rules::transitions::apply`].

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId, UnitName};
use nyl_core::orchestration::ports::{Checkpoint, RunState};
use nyl_core::orchestration::state::{ArtifactDocument, Condition, DesiredUnit, ObservedUnit};
use serde::Serialize;

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
    // A condition applies only to the incarnation it was recorded for.
    let observed = observed.filter(|observed| observed.unit.uid == desired.unit.uid);
    let nothing = || {
        Finding::new(
            Code::RecoverNothing,
            format!("{unit} in {environment} has no uncertain or failed condition to recover"),
            format!(
                "check the unit with `nyl status -e {environment}`; the next `nyl reconcile` retries a retryable failure without recover"
            ),
        )
    };
    let refused = |refusal: Refusal| match refusal {
        Refusal::NothingToRecover => nothing(),
    };
    match action {
        RecoverAction::Retry => match apply(&desired.unit, observed, Event::RecoverRetry).map_err(refused)? {
            Applied::Changed(document) => Ok(Recovery {
                observed: *document,
                artifacts: None,
            }),
            Applied::Evidence => Err(nothing()),
        },
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
                        condition_name(condition)
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
            match apply(&desired.unit, observed, Event::RecoverAccept { receipt }).map_err(refused)? {
                Applied::Changed(document) => Ok(Recovery {
                    observed: *document,
                    artifacts: Some(checkpoint.artifacts.clone()),
                }),
                Applied::Evidence => Err(nothing()),
            }
        }
    }
}

/// Whether `--accept` adopts `checkpoint` for `desired`: it holds a receipt,
/// and its desired document's uid and execution key, and the receipt's key,
/// equal the current desired document's.
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

fn condition_name(condition: &Condition) -> &'static str {
    match condition {
        Condition::Failed { .. } => "failed",
        Condition::Uncertain { .. } => "uncertain",
    }
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
                        Some(condition) => (condition_name(condition), checkpoint.desired.execution_key.clone()),
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
    use nyl_core::orchestration::state::{FailureCategory, Operation, Run, RunKind, StateV1};

    use super::*;
    use crate::rules::freshness::fixtures::{desired_named, key, observed_named, uid};

    fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

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
            "metadata": {"name": "image", "unit": {"environment": "dev", "name": "database", "uid": uid(uid_number)}},
            "executionKey": key(key_digit),
            "spec": {"reference": "registry.example/web@sha256:1"}
        }))
        .unwrap();
        RunState {
            run: Run {
                api_version: StateV1,
                kind: RunKind,
                run: run_id(number),
                runner: "ci".to_owned(),
                operation: Operation::Reconcile,
                source_commit: None,
                read_desired: None,
                read_observed: None,
                deadline: "2026-01-01T02:00:00Z".parse().unwrap(),
                units: Vec::new(),
                finished: true,
            },
            checkpoints: vec![Checkpoint {
                desired: desired_named("database", uid_number, Some(key(key_digit))),
                observed,
                artifacts: BTreeMap::from([("image".to_owned(), artifact)]),
            }],
            created: Vec::new(),
            version: format!("v{number}"),
            foreign: false,
        }
    }

    fn decide_for(observed: &ObservedUnit, action: &RecoverAction, runs: &[RunState]) -> Result<Recovery, Finding> {
        decide(
            &dev(),
            &database(),
            Some(&desired_named("database", 1, Some(key('b')))),
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
            uncertain(observed_named("database", 1, Some(key('a')))),
            failed(observed_named("database", 1, Some(key('a'))), false),
            failed(observed_named("database", 1, Some(key('a'))), true),
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
        let current = observed_named("database", 1, Some(key('a')));
        let refusal = decide_for(&current, &RecoverAction::Retry, &[]).unwrap_err();
        assert_eq!(refusal.code, Code::RecoverNothing);
        assert!(refusal.fix.contains("nyl status"), "{}", refusal.fix);
        let accept = RecoverAction::Accept { run: run_id(2) };
        assert_eq!(
            decide_for(&current, &accept, &[]).unwrap_err().code,
            Code::RecoverNothing
        );
        let unknown = decide(&dev(), &database(), None, Some(&current), &RecoverAction::Retry, &[]);
        assert_eq!(unknown.unwrap_err().code, Code::UnitUnknown);
    }

    /// Catches a condition of a former incarnation being recovered onto the
    /// current one: it does not apply to the new uid.
    #[test]
    fn test_condition_of_another_incarnation_is_not_recoverable() {
        let former = uncertain(observed_named("database", 9, Some(key('a'))));
        assert_eq!(
            decide_for(&former, &RecoverAction::Retry, &[]).unwrap_err().code,
            Code::RecoverNothing
        );
    }

    /// Catches `--accept` not adopting a late checkpoint that matches what
    /// the unit runs now, or losing its artifacts.
    #[test]
    fn test_accept_adopts_a_matching_checkpoint_with_its_artifacts() {
        let current = uncertain(observed_named("database", 1, Some(key('a'))));
        let late = run_ref(2, 1, 'b', observed_named("database", 1, Some(key('b'))));
        let recovery = decide_for(
            &current,
            &RecoverAction::Accept { run: run_id(2) },
            std::slice::from_ref(&late),
        )
        .unwrap();
        assert_eq!(recovery.observed.condition, None);
        assert_eq!(recovery.observed.receipt, late.checkpoints[0].observed.receipt);
        assert_eq!(recovery.artifacts, Some(late.checkpoints[0].artifacts.clone()));
    }

    /// Catches `--accept` adopting a receipt for other inputs or another
    /// incarnation, or a run without a receipt of the unit.
    #[test]
    fn test_accept_refuses_a_mismatched_key_and_a_missing_or_failed_checkpoint() {
        let current = uncertain(observed_named("database", 1, Some(key('a'))));
        let accept = RecoverAction::Accept { run: run_id(2) };
        let other_key = run_ref(2, 1, 'c', observed_named("database", 1, Some(key('c'))));
        let refusal = decide_for(&current, &accept, &[other_key]).unwrap_err();
        assert_eq!(refusal.code, Code::RecoverKeyMismatch);
        assert!(
            refusal.message.contains(key('c').as_str()) && refusal.message.contains(key('b').as_str()),
            "{}",
            refusal.message
        );
        let other_uid = run_ref(2, 5, 'b', observed_named("database", 5, Some(key('b'))));
        assert_eq!(
            decide_for(&current, &accept, &[other_uid]).unwrap_err().code,
            Code::RecoverKeyMismatch
        );
        assert_eq!(
            decide_for(&current, &accept, &[]).unwrap_err().code,
            Code::RecoverCheckpointMissing
        );
        let failure = run_ref(2, 1, 'b', failed(observed_named("database", 1, None), true));
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
        let current = uncertain(observed_named("database", 1, Some(key('a'))));
        let older = run_ref(2, 1, 'b', failed(observed_named("database", 1, None), true));
        let newer = run_ref(2, 1, 'b', observed_named("database", 1, Some(key('b'))));
        let accept = RecoverAction::Accept { run: run_id(2) };
        assert!(decide_for(&current, &accept, &[older, newer]).is_ok());
    }

    /// Catches an operator seeing checkpoints out of order or without
    /// whether `--accept` would take them.
    #[test]
    fn test_evidence_lists_checkpoints_newest_first_with_acceptability() {
        let runs = [
            run_ref(2, 1, 'c', observed_named("database", 1, Some(key('c')))),
            run_ref(3, 1, 'b', observed_named("database", 1, Some(key('b')))),
            run_ref(4, 1, 'b', uncertain(observed_named("database", 1, None))),
        ];
        let desired = desired_named("database", 1, Some(key('b')));
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
