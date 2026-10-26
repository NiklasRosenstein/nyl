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
#[allow(clippy::large_enum_variant)] // Short-lived values, built and applied at once.
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
            // Teardown waits are recorded by the lifecycle shell, not by
            // these events (package `lifecycle` adds teardown events).
            waiting: None,
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
    use crate::rules::fixtures;

    fn identity() -> UnitIdentity {
        fixtures::observed("database", "Command", None).unit
    }

    fn run() -> RunId {
        fixtures::run(fixtures::RUN_1)
    }

    fn receipt(key: char) -> Receipt {
        fixtures::receipt(&key.to_string(), json!({}), Vec::new())
    }

    /// Catches a late checkpoint clearing the `uncertain` condition it
    /// arrived after; the property test below covers every other event.
    #[test]
    fn test_apply_keeps_a_late_checkpoint_as_evidence_of_an_uncertain_unit() {
        let current = document(Some('a'), 2);
        let checkpointed = document(Some('b'), 0);
        assert_eq!(
            apply(&identity(), Some(&current), Event::Imported { checkpointed }),
            Ok(Applied::Evidence)
        );
    }

    /// One generated event, with receipts drawn from a small set of keys.
    fn event(choice: u8, key: char, checkpointed: Option<(Option<char>, u8)>) -> Event {
        let since: Timestamp = "2026-09-24T12:00:00Z".parse().unwrap();
        match choice % 6 {
            0 => Event::Succeeded { receipt: receipt(key) },
            1 => Event::Failed {
                since,
                run: run(),
                category: FailureCategory::ToolError,
                retryable: key == 'a',
                message: "exit 1".to_owned(),
            },
            2 => Event::Uncertain {
                since,
                run: run(),
                reason: "run lost its lease".to_owned(),
            },
            3 => {
                let (receipt_key, condition) = checkpointed.unwrap_or((Some(key), 0));
                Event::Imported {
                    checkpointed: document(receipt_key, condition),
                }
            }
            4 => Event::RecoverRetry,
            _ => Event::RecoverAccept { receipt: receipt(key) },
        }
    }

    /// An observed document with an optional receipt and condition
    /// (0: none, 1: failed, 2: uncertain).
    fn document(receipt_key: Option<char>, condition: u8) -> ObservedUnit {
        let since: Timestamp = "2026-09-24T11:00:00Z".parse().unwrap();
        ObservedUnit {
            api_version: StateV1,
            kind: ObservedUnitKind,
            unit: identity(),
            receipt: receipt_key.map(receipt),
            condition: match condition % 3 {
                0 => None,
                1 => Some(Condition::Failed {
                    since,
                    run: run(),
                    category: FailureCategory::ToolError,
                    retryable: false,
                    message: "exit 1".to_owned(),
                }),
                _ => Some(Condition::Uncertain {
                    since,
                    run: run(),
                    reason: "interrupted".to_owned(),
                }),
            },
            waiting: None,
        }
    }

    fn key_char() -> impl proptest::strategy::Strategy<Value = char> {
        proptest::sample::select(vec!['a', 'b', 'c'])
    }

    proptest::proptest! {
        /// Catches a transition that loses the last receipt, clears an
        /// uncertain condition without an execution or an operator (an
        /// import never does), lets a retry change the receipt, or lets
        /// `recover` act on a unit without a condition.
        #[test]
        fn test_apply_preserves_receipts_and_conditions_over_event_sequences(
            start in proptest::option::of((proptest::option::of(key_char()), 0u8..3)),
            events in proptest::collection::vec(
                (0u8..6, key_char(), proptest::option::of((proptest::option::of(key_char()), 0u8..3))),
                1..12,
            ),
        ) {
            let mut current = start.map(|(key, condition)| document(key, condition));
            for (choice, key, checkpointed) in events {
                let event = event(choice, key, checkpointed);
                let condition = current.as_ref().and_then(|unit| unit.condition.clone());
                let was_uncertain = matches!(condition, Some(Condition::Uncertain { .. }));
                let receipt_before = current.as_ref().and_then(|unit| unit.receipt.clone());
                let replaces = matches!(
                    event,
                    Event::Succeeded { .. } | Event::RecoverAccept { .. } | Event::Imported { .. }
                );
                let clears = matches!(
                    event,
                    Event::Succeeded { .. } | Event::RecoverRetry | Event::RecoverAccept { .. }
                );
                let recover = matches!(event, Event::RecoverRetry | Event::RecoverAccept { .. });
                let imported = matches!(event, Event::Imported { .. });
                let retry = matches!(event, Event::RecoverRetry);
                match apply(&identity(), current.as_ref(), event) {
                    Err(Refusal::NothingToRecover) => {
                        proptest::prop_assert!(recover && condition.is_none());
                    }
                    Ok(Applied::Evidence) => {
                        proptest::prop_assert!(imported && was_uncertain);
                    }
                    Ok(Applied::Changed(after)) => {
                        proptest::prop_assert!(!(recover && condition.is_none()), "recover applied without a condition");
                        proptest::prop_assert!(!(imported && was_uncertain), "an import cleared an uncertain condition");
                        if receipt_before.is_some() {
                            proptest::prop_assert!(after.receipt.is_some(), "the last receipt was removed");
                        }
                        if !replaces {
                            proptest::prop_assert_eq!(&after.receipt, &receipt_before);
                        }
                        if retry {
                            proptest::prop_assert_eq!(&after.condition, &None);
                        }
                        if was_uncertain && after.condition.is_none() {
                            proptest::prop_assert!(clears, "an uncertain condition was cleared by another event");
                        }
                        current = Some(*after);
                    }
                }
            }
        }
    }
}
