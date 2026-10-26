//! The lifecycle transition table of a desired unit: deletion, retention,
//! release, explicit teardown, and holds.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion)
//! (the `lifecycle` block, omission, teardown, retain, fencing, explicit
//! teardown, holds), [Identity](../../../../design/orchestration-core.md#identity)
//! (a kind change ends the incarnation), [Instances](../../../../design/orchestration-core.md#instances)
//! (`teardown --all` holds and releases), and
//! [Recovery](../../../../design/orchestration-core.md#recovery), "Holds on
//! uncertain units". `design/implementation-architecture.md`: lifecycle
//! transitions live in this one table, and each rule is one row with one
//! test.
//!
//! Every change of a desired unit's `lifecycle` goes through [`transition`]:
//! `reconcile` (omission, return, retention, release, kind change, teardown
//! progress), `nyl teardown`, `nyl hold`, and `nyl resume`. The table decides
//! the lifecycle only; the caller moves documents accordingly (a tombstone's
//! receipt, an observed file, a uid), and receipts and conditions keep
//! following [`super::transitions`].
//!
//! Rows (current state, event: next):
//!
//! - active, left with intent teardown: deleting, teardown pending.
//! - active, left with intent retain: deleting with intent retain (the
//!   caller retains it in the same run).
//! - active or deleting with intent retain, retain: retained tombstone.
//! - active or deleting, released (a kind that cannot tear down): files removed.
//! - deleting with teardown pending and reason omission or kind-change,
//!   returned: active (the caller keeps uid and receipt). Any other deleting
//!   unit, returned: unchanged (an explicit request is not undone by source,
//!   and a started teardown continues; a new incarnation waits for it).
//! - retained, returned (same name and kind): active, adopting the tombstone.
//! - retained, left with reason kind-change: files removed (the tombstone of
//!   another kind is dropped, resources left in place).
//! - deleting or held-deleting, teardown started: teardown in progress.
//! - deleting, torn down: files removed.
//! - held, torn down: held without an incarnation (the file survives
//!   without a uid until `resume`).
//! - active, deleting, or retained, teardown requested (explicit): deleting
//!   with the request's reason, teardown pending; a deleting unit keeps its
//!   progress.
//! - held, teardown requested: held, with the deletion block next to the hold.
//! - held, left: unchanged (a held unit leaving the ownership set is not
//!   deleted; `hold.pending.deletion` reports it).
//! - held, released: held without an incarnation.
//! - active, hold: held. Deleting, hold: held, keeping the deletion block.
//!   Held, hold: unchanged. Retained, hold: refused.
//! - held, resume: active, or deleting where it stopped. Not held, resume: unchanged.

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
    /// The teardown completed.
    TornDown,
    /// A declared `Retain` unit leaves its files for a tombstone.
    Retain { tombstone: Tombstone },
    /// A kind that cannot tear down leaves its resources in place.
    Released,
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
            Self::TornDown => "torn-down",
            Self::Retain { .. } => "retain",
            Self::Released => "released",
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
        (Lifecycle::Active | Lifecycle::Deleting { .. }, Event::Released) => Next::Remove,
        (Lifecycle::Held { hold, .. }, Event::Released) => Next::EndIncarnation(Lifecycle::Held {
            hold: hold.clone(),
            deletion: None,
        }),

        // Explicit teardown.
        (Lifecycle::Active | Lifecycle::Retained { .. }, Event::TeardownRequested { reason, requested }) => {
            Next::Lifecycle(Lifecycle::Deleting {
                deletion: Deletion {
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
                    teardown: Some(TeardownProgress::InProgress),
                    ..deletion.clone()
                },
            })
        }
        (
            Lifecycle::Held {
                hold,
                deletion: Some(deletion),
            },
            Event::TeardownStarted,
        ) if deletion.intent == DeletionIntent::Teardown => Next::Lifecycle(Lifecycle::Held {
            hold: hold.clone(),
            deletion: Some(Deletion {
                teardown: Some(TeardownProgress::InProgress),
                ..deletion.clone()
            }),
        }),
        (Lifecycle::Deleting { deletion }, Event::TornDown) if deletion.intent == DeletionIntent::Teardown => {
            Next::Remove
        }
        (
            Lifecycle::Held {
                hold,
                deletion: Some(deletion),
            },
            Event::TornDown,
        ) if deletion.intent == DeletionIntent::Teardown => Next::EndIncarnation(Lifecycle::Held {
            hold: hold.clone(),
            deletion: None,
        }),

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
