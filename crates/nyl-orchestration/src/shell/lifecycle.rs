//! `nyl hold -e <env> --unit <u>` and `nyl resume -e <env> --unit <u>`.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit", [Recovery](../../../../design/orchestration-core.md#recovery),
//! "Holds on uncertain units", and [Commands](../../../../design/orchestration-core.md#commands)
//! (each writes one desired commit, operation `hold` or `resume`, with the
//! `Nyl-Requested-By` and `Nyl-Reason` trailers; an operation that changes
//! nothing writes nothing). Both never take a broken lease over
//! (`NYL-LEASE-BROKEN`). Lifecycle changes go through
//! [`crate::rules::lifecycle::transition`], and what a hold writes is
//! decided by [`crate::rules::hold::hold`], which the run path shares.
//!
//! As amended by the part 5 decisions:
//! - `hold` never renders source. A unit with no desired document gets a
//!   held marker (Q19). An active unit with a receipt reverts to the
//!   desired document the receipt was produced for, read from the observed
//!   unit's copy (Q16); with an `uncertain` condition the condition and any
//!   evidence ref stay, and `resume` does not clear it.
//! - When a reconcile or teardown run holds the lease, `hold` does not
//!   refuse with `NYL-LEASE-HELD`: it delivers the hold through
//!   [`super::signal::deliver`] (`SignalAction::Hold`), reports
//!   `NYL-HOLD-DELIVERED`, and exits 0; if the run finished meanwhile, it
//!   applies the hold directly under its own lease (Q28). Any other holder
//!   reads no signals, so `hold` meets its lease as a held lease. A run
//!   applies a hold it read only after releasing its lease through
//!   [`hold_signalled`].
//! - `resume` only lifts the hold and deletes nothing (Q26); a uid-less
//!   held file outside the ownership set is removed by the next reconcile.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::TransitionChanges;
use nyl_core::orchestration::state::{Hold, Lifecycle, Operation, Signal, SignalAction, StateLocation};
use nyl_core::orchestration::time::{Duration, Timestamp};
use serde_json::json;

use super::local::{check_record, check_remote_tips};
use super::run::Cancellation;
use super::signal::{deliver, Delivery};
use super::state::{commit, read, summary, with_lease, Requested};
use super::{store_error, Context};
use crate::report::StateOperationReport;
use crate::rules::hold::{self as hold_rule, HoldWrite};
use crate::rules::lease::{self as lease_rule, BrokenLease, LeaseDecision};
use crate::rules::lifecycle::{transition, Event, Next};

/// The summary entry of an operator action on `unit`.
fn action(name: &str, requested: &Requested) -> serde_json::Value {
    let mut entry = json!({"action": name});
    if let Some(reason) = &requested.reason {
        entry["reason"] = json!(reason);
    }
    entry
}

/// The report of a hold delivered to the run holding the lease: done,
/// exit 0, no commit of its own.
fn delivered(environment: &EnvironmentName, unit: &UnitName, run: &RunId, runner: &str) -> StateOperationReport {
    let mut report = StateOperationReport::new(environment.clone(), Operation::Hold.as_str()).done();
    report.warnings.push(Finding::new(
        Code::HoldDelivered,
        format!("run {run} by {runner} holds the lease of {environment}; the hold of {unit} was delivered to it"),
        format!(
            "the run starts no further work on {unit} and records the hold in its own transition commit; check it with `nyl status -e {environment}`"
        ),
    ));
    report
        .details
        .insert("deliveredTo".to_owned(), json!({"run": run, "runner": runner}));
    report.details.insert("unit".to_owned(), json!(unit));
    report
}

/// `nyl hold`.
pub fn hold(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    requested: &Requested,
) -> StateOperationReport {
    hold_since(context, environment, location, unit, requested, None)
}

/// The hold of `signal`, which the run it was sent to read only after
/// releasing its lease: applied as `nyl hold` applies it, requested by the
/// signal's sender, with its reason and time (Q28).
pub(crate) fn hold_signalled(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    signal: &Signal,
    wait: Option<Duration>,
    cancellation: &Cancellation,
) -> StateOperationReport {
    let context = Context {
        store: context.store,
        clock: context.clock,
        ids: context.ids,
        faults: context.faults,
        runner: context.runner.clone(),
        requested_by: signal.by.clone(),
        version: context.version.clone(),
        settings: context.settings,
    };
    let requested = Requested {
        reason: signal.reason.clone(),
        wait,
        cancellation: cancellation.clone(),
    };
    hold_since(
        &context,
        environment,
        location,
        &signal.unit,
        &requested,
        Some(signal.at),
    )
}

