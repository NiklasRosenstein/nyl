//! What a hold writes: `nyl hold` applies it directly, and a run that
//! received a hold signal applies it in its own transition commit, so both
//! write the same document.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit", and [Recovery](../../../../design/orchestration-core.md#recovery),
//! "Holds on uncertain units", as amended by the part 5 decisions (Q16,
//! Q17, Q19):
//!
//! - No desired document in state: a held marker
//!   ([`DesiredUnit::held_marker`]), no uid, no rendering.
//! - An active unit with a receipt of its incarnation: the desired document
//!   the receipt was produced for, read from the observed unit's copy
//!   ([`ObservedUnit::desired`]), never from commit history; its uid stays
//!   and the newer document is recorded under `hold.pending`. The
//!   condition, if any, stays.
//! - A unit with state but nothing showing it ever ran (no receipt, no
//!   condition, and no leftover run listing it): held without its uid.
//! - A deleting unit pauses and keeps its deletion block.
//! - A retained tombstone is refused (`NYL-HOLD-REFUSED`); a held unit is
//!   unchanged.
//!
//! The lifecycle change itself goes through
//! [`super::lifecycle::transition`] (or [`super::lifecycle::transition_absent`]).

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::RunState;
use nyl_core::orchestration::state::{DesiredUnit, Hold, HoldPending, Lifecycle, ObservedUnit};

use super::lifecycle::{transition, transition_absent, Event, Next, Refusal};

/// The documents a hold writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldWrite {
    /// The unit is already held: nothing is written (exit 0 with a note).
    Unchanged,
    /// Write this desired document. When it has no uid while the stored
    /// document had one, the incarnation ended (nothing showed it ever ran):
    /// the caller drops the unit's observed file, as for
    /// [`Next::EndIncarnation`]. Otherwise the observed file stays as it is.
    Desired(Box<DesiredUnit>),
}

/// Whether any leftover run ref lists `unit`: in its record's units, among
/// the units it created, or in a checkpoint.
fn listed_by_a_run(unit: &UnitName, runs: &[RunState]) -> bool {
    runs.iter().any(|state| {
        state.run.units.contains(unit)
            || state.created.iter().any(|created| created.unit.name == *unit)
            || state
                .checkpoints
                .iter()
                .any(|checkpoint| checkpoint.desired.unit.name == *unit)
    })
}

fn refused(message: String, fix: String) -> Finding {
    Finding::new(Code::HoldRefused, message, fix)
}

