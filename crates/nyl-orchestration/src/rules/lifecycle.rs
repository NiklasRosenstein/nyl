//! Pure lifecycle transitions for deletion, retention, release, and holds.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion) and
//! [Instances](../../../../design/orchestration-core.md#instances). All lifecycle
//! changes go through [`transition`]; callers move documents according to [`Next`].

use nyl_core::orchestration::state::{
    Deletion, DeletionIntent, DeletionReason, Hold, Lifecycle, Request, TeardownProgress, Tombstone,
};

/// What happened to a unit's lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The unit left the ownership set (`omission`, `expired`) or its kind
    /// changed (`kind-change`), with the intent of its deletion policy.
    Left {
        reason: DeletionReason,
        intent: DeletionIntent,
    },
    /// The unit is in the ownership set again, with the same name and kind.
    Returned,
    /// `nyl teardown --unit` (`teardown` outside the ownership set, `replace`
    /// inside it) or `teardown --all` and `state delete --teardown`
    /// (`decommission`).
    TeardownRequested { reason: DeletionReason, requested: Request },
    /// The driver's teardown started.
    TeardownStarted,
    /// The teardown completed; `hold` is applied afterwards (`teardown
    /// --hold`, or `--all` for a unit in the ownership set).
    TornDown { hold: Option<Hold> },
    /// A declared `Retain` unit leaves its files for a tombstone.
    Retain { tombstone: Tombstone },
    /// A kind that cannot tear down, or a `Retain` unit under `teardown
    /// --all`, leaves its resources in place; `hold` as for `TornDown`.
    Released { hold: Option<Hold> },
    /// A unit without an incarnation (no uid) is outside the ownership set,
    /// such as a uid-less held file after `resume`: nothing to delete.
    LeftWithoutIncarnation,
    /// `nyl hold`.
    Hold { hold: Hold },
    /// `nyl resume`.
    Resume,
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Self::Left { .. } => "left",
            Self::Returned => "returned",
            Self::TeardownRequested { .. } => "teardown-requested",
            Self::TeardownStarted => "teardown-started",
            Self::TornDown { .. } => "torn-down",
            Self::Retain { .. } => "retain",
            Self::Released { .. } => "released",
            Self::LeftWithoutIncarnation => "left-without-incarnation",
            Self::Hold { .. } => "hold",
            Self::Resume => "resume",
        }
    }
}

/// The result of an accepted event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The desired file stays, with this lifecycle.
    Lifecycle(Lifecycle),
    /// The desired file stays with this lifecycle, but the incarnation ends:
    /// the caller drops its uid, receipt, and observed file.
    EndIncarnation(Lifecycle),
    /// The desired file stays with this lifecycle, and the caller writes the
    /// tombstone's receipt back as the unit's observed file (of the same
    /// uid); the artifact files stayed with the tombstone. Only a retained
    /// tombstone's explicit teardown gives it.
    RestoreReceipt(Lifecycle),
    /// The unit's desired and observed files are removed.
    Remove,
    /// Nothing changes.
    Unchanged,
}

/// Why an event does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `nyl hold` of a `retained` tombstone, which nothing reconciles.
    HoldRetained,
    /// An event that has no row for the current state, such as a teardown
    /// starting for an active unit.
    NotApplicable { state: &'static str, event: &'static str },
}

/// The name of a lifecycle state, as the `state` field writes it.
pub fn state_name(lifecycle: &Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Active => "active",
        Lifecycle::Deleting { .. } => "deleting",
        Lifecycle::Held { .. } => "held",
        Lifecycle::Retained { .. } => "retained",
    }
}

/// The end of a teardown or release: the files go, or, with a hold to apply
/// afterwards, the file stays held without an incarnation.
fn then_hold(hold: Option<Hold>) -> Next {
    match hold {
        None => Next::Remove,
        Some(hold) => Next::EndIncarnation(Lifecycle::Held { hold, deletion: None }),
    }
}