/// `nyl hold`, its hold dated `since` or, without one, when it takes the lease.
fn hold_since(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    requested: &Requested,
    since: Option<Timestamp>,
) -> StateOperationReport {
    let refuse = |finding| StateOperationReport::new(environment.clone(), Operation::Hold.as_str()).refused(finding);
    // A run holding the lease (a live lease or run ref) receives the hold
    // through its signal ref and applies it before starting the unit (Q28).
    let coordination = match context.store.coordination(location, environment) {
        Ok(coordination) => coordination,
        Err(error) => return refuse(store_error(environment, &error)),
    };
    let running = matches!(
        lease_rule::decide(&coordination, context.clock.now(), BrokenLease::Refuse),
        LeaseDecision::Held(_)
    );
    if running {
        match deliver(
            context,
            location,
            environment,
            unit,
            SignalAction::Hold,
            requested.reason.as_deref(),
        ) {
            // The run finished meanwhile: hold directly, under this lease.
            Ok(Delivery::Direct) => {}
            Ok(Delivery::Delivered { run, runner }) => return delivered(environment, unit, &run, &runner),
            Err(finding) => return refuse(finding),
        }
    }
    with_lease(
        context,
        environment,
        Operation::Hold,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            let coordination = context
                .store
                .coordination(location, environment)
                .map_err(|error| store_error(environment, &error))?;
            let runs = coordination
                .runs
                .iter()
                .chain(&coordination.evidence)
                .cloned()
                .collect::<Vec<_>>();
            let stored = snapshot.desired.get(unit);
            let observed = snapshot.observed.get(unit);
            let the_hold = Hold {
                since: since.unwrap_or(held.lease.taken_at),
                by: context.requested_by.clone(),
                reason: requested.reason.clone(),
                pending: None,
            };
            report.details.insert("unit".to_owned(), json!(unit));
            let document = match hold_rule::hold(environment, unit, stored, observed, &runs, the_hold)? {
                HoldWrite::Unchanged => {
                    report
                        .details
                        .insert("note".to_owned(), json!(format!("{unit} is already held")));
                    return Ok(());
                }
                HoldWrite::Desired(document) => *document,
            };
            if document.is_held_marker() {
                report.details.insert("marker".to_owned(), json!(true));
            }
            if let Lifecycle::Held {
                hold: Hold {
                    pending: Some(pending), ..
                },
                ..
            } = &document.lifecycle
            {
                report
                    .details
                    .insert("pending".to_owned(), json!(pending.source_commit));
            }
            let mut changes = TransitionChanges::default();
            // The incarnation ended (nothing showed it ever ran): its
            // observed documents go with its uid.
            let ended = document.unit.uid.is_none() && stored.is_some_and(|stored| stored.unit.uid.is_some());
            if ended {
                if snapshot.observed.contains_key(unit) {
                    changes.observed.insert(unit.clone(), None);
                }
                if snapshot.artifacts.contains_key(unit) {
                    changes.artifacts.insert(unit.clone(), None);
                }
                if snapshot.observations.contains_key(unit) {
                    changes.observations.insert(unit.clone(), None);
                }
                if snapshot.attestations.contains_key(unit) {
                    changes.attestations.insert(unit.clone(), None);
                }
            }
            changes.desired.insert(unit.clone(), Some(document));
            let mut summary = summary(Operation::Hold, environment, unit.as_str());
            summary.units = BTreeMap::from([(unit.clone(), action("hold", requested))]);
            commit(context, location, held, &snapshot, summary, changes, requested, report)
        },
    )
}

/// `nyl resume`.
pub fn resume(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    requested: &Requested,
) -> StateOperationReport {
    with_lease(
        context,
        environment,
        Operation::Resume,
        location,
        requested,
        |held, report| {
            let snapshot = read(context, location, environment)?;
            check_record(context.store, snapshot.record.as_ref(), environment, location)?;
            check_remote_tips(context.store, location, environment)?;
            report.details.insert("unit".to_owned(), json!(unit));
            let Some(stored) = snapshot.desired.get(unit) else {
                return Err(Finding::new(
                    Code::UnitUnknown,
                    format!("{environment} has no unit {unit} in its state"),
                    format!("list the units with `nyl get units -e {environment}`"),
                ));
            };
            let lifecycle = match transition(&stored.lifecycle, Event::Resume) {
                Ok(Next::Lifecycle(lifecycle)) => lifecycle,
                Ok(Next::Unchanged) => {
                    report
                        .details
                        .insert("note".to_owned(), json!(format!("{unit} is not held")));
                    return Ok(());
                }
                Ok(next) => unreachable!("resume only lifts a hold: {next:?}"),
                Err(refusal) => unreachable!("every state has a resume row: {refusal:?}"),
            };
            // Resume only lifts the hold: the document, its uid (or its
            // absence), and the observed file with any condition stay (Q26).
            let mut document = stored.clone();
            report.details.insert(
                "state".to_owned(),
                json!(crate::rules::lifecycle::state_name(&lifecycle)),
            );
            document.lifecycle = lifecycle;
            let changes = TransitionChanges {
                desired: BTreeMap::from([(unit.clone(), Some(document))]),
                ..TransitionChanges::default()
            };
            let mut summary = summary(Operation::Resume, environment, unit.as_str());
            summary.units = BTreeMap::from([(unit.clone(), action("resume", requested))]);
            commit(context, location, held, &snapshot, summary, changes, requested, report)
        },
    )
}