/// Decide what holding `unit` writes, from the stored desired and observed
/// documents and the leftover run refs (run refs and evidence refs).
pub fn hold(
    environment: &EnvironmentName,
    unit: &UnitName,
    desired: Option<&DesiredUnit>,
    observed: Option<&ObservedUnit>,
    runs: &[RunState],
    hold: Hold,
) -> Result<HoldWrite, Finding> {
    let Some(stored) = desired else {
        return match transition_absent(Event::Hold { hold: hold.clone() }) {
            Ok(Next::Lifecycle(Lifecycle::Held { hold, .. })) => Ok(HoldWrite::Desired(Box::new(
                DesiredUnit::held_marker(environment.clone(), unit.clone(), hold),
            ))),
            other => unreachable!("the table holds an absent unit as a marker, not {other:?}"),
        };
    };
    let lifecycle = match transition(&stored.lifecycle, Event::Hold { hold }) {
        Ok(Next::Unchanged) => return Ok(HoldWrite::Unchanged),
        Ok(Next::Lifecycle(lifecycle)) => lifecycle,
        Ok(next) => unreachable!("a hold never ends or restores an incarnation: {next:?}"),
        Err(Refusal::HoldRetained) => {
            return Err(refused(
                format!("{unit} in {environment} is a retained tombstone, which nothing reconciles"),
                format!(
                    "tear it down with `nyl teardown -e {environment} --unit {unit}`, or drop it with `nyl state forget -e {environment} --unit {unit}`"
                ),
            ))
        }
        Err(Refusal::NotApplicable { state, event }) => {
            return Err(refused(
                format!("{unit} in {environment} is {state}, where {event} does not apply"),
                format!("check the unit with `nyl status -e {environment}`"),
            ))
        }
    };
    // A deleting unit pauses where it is: its document and deletion block stay.
    if matches!(stored.lifecycle, Lifecycle::Deleting { .. }) {
        let mut document = stored.clone();
        document.lifecycle = lifecycle;
        return Ok(HoldWrite::Desired(Box::new(document)));
    }
    let observed = observed.filter(|observed| stored.unit.uid.is_some() && observed.unit.uid == stored.unit.uid);
    let receipt = observed.and_then(|observed| observed.receipt.as_ref());
    if let (Some(observed), Some(_)) = (observed, receipt) {
        // An active unit with a receipt of its incarnation: what actually
        // runs is the document the receipt was produced for.
        let Some(copy) = observed.desired.as_deref() else {
            return Err(refused(
                format!("the receipt of {unit} in {environment} carries no copy of the desired document it was produced for"),
                format!("run `nyl reconcile -e {environment} --unit {unit}` once, so its receipt records that document, then hold it"),
            ));
        };
        let mut document = copy.clone();
        document.unit.uid.clone_from(&stored.unit.uid);
        let mut lifecycle = lifecycle;
        if let Lifecycle::Held { hold, .. } = &mut lifecycle {
            if stored.execution_key != copy.execution_key {
                if let Some(source_commit) = &stored.source_commit {
                    hold.pending = Some(HoldPending {
                        source_commit: source_commit.clone(),
                        execution_key: stored.execution_key.clone(),
                        deletion: false,
                    });
                }
            }
        }
        document.lifecycle = lifecycle;
        return Ok(HoldWrite::Desired(Box::new(document)));
    }
    let mut document = stored.clone();
    document.lifecycle = lifecycle;
    let ran = observed.is_some_and(|observed| observed.condition.is_some()) || listed_by_a_run(unit, runs);
    if !ran {
        // Nothing shows the incarnation ever ran: it ends, and the unit is
        // held without one, so it is not created while held.
        document.unit.uid = None;
    }
    // Otherwise (an uncertain unit without a receipt) the document stays,
    // and its dependents stay blocked.
    Ok(HoldWrite::Desired(Box::new(document)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::fixtures::{
        commit, desired, env, key, name, observed, receipt, run_record, run_state, OTHER_COMMIT, RUN_1,
    };
    use nyl_core::orchestration::state::{
        Condition, Deletion, DeletionIntent, DeletionReason, TeardownProgress, Tombstone,
    };
    use serde_json::json;

    fn the_hold() -> Hold {
        Hold {
            since: "2026-09-24T12:05:00Z".parse().unwrap(),
            by: "alice".to_owned(),
            reason: Some("incident 4711".to_owned()),
            pending: None,
        }
    }

    fn written(write: Result<HoldWrite, Finding>) -> DesiredUnit {
        match write.unwrap() {
            HoldWrite::Desired(document) => *document,
            HoldWrite::Unchanged => panic!("expected a document"),
        }
    }

    fn held(document: &DesiredUnit) -> &Hold {
        match &document.lifecycle {
            Lifecycle::Held { hold, .. } => hold,
            other => panic!("not held: {other:?}"),
        }
    }

    /// Q19: holding a unit never reconciled writes a held marker, so the
    /// next reconcile does not create it.
    #[test]
    fn test_hold_of_an_absent_unit_writes_a_marker() {
        let document = written(hold(&env(), &name("api"), None, None, &[], the_hold()));
        assert!(document.is_held_marker());
        assert_eq!(document.unit.uid, None);
        assert_eq!(held(&document).reason.as_deref(), Some("incident 4711"));
    }

    /// Q16, Q17: an active unit reverts to the document its receipt was
    /// produced for, keeps its uid, and records the displaced document
    /// under `hold.pending`, so consumers keep resolving from what runs.
    #[test]
    fn test_hold_reverts_to_the_receipts_document_and_records_pending() {
        let previous = desired("network", "OpenTofu", json!({"v": 1}), "old");
        let mut stored = desired("network", "OpenTofu", json!({"v": 2}), "new");
        stored.source_commit = Some(commit(OTHER_COMMIT));
        let mut observed = observed("network", "OpenTofu", Some(receipt("old", json!({}), Vec::new())));
        observed.desired = Some(Box::new(previous.clone()));
        let document = written(hold(
            &env(),
            &name("network"),
            Some(&stored),
            Some(&observed),
            &[],
            the_hold(),
        ));
        assert_eq!(document.spec, previous.spec);
        assert_eq!(document.execution_key, previous.execution_key);
        assert_eq!(document.unit.uid, stored.unit.uid);
        let pending = held(&document).pending.as_ref().expect("pending changes");
        assert_eq!(pending.source_commit, commit(OTHER_COMMIT));
        assert_eq!(pending.execution_key, Some(key("new")));
    }

    /// Recovery, "Holds on uncertain units": the receipt's document and uid
    /// are the held state; the condition lives in the observed file, which
    /// a hold never writes, so it stays.
    #[test]
    fn test_hold_of_an_uncertain_unit_with_a_receipt_keeps_its_uid() {
        let previous = desired("db", "Command", json!({}), "k");
        let mut observed = observed("db", "Command", Some(receipt("k", json!({}), Vec::new())));
        observed.desired = Some(Box::new(previous.clone()));
        observed.condition = Some(Condition::Uncertain {
            since: "2026-09-24T11:12:00Z".parse().unwrap(),
            run: crate::rules::fixtures::run(RUN_1),
            reason: "run lost its lease".to_owned(),
        });
        let document = written(hold(
            &env(),
            &name("db"),
            Some(&previous),
            Some(&observed),
            &[],
            the_hold(),
        ));
        assert_eq!(document.unit.uid, previous.unit.uid);
        assert_eq!(held(&document).pending, None, "same execution key: nothing pending");
    }

    /// Q19: a unit nothing shows ever ran is held without its uid, so it
    /// is not created while held and starts a new incarnation after resume.
    #[test]
    fn test_hold_of_a_unit_that_never_ran_drops_its_uid() {
        let stored = desired("api", "Command", json!({}), "k");
        let document = written(hold(&env(), &name("api"), Some(&stored), None, &[], the_hold()));
        assert_eq!(document.unit.uid, None);
        assert_eq!(document.spec, stored.spec);
    }

    /// A leftover run listing the unit may have started it: the uid stays,
    /// so the import that follows can still adopt the run's effects.
    #[test]
    fn test_hold_of_a_unit_a_run_lists_keeps_its_uid() {
        let stored = desired("api", "Command", json!({}), "k");
        let mut record = run_record(RUN_1, "2026-09-24T12:00:00Z".parse().unwrap());
        record.units = vec![name("api")];
        let document = written(hold(
            &env(),
            &name("api"),
            Some(&stored),
            None,
            &[run_state(record)],
            the_hold(),
        ));
        assert_eq!(document.unit.uid, stored.unit.uid);
    }

    /// Deletion: a deleting unit pauses and keeps its deletion block, so
    /// `resume` returns it to deleting where it stopped.
    #[test]
    fn test_hold_of_a_deleting_unit_keeps_its_deletion() {
        let mut stored = desired("api", "Command", json!({}), "k");
        let deletion = Deletion {
            reason: DeletionReason::Omission,
            intent: DeletionIntent::Teardown,
            teardown: Some(TeardownProgress::Pending),
            requested: None,
        };
        stored.lifecycle = Lifecycle::Deleting {
            deletion: deletion.clone(),
        };
        let document = written(hold(&env(), &name("api"), Some(&stored), None, &[], the_hold()));
        assert!(matches!(&document.lifecycle, Lifecycle::Held { deletion: Some(kept), .. } if *kept == deletion));
        assert_eq!(document.unit.uid, stored.unit.uid);
    }

    /// Q22: a retained tombstone is refused with a fix naming the two
    /// operations that do apply to it.
    #[test]
    fn test_hold_of_a_retained_tombstone_is_refused() {
        let mut stored = desired("api", "Command", json!({}), "k");
        stored.lifecycle = Lifecycle::Retained {
            tombstone: Tombstone {
                since: "2026-09-24T10:00:00Z".parse().unwrap(),
                receipt: None,
            },
        };
        let finding = hold(&env(), &name("api"), Some(&stored), None, &[], the_hold()).unwrap_err();
        assert_eq!(finding.code, Code::HoldRefused);
        assert!(finding.fix.contains("nyl teardown") && finding.fix.contains("nyl state forget"));
    }

    /// Q25: a held unit is unchanged, so `nyl hold` writes no commit.
    #[test]
    fn test_hold_of_a_held_unit_is_unchanged() {
        let mut stored = desired("api", "Command", json!({}), "k");
        stored.lifecycle = Lifecycle::Held {
            hold: the_hold(),
            deletion: None,
        };
        assert_eq!(
            hold(&env(), &name("api"), Some(&stored), None, &[], the_hold()),
            Ok(HoldWrite::Unchanged)
        );
    }
}
