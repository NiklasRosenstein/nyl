//! The transition table of a unit's observed state: its latest receipt and
//! its condition.
//!
//! Contract: [Observed unit](../../../../design/orchestration-core.md#observed-unit),
//! [Recovery](../../../../design/orchestration-core.md#recovery), and
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (takeover and late checkpoints).
//!
//! Every change of a unit's receipt or condition goes through [`apply`]:
//! executions, `inspect`, imported checkpoints, takeover, and `nyl recover`.
//! No other module builds an observed document with a changed condition.
//! Lifecycle transitions (teardown, holds) join this table in part 5.
//!
//! - A condition records the latest failure or uncertainty and stays until
//!   the unit next succeeds or an operator runs `recover`; it never removes
//!   the last receipt.
//! - A checkpoint imported from a leftover run brings its result, unless the
//!   unit is already `uncertain`: a late checkpoint never clears that
//!   condition by itself and stays evidence for `recover --accept`.
//! - `recover --retry` clears a condition and records nothing else;
//!   `recover --accept` replaces it with the accepted receipt. Both refuse a
//!   unit without a condition.

use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::state::{
    Condition, FailureCategory, ObservedUnit, ObservedUnitKind, Receipt, StateV1, UnitIdentity,
};
use nyl_core::orchestration::time::Timestamp;

/// What happened to a unit.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// An execution succeeded, or `inspect` found its effects applied: the
    /// receipt replaces the last one and clears any condition.
    Succeeded { receipt: Receipt },
    /// An execution failed without effects it did not report.
    Failed {
        since: Timestamp,
        run: RunId,
        category: FailureCategory,
        retryable: bool,
        message: String,
    },
    /// Effects may or may not have happened: an uncertain outcome, a driver
    /// error after effects may have started, a unit a dead run was executing
    /// without a checkpoint, or a superseded result of a unit whose recovery
    /// policy is not `converge`.
    Uncertain {
        since: Timestamp,
        run: RunId,
        reason: String,
    },
    /// A leftover run's checkpoint whose execution key equals the unit's
    /// current desired document's: its receipt or condition, as checkpointed.
    Imported { checkpointed: ObservedUnit },
    /// `nyl recover --retry`.
    RecoverRetry,
    /// `nyl recover --accept <run>`, with the receipt that run checkpointed.
    RecoverAccept { receipt: Receipt },
}

/// The result of an accepted event.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The unit's new observed document.
    Changed(Box<ObservedUnit>),
    /// Nothing changes: the event is evidence only, such as a late
    /// checkpoint of an `uncertain` unit.
    Evidence,
}

/// Why an event does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `recover` names a unit without a condition.
    NothingToRecover,
}

/// Apply `event` to a unit whose observed document of the same incarnation
/// is `current` (`None` when it has none yet); `unit` names the incarnation.
pub fn apply(unit: &UnitIdentity, current: Option<&ObservedUnit>, event: Event) -> Result<Applied, Refusal> {
    let receipt = current.and_then(|observed| observed.receipt.clone());
    let condition = current.and_then(|observed| observed.condition.as_ref());
    let document = |receipt: Option<Receipt>, condition: Option<Condition>| {
        Applied::Changed(Box::new(ObservedUnit {
            api_version: StateV1,
            kind: ObservedUnitKind,
            unit: unit.clone(),
            receipt,
            condition,
        }))
    };
    if matches!(event, Event::RecoverRetry | Event::RecoverAccept { .. }) && condition.is_none() {
        return Err(Refusal::NothingToRecover);
    }
    Ok(match event {
        Event::Succeeded { receipt } | Event::RecoverAccept { receipt } => document(Some(receipt), None),
        Event::Failed {
            since,
            run,
            category,
            retryable,
            message,
        } => document(
            receipt,
            Some(Condition::Failed {
                since,
                run,
                category,
                retryable,
                message,
            }),
        ),
        Event::Uncertain { since, run, reason } => document(receipt, Some(Condition::Uncertain { since, run, reason })),
        Event::Imported { .. } if matches!(condition, Some(Condition::Uncertain { .. })) => Applied::Evidence,
        Event::Imported { checkpointed } => document(checkpointed.receipt.or(receipt), checkpointed.condition),
        Event::RecoverRetry => document(receipt, None),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn identity() -> UnitIdentity {
        serde_json::from_value(json!({
            "environment": "dev", "name": "database", "uid": "0192f0c4-8d0a-7c3e-9a51-000000000001",
            "apiVersion": "units.gitops.nyl/v1", "kind": "Command"
        }))
        .unwrap()
    }

    fn run() -> RunId {
        RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap()
    }

    fn receipt(key: char) -> Receipt {
        serde_json::from_value(json!({
            "executionKey": format!("sha256:{}", key.to_string().repeat(64)),
            "behaviorVersion": 1,
            "run": run(),
            "startedAt": "2026-09-24T10:00:00Z",
            "finishedAt": "2026-09-24T10:07:12Z",
        }))
        .unwrap()
    }

    fn uncertain(receipt: Option<Receipt>) -> ObservedUnit {
        ObservedUnit {
            api_version: StateV1,
            kind: ObservedUnitKind,
            unit: identity(),
            receipt,
            condition: Some(Condition::Uncertain {
                since: "2026-09-24T11:00:00Z".parse().unwrap(),
                run: run(),
                reason: "run lost its lease".to_owned(),
            }),
        }
    }

    #[test]
    fn test_apply_keeps_a_late_checkpoint_as_evidence_of_an_uncertain_unit() {
        let current = uncertain(Some(receipt('a')));
        let checkpointed = ObservedUnit {
            condition: None,
            receipt: Some(receipt('b')),
            ..current.clone()
        };
        assert_eq!(
            apply(&identity(), Some(&current), Event::Imported { checkpointed }),
            Ok(Applied::Evidence)
        );
    }

    #[test]
    fn test_apply_recover_clears_a_condition_and_refuses_without_one() {
        let current = uncertain(Some(receipt('a')));
        let Ok(Applied::Changed(retried)) = apply(&identity(), Some(&current), Event::RecoverRetry) else {
            panic!("retry applies to an uncertain unit");
        };
        assert_eq!(retried.condition, None);
        assert_eq!(retried.receipt, Some(receipt('a')), "a retry keeps the last receipt");
        assert_eq!(
            apply(&identity(), Some(&retried), Event::RecoverRetry),
            Err(Refusal::NothingToRecover)
        );
        assert_eq!(
            apply(&identity(), None, Event::RecoverAccept { receipt: receipt('b') }),
            Err(Refusal::NothingToRecover)
        );
    }

    #[test]
    fn test_apply_failure_keeps_the_last_receipt() {
        let current = ObservedUnit {
            condition: None,
            ..uncertain(Some(receipt('a')))
        };
        let event = Event::Failed {
            since: "2026-09-24T11:00:00Z".parse().unwrap(),
            run: run(),
            category: FailureCategory::ToolError,
            retryable: false,
            message: "exit 1".to_owned(),
        };
        let Ok(Applied::Changed(failed)) = apply(&identity(), Some(&current), event) else {
            panic!("a failure applies");
        };
        assert_eq!(failed.receipt, Some(receipt('a')));
        assert!(matches!(failed.condition, Some(Condition::Failed { .. })));
    }
}
