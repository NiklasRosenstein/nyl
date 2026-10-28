//! The transition table of a unit's observed state: its latest receipt and
//! its condition.
//!
//! Contract: [Observed unit](../../../../design/orchestration-core.md#observed-unit),
//! [Recovery](../../../../design/orchestration-core.md#recovery), and
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (takeover and late checkpoints).
//!
//! Every change of a unit's receipt or condition goes through [`apply_to`]
//! (or [`apply`] where only the unit's identity is at hand): executions,
//! `inspect`, imported checkpoints, takeover, teardowns, and `nyl recover`.
//! No other module builds an observed document with a changed condition.
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
//! - Every new receipt carries a copy of the desired document it was
//!   produced for (`desired`), written in the same document, so `nyl hold`
//!   reverts to it without reading history (part 5 decision Q16): the
//!   document [`apply_to`] is given, or the copy an imported checkpoint
//!   carries. An event that writes no receipt keeps the existing copy.
//! - A publication teardown that published its first phase and waits
//!   records the wait ([`Event::Waiting`]) next to the receipt it keeps;
//!   conditions keep a recorded wait, and a new receipt ends it.

use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::state::{
    Condition, DesiredUnit, FailureCategory, ObservedUnit, ObservedUnitKind, OperatorModification, Receipt,
    TeardownWaiting, UnitIdentity,
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
    /// A held native tool may have changed effects outside reconciliation.
    OperatorModified { modification: OperatorModification },
    /// Recovery accepted an empty native backend for one recorded ownership scope.
    AcceptEmptyState {
        scope: nyl_core::orchestration::driver::OwnershipScope,
    },
    /// `nyl recover --accept <run>`, with the receipt that run checkpointed.
    RecoverAccept { receipt: Receipt },
    /// A two-phase teardown published its first phase and waits under its
    /// `teardownWait`: the wait is recorded, the receipt and any condition stay.
    Waiting { waiting: TeardownWaiting },
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
/// is `current` (`None` when it has none yet); `desired` is the document the
/// event is about, whose copy a new receipt carries.
pub fn apply_to(desired: &DesiredUnit, current: Option<&ObservedUnit>, event: Event) -> Result<Applied, Refusal> {
    transition(&desired.unit, current, event, Some(desired))
}

/// [`apply_to`] for a caller that has only the unit's identity: a new
/// receipt keeps the existing desired copy (or the copy an imported
/// checkpoint carries), since there is no document to copy.
pub fn apply(unit: &UnitIdentity, current: Option<&ObservedUnit>, event: Event) -> Result<Applied, Refusal> {
    transition(unit, current, event, None)
}

fn transition(
    unit: &UnitIdentity,
    current: Option<&ObservedUnit>,
    event: Event,
    produced_for: Option<&DesiredUnit>,
) -> Result<Applied, Refusal> {
    let receipt = current.and_then(|observed| observed.receipt.clone());
    let succeeded = matches!(&event, Event::Succeeded { .. });
    let condition = current.and_then(|observed| observed.condition.as_ref());
    let existing_copy = || current.and_then(|observed| observed.desired.clone());
    let existing_wait = || current.and_then(|observed| observed.waiting.clone());
    // The copy of the document a new receipt was produced for.
    let new_copy = || {
        produced_for
            .map(|desired| Box::new(desired.clone()))
            .or_else(existing_copy)
    };
    let document = |receipt: Option<Receipt>,
                    condition: Option<Condition>,
                    desired: Option<Box<DesiredUnit>>,
                    waiting: Option<TeardownWaiting>| {
        Applied::Changed(Box::new(ObservedUnit {
            api_version: nyl_core::orchestration::state::ObservedV2,
            operator_modified: if succeeded {
                None
            } else {
                current.and_then(|observed| observed.operator_modified.clone())
            },
            accepted_empty_state: if succeeded {
                None
            } else {
                current.and_then(|observed| observed.accepted_empty_state.clone())
            },
            kind: ObservedUnitKind,
            unit: unit.clone(),
            // A document without a receipt has no copy.
            desired: receipt.as_ref().and(desired),
            receipt,
            condition,
            waiting,
        }))
    };
    if matches!(event, Event::RecoverRetry | Event::RecoverAccept { .. }) && condition.is_none() {
        return Err(Refusal::NothingToRecover);
    }
    Ok(match event {
        Event::Succeeded { receipt } | Event::RecoverAccept { receipt } => {
            document(Some(receipt), None, new_copy(), None)
        }
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
            existing_copy(),
            existing_wait(),
        ),
        Event::Uncertain { since, run, reason } => document(
            receipt,
            Some(Condition::Uncertain { since, run, reason }),
            existing_copy(),
            existing_wait(),
        ),
        Event::Imported { .. } if matches!(condition, Some(Condition::Uncertain { .. })) => Applied::Evidence,
        Event::Imported { checkpointed } => match checkpointed.receipt {
            Some(imported) => {
                let copy = checkpointed.desired.or_else(new_copy);
                document(Some(imported), checkpointed.condition, copy, checkpointed.waiting)
            }
            None => document(
                receipt,
                checkpointed.condition,
                existing_copy(),
                checkpointed.waiting.or_else(existing_wait),
            ),
        },
        Event::RecoverRetry => document(receipt, None, existing_copy(), existing_wait()),
        Event::OperatorModified { modification } => {
            let mut changed = match document(receipt, condition.cloned(), existing_copy(), existing_wait()) {
                Applied::Changed(changed) => changed,
                Applied::Evidence => unreachable!("document changes observed state"),
            };
            changed.operator_modified = Some(modification);
            Applied::Changed(changed)
        }
        Event::AcceptEmptyState { scope } => {
            let mut changed = match document(receipt, condition.cloned(), existing_copy(), existing_wait()) {
                Applied::Changed(changed) => changed,
                Applied::Evidence => unreachable!("document changes observed state"),
            };
            changed.accepted_empty_state = Some(scope);
            Applied::Changed(changed)
        }
        Event::Waiting { waiting } => document(receipt, condition.cloned(), existing_copy(), Some(waiting)),
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

    /// Catches a receipt written without the document it was produced for,
    /// which `nyl hold` reverts to (decision Q16): an execution's receipt
    /// carries a copy of the executed document.
    #[test]
    fn test_a_new_receipt_carries_a_copy_of_its_desired_document() {
        let executed = fixtures::desired("database", "Command", json!({"command": ["./db.sh"]}), "b");
        let older = fixtures::desired("database", "Command", json!({"command": ["./old.sh"]}), "a");
        let mut current = document(Some('a'), 0);
        current.desired = Some(Box::new(older));
        let Ok(Applied::Changed(after)) =
            apply_to(&executed, Some(&current), Event::Succeeded { receipt: receipt('b') })
        else {
            panic!("a success changes the observed document");
        };
        assert_eq!(after.desired.as_deref(), Some(&executed));
    }

    /// Catches a failure or uncertainty replacing the copy of the document
    /// the kept receipt was produced for with the failed document, so a
    /// hold would revert to something that never ran.
    #[test]
    fn test_a_condition_keeps_the_copy_of_the_kept_receipt() {
        let older = fixtures::desired("database", "Command", json!({"command": ["./old.sh"]}), "a");
        let failing = fixtures::desired("database", "Command", json!({"command": ["./db.sh"]}), "b");
        let mut current = document(Some('a'), 0);
        current.desired = Some(Box::new(older.clone()));
        let since: Timestamp = "2026-09-24T12:00:00Z".parse().unwrap();
        let event = Event::Uncertain {
            since,
            run: run(),
            reason: "interrupted".to_owned(),
        };
        let Ok(Applied::Changed(after)) = apply_to(&failing, Some(&current), event) else {
            panic!("an uncertain outcome changes the observed document");
        };
        assert_eq!(after.desired.as_deref(), Some(&older));
        assert_eq!(after.receipt, current.receipt);
    }

    /// Catches a teardown wait losing the receipt (whose outputs the second
    /// phase reads) or a later condition dropping the recorded wait.
    #[test]
    fn test_a_teardown_wait_keeps_the_receipt_and_survives_a_condition() {
        let unit = fixtures::desired("database", "Command", json!({}), "a");
        let since: Timestamp = "2026-09-24T12:00:00Z".parse().unwrap();
        let waiting = TeardownWaiting {
            phase: "applications-removed".to_owned(),
            reason: "waiting".to_owned(),
            since,
            until: None,
            phase1_commit: None,
            run: run(),
        };
        let current = document(Some('a'), 0);
        let Ok(Applied::Changed(waited)) = apply_to(
            &unit,
            Some(&current),
            Event::Waiting {
                waiting: waiting.clone(),
            },
        ) else {
            panic!("a wait changes the observed document");
        };
        assert_eq!(waited.receipt, current.receipt);
        assert_eq!(waited.waiting.as_ref(), Some(&waiting));
        let failed = Event::Uncertain {
            since,
            run: run(),
            reason: "the wait could not observe Argo CD".to_owned(),
        };
        let Ok(Applied::Changed(after)) = apply_to(&unit, Some(&waited), failed) else {
            panic!("an uncertain outcome changes the observed document");
        };
        assert_eq!(after.waiting.as_ref(), Some(&waiting));
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
            api_version: nyl_core::orchestration::state::ObservedV2,
            operator_modified: None,
            accepted_empty_state: None,
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
            desired: None,

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