/// Apply `event` to a unit with no desired document in state: only `nyl
/// hold` applies, and the caller writes a held marker
/// ([`nyl_core::orchestration::state::DesiredUnit::held_marker`]).
pub fn transition_absent(event: Event) -> Result<Next, Refusal> {
    match event {
        Event::Hold { hold } => Ok(Next::Lifecycle(Lifecycle::Held { hold, deletion: None })),
        event => Err(Refusal::NotApplicable {
            state: "absent",
            event: event.name(),
        }),
    }
}

/// Apply `event` to a unit whose lifecycle is `current`.
#[allow(clippy::too_many_lines)] // One table: each arm is one contract row.
#[allow(clippy::match_same_arms)] // Rows stay one per contract rule, even with equal results.
pub fn transition(current: &Lifecycle, event: Event) -> Result<Next, Refusal> {
    let refused = |event: &Event| Refusal::NotApplicable {
        state: state_name(current),
        event: event.name(),
    };
    let next = match (current, event) {
        // Leaving the ownership set.
        (Lifecycle::Active, Event::Left { reason, intent }) => Next::Lifecycle(Lifecycle::Deleting {
            deletion: Deletion {
                hold: false,
                reason,
                intent,
                teardown: (intent == DeletionIntent::Teardown).then_some(TeardownProgress::Pending),
                requested: None,
            },
        }),
        (
            Lifecycle::Retained { .. },
            Event::Left {
                reason: DeletionReason::KindChange,
                ..
            },
        ) => Next::Remove,
        (Lifecycle::Deleting { .. } | Lifecycle::Retained { .. } | Lifecycle::Held { .. }, Event::Left { .. }) => {
            Next::Unchanged
        }

        // Returning to the ownership set.
        (Lifecycle::Deleting { deletion }, Event::Returned) => {
            let undone_by_source = matches!(deletion.reason, DeletionReason::Omission | DeletionReason::KindChange);
            let started = deletion.teardown == Some(TeardownProgress::InProgress);
            if undone_by_source && !started {
                Next::Lifecycle(Lifecycle::Active)
            } else {
                Next::Unchanged
            }
        }
        (Lifecycle::Retained { .. }, Event::Returned) => Next::Lifecycle(Lifecycle::Active),
        (Lifecycle::Active | Lifecycle::Held { .. }, Event::Returned) => Next::Unchanged,

        // Retention and release.
        (Lifecycle::Active, Event::Retain { tombstone }) => Next::Lifecycle(Lifecycle::Retained { tombstone }),
        (Lifecycle::Deleting { deletion }, Event::Retain { tombstone })
            if deletion.intent == DeletionIntent::Retain =>
        {
            Next::Lifecycle(Lifecycle::Retained { tombstone })
        }
        (Lifecycle::Active | Lifecycle::Deleting { .. }, Event::Released { hold }) => then_hold(hold),
        (Lifecycle::Retained { .. }, Event::Released { hold }) => then_hold(hold),
        (Lifecycle::Active | Lifecycle::Deleting { .. }, Event::LeftWithoutIncarnation) => Next::Remove,
        (Lifecycle::Held { .. }, Event::LeftWithoutIncarnation) => Next::Unchanged,

        // Explicit teardown.
        (Lifecycle::Active, Event::TeardownRequested { reason, requested }) => Next::Lifecycle(Lifecycle::Deleting {
            deletion: Deletion {
                hold: false,
                reason,
                intent: DeletionIntent::Teardown,
                teardown: Some(TeardownProgress::Pending),
                requested: Some(requested),
            },
        }),
        (Lifecycle::Retained { .. }, Event::TeardownRequested { reason, requested }) => {
            Next::RestoreReceipt(Lifecycle::Deleting {
                deletion: Deletion {
                    hold: false,
                    reason,
                    intent: DeletionIntent::Teardown,
                    teardown: Some(TeardownProgress::Pending),
                    requested: Some(requested),
                },
            })
        }
        (Lifecycle::Deleting { deletion }, Event::TeardownRequested { reason, requested }) => {
            Next::Lifecycle(Lifecycle::Deleting {
                deletion: Deletion {
                    hold: false,
                    reason,
                    intent: DeletionIntent::Teardown,
                    teardown: Some(deletion.teardown.unwrap_or(TeardownProgress::Pending)),
                    requested: Some(requested),
                },
            })
        }
        (Lifecycle::Held { hold, deletion }, Event::TeardownRequested { reason, requested }) => {
            Next::Lifecycle(Lifecycle::Held {
                hold: hold.clone(),
                deletion: Some(Deletion {
                    hold: false,
                    reason,
                    intent: DeletionIntent::Teardown,
                    teardown: Some(
                        deletion
                            .as_ref()
                            .and_then(|deletion| deletion.teardown)
                            .unwrap_or(TeardownProgress::Pending),
                    ),
                    requested: Some(requested),
                }),
            })
        }

        // Teardown progress.
        (Lifecycle::Deleting { deletion }, Event::TeardownStarted) if deletion.intent == DeletionIntent::Teardown => {
            Next::Lifecycle(Lifecycle::Deleting {
                deletion: Deletion {
                    hold: false,
                    teardown: Some(TeardownProgress::InProgress),
                    ..deletion.clone()
                },
            })
        }
        (Lifecycle::Deleting { deletion }, Event::TornDown { hold }) if deletion.intent == DeletionIntent::Teardown => {
            then_hold(hold)
        }

        // Holds.
        (Lifecycle::Active, Event::Hold { hold }) => Next::Lifecycle(Lifecycle::Held { hold, deletion: None }),
        (Lifecycle::Deleting { deletion }, Event::Hold { hold }) => Next::Lifecycle(Lifecycle::Held {
            hold,
            deletion: Some(deletion.clone()),
        }),
        (Lifecycle::Held { .. }, Event::Hold { .. }) => Next::Unchanged,
        (Lifecycle::Retained { .. }, Event::Hold { .. }) => return Err(Refusal::HoldRetained),
        (Lifecycle::Held { deletion, .. }, Event::Resume) => Next::Lifecycle(match deletion {
            None => Lifecycle::Active,
            Some(deletion) => Lifecycle::Deleting {
                deletion: deletion.clone(),
            },
        }),
        (Lifecycle::Active | Lifecycle::Deleting { .. } | Lifecycle::Retained { .. }, Event::Resume) => Next::Unchanged,

        (_, event) => return Err(refused(&event)),
    };
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tombstone() -> Tombstone {
        serde_json::from_value(serde_json::json!({"since": "2026-09-24T10:00:00Z"})).unwrap()
    }

    fn request() -> Request {
        serde_json::from_value(serde_json::json!({"by": "alice", "at": "2026-09-24T10:00:00Z", "reason": "done"}))
            .unwrap()
    }

    fn hold() -> Hold {
        serde_json::from_value(
            serde_json::json!({"since": "2026-09-24T12:05:00Z", "by": "alice", "reason": "incident"}),
        )
        .unwrap()
    }

    fn deleting(reason: DeletionReason, intent: DeletionIntent, teardown: Option<TeardownProgress>) -> Lifecycle {
        Lifecycle::Deleting {
            deletion: Deletion {
                hold: false,
                reason,
                intent,
                teardown,
                requested: None,
            },
        }
    }

    fn pending_teardown(reason: DeletionReason) -> Lifecycle {
        deleting(reason, DeletionIntent::Teardown, Some(TeardownProgress::Pending))
    }

    fn requested(reason: DeletionReason) -> Event {
        Event::TeardownRequested {
            reason,
            requested: request(),
        }
    }

    /// Deletion, "Explicit teardown": a tombstone's teardown keeps its
    /// receipt as the observed file, so a teardown that does not finish in
    /// this run still has the outputs it reads.
    #[test]
    fn test_teardown_of_a_tombstone_restores_its_receipt() {
        let retained = Lifecycle::Retained { tombstone: tombstone() };
        let next = transition(&retained, requested(DeletionReason::Teardown)).unwrap();
        assert!(matches!(
            next,
            Next::RestoreReceipt(Lifecycle::Deleting { ref deletion })
                if deletion.teardown == Some(TeardownProgress::Pending)
        ));
    }

    /// Instances, `teardown --all`: a tombstone whose kind cannot tear down is
    /// released; its files go and its resources stay.
    #[test]
    fn test_a_released_tombstone_is_removed() {
        let retained = Lifecycle::Retained { tombstone: tombstone() };
        assert_eq!(transition(&retained, Event::Released { hold: None }), Ok(Next::Remove));
    }

    /// "Tear down, then hold" (`teardown --hold`, `--all` in the ownership
    /// set): catches the hold being lost, which would let the next run
    /// recreate the unit, or the file surviving with its old uid.
    #[test]
    fn test_tear_down_then_hold_ends_the_incarnation_held() {
        let in_progress = deleting(
            DeletionReason::Teardown,
            DeletionIntent::Teardown,
            Some(TeardownProgress::InProgress),
        );
        let held = Next::EndIncarnation(Lifecycle::Held {
            hold: hold(),
            deletion: None,
        });
        assert_eq!(
            transition(&in_progress, Event::TornDown { hold: Some(hold()) }),
            Ok(held.clone())
        );
        assert_eq!(
            transition(&in_progress, Event::TornDown { hold: None }),
            Ok(Next::Remove)
        );
        assert_eq!(
            transition(&Lifecycle::Active, Event::Released { hold: Some(hold()) }),
            Ok(held)
        );
    }

    /// Holds (Q26): a held unit never progresses. Catches a teardown of a
    /// held unit destroying anything; its request is recorded and carried
    /// out after `resume`.
    #[test]
    fn test_a_held_unit_never_progresses_and_records_a_teardown_request() {
        let held = Lifecycle::Held {
            hold: hold(),
            deletion: None,
        };
        for event in [
            Event::TeardownStarted,
            Event::TornDown { hold: None },
            Event::Released { hold: None },
        ] {
            assert!(
                matches!(transition(&held, event.clone()), Err(Refusal::NotApplicable { .. })),
                "{event:?}"
            );
        }
        let Ok(Next::Lifecycle(recorded)) = transition(&held, requested(DeletionReason::Replace)) else {
            panic!("a teardown request on a held unit is recorded");
        };
        let Lifecycle::Held {
            deletion: Some(ref deletion),
            ..
        } = recorded
        else {
            panic!("the request is recorded next to the hold: {recorded:?}");
        };
        assert_eq!(deletion.reason, DeletionReason::Replace);
        assert_eq!(deletion.teardown, Some(TeardownProgress::Pending));
        assert_eq!(deletion.requested, Some(request()));
        assert_eq!(
            transition(&recorded, Event::Resume),
            Ok(Next::Lifecycle(Lifecycle::Deleting {
                deletion: deletion.clone()
            }))
        );
    }

    /// Q26: a uid-less file left outside the ownership set after `resume`
    /// is removed, but a held one stays until its hold is lifted.
    #[test]
    fn test_left_without_incarnation_removes_only_unheld_files() {
        assert_eq!(
            transition(&Lifecycle::Active, Event::LeftWithoutIncarnation),
            Ok(Next::Remove)
        );
        let held = Lifecycle::Held {
            hold: hold(),
            deletion: None,
        };
        assert_eq!(transition(&held, Event::LeftWithoutIncarnation), Ok(Next::Unchanged));
    }

    /// Q19: only a hold applies to a unit without state, and it writes a
    /// held marker.
    #[test]
    fn test_only_a_hold_applies_to_a_unit_without_state() {
        assert_eq!(
            transition_absent(Event::Hold { hold: hold() }),
            Ok(Next::Lifecycle(Lifecycle::Held {
                hold: hold(),
                deletion: None
            }))
        );
        assert!(transition_absent(requested(DeletionReason::Teardown)).is_err());
        assert!(transition_absent(Event::Resume).is_err());
    }

    /// Fencing and "Explicit teardown": a kind change leaves the old
    /// incarnation pending teardown (which `--allow-teardown` gates), while
    /// an explicit request records who asked.
    #[test]
    fn test_kind_change_is_pending_and_explicit_teardown_records_its_request() {
        let left = transition(
            &Lifecycle::Active,
            Event::Left {
                reason: DeletionReason::KindChange,
                intent: DeletionIntent::Teardown,
            },
        );
        assert_eq!(left, Ok(Next::Lifecycle(pending_teardown(DeletionReason::KindChange))));
        let Ok(Next::Lifecycle(Lifecycle::Deleting { deletion })) =
            transition(&Lifecycle::Active, requested(DeletionReason::Replace))
        else {
            panic!("an explicit teardown makes the unit deleting");
        };
        assert_eq!(deletion.requested, Some(request()));
        assert_eq!(deletion.teardown, Some(TeardownProgress::Pending));
    }

    /// Omission: a unit restored before its teardown starts is active
    /// again; a started teardown or an explicit request is not undone by
    /// source.
    #[test]
    fn test_return_undoes_only_a_pending_teardown_caused_by_source() {
        let pending = pending_teardown(DeletionReason::Omission);
        assert_eq!(
            transition(&pending, Event::Returned),
            Ok(Next::Lifecycle(Lifecycle::Active))
        );
        let started = deleting(
            DeletionReason::Omission,
            DeletionIntent::Teardown,
            Some(TeardownProgress::InProgress),
        );
        assert_eq!(transition(&started, Event::Returned), Ok(Next::Unchanged));
        let explicit = pending_teardown(DeletionReason::Replace);
        assert_eq!(transition(&explicit, Event::Returned), Ok(Next::Unchanged));
        let retained = Lifecycle::Retained { tombstone: tombstone() };
        assert_eq!(
            transition(&retained, Event::Returned),
            Ok(Next::Lifecycle(Lifecycle::Active))
        );
    }

    /// A retained tombstone is reconciled by nothing, so holding it is refused.
    #[test]
    fn test_hold_of_a_tombstone_is_refused() {
        let retained = Lifecycle::Retained { tombstone: tombstone() };
        assert_eq!(
            transition(&retained, Event::Hold { hold: hold() }),
            Err(Refusal::HoldRetained)
        );
    }

    fn lifecycle_strategy() -> impl proptest::strategy::Strategy<Value = Lifecycle> {
        use proptest::prelude::*;
        let reason = proptest::sample::select(vec![
            DeletionReason::Omission,
            DeletionReason::Teardown,
            DeletionReason::Replace,
            DeletionReason::KindChange,
            DeletionReason::Decommission,
        ]);
        let progress = proptest::sample::select(vec![TeardownProgress::Pending, TeardownProgress::InProgress]);
        prop_oneof![
            Just(Lifecycle::Active),
            (reason.clone(), progress).prop_map(|(reason, progress)| deleting(
                reason,
                DeletionIntent::Teardown,
                Some(progress)
            )),
            reason
                .clone()
                .prop_map(|reason| deleting(reason, DeletionIntent::Retain, None)),
            Just(Lifecycle::Held {
                hold: hold(),
                deletion: None
            }),
            reason.prop_map(|reason| Lifecycle::Held {
                hold: hold(),
                deletion: Some(Deletion {
                    hold: false,
                    reason,
                    intent: DeletionIntent::Teardown,
                    teardown: Some(TeardownProgress::Pending),
                    requested: None,
                }),
            }),
            Just(Lifecycle::Retained { tombstone: tombstone() }),
        ]
    }

    fn event_strategy() -> impl proptest::strategy::Strategy<Value = Event> {
        use proptest::prelude::*;
        let reason = proptest::sample::select(vec![
            DeletionReason::Omission,
            DeletionReason::Teardown,
            DeletionReason::Replace,
            DeletionReason::KindChange,
            DeletionReason::Decommission,
        ]);
        let intent = proptest::sample::select(vec![DeletionIntent::Retain, DeletionIntent::Teardown]);
        let held = proptest::option::of(Just(hold()));
        prop_oneof![
            (reason.clone(), intent).prop_map(|(reason, intent)| Event::Left { reason, intent }),
            Just(Event::Returned),
            reason.prop_map(requested),
            Just(Event::TeardownStarted),
            held.clone().prop_map(|hold| Event::TornDown { hold }),
            Just(Event::Retain { tombstone: tombstone() }),
            held.prop_map(|hold| Event::Released { hold }),
            Just(Event::LeftWithoutIncarnation),
            Just(Event::Hold { hold: hold() }),
            Just(Event::Resume),
        ]
    }

    /// The lifecycle after an accepted event; `None` once the files are gone.
    fn after(current: &Lifecycle, next: &Next) -> Option<Lifecycle> {
        match next {
            Next::Lifecycle(lifecycle) | Next::EndIncarnation(lifecycle) | Next::RestoreReceipt(lifecycle) => {
                Some(lifecycle.clone())
            }
            Next::Remove => None,
            Next::Unchanged => Some(current.clone()),
        }
    }

    proptest::proptest! {
        /// Catches a row that removes a unit's files (and with them the last
        /// desired document a teardown needs) for any event but a finished
        /// teardown or release without a hold, a uid-less unit leaving, or
        /// a tombstone of another kind being dropped.
        #[test]
        fn test_only_finishing_events_remove_a_unit(
            start in lifecycle_strategy(),
            events in proptest::collection::vec(event_strategy(), 1..10),
        ) {
            let mut current = start;
            for event in events {
                let removes_by_rule = matches!(
                    event,
                    Event::TornDown { hold: None }
                        | Event::Released { hold: None }
                        | Event::LeftWithoutIncarnation
                        | Event::Left { reason: DeletionReason::KindChange, .. }
                );
                let Ok(next) = transition(&current, event) else { continue };
                if next == Next::Remove {
                    proptest::prop_assert!(removes_by_rule);
                    break;
                }
                current = after(&current, &next).expect("only Remove removes");
            }
        }

        /// Catches a hold that does not return the unit to where it stopped
        /// when it is lifted.
        #[test]
        fn test_hold_then_resume_restores_the_lifecycle(start in lifecycle_strategy()) {
            let held_or_retained = matches!(start, Lifecycle::Held { .. } | Lifecycle::Retained { .. });
            proptest::prop_assume!(!held_or_retained);
            let held = after(&start, &transition(&start, Event::Hold { hold: hold() }).unwrap()).unwrap();
            let resumed = after(&held, &transition(&held, Event::Resume).unwrap()).unwrap();
            proptest::prop_assert_eq!(resumed, start);
        }

        /// Catches a held unit's teardown starting or finishing without
        /// `resume` (Q26: a held unit never progresses).
        #[test]
        fn test_a_held_unit_reaches_no_teardown_without_resume(
            deletion in proptest::option::of(proptest::strategy::Just(())),
            events in proptest::collection::vec(event_strategy(), 1..10),
        ) {
            let mut current = Lifecycle::Held {
                hold: hold(),
                deletion: deletion.map(|()| Deletion {
                    hold: false,
                    reason: DeletionReason::Omission,
                    intent: DeletionIntent::Teardown,
                    teardown: Some(TeardownProgress::Pending),
                    requested: None,
                }),
            };
            for event in events {
                if matches!(event, Event::Resume) {
                    break;
                }
                let Ok(next) = transition(&current, event) else { continue };
                proptest::prop_assert!(next != Next::Remove, "a held unit was removed");
                let lifecycle = after(&current, &next).unwrap();
                proptest::prop_assert!(matches!(lifecycle, Lifecycle::Held { .. }), "{:?}", lifecycle);
                if let Lifecycle::Held { deletion: Some(deletion), .. } = &lifecycle {
                    proptest::prop_assert_ne!(deletion.teardown, Some(TeardownProgress::InProgress));
                }
                current = lifecycle;
            }
        }
    }
}
