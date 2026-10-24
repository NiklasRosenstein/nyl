//! A unit's state and receipt state, for `status`, `get units -e`, and `plan`.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection):
//! each unit is in one of `blocked`, `ready`, `awaiting-approval`,
//! `running`, `uncertain`, `failed`, `current`, `held`, `pending-teardown`,
//! `tearing-down`, or `retained`, and its receipt is shown separately as
//! current, stale, or none.

use nyl_core::orchestration::ports::{Coordination, EnvironmentSnapshot};
use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit};

use crate::run_report::{EnvironmentRunReport, ReceiptState, UnitState};

/// Current, stale, or none, by [`crate::rules::freshness::receipt_is_current`].
pub fn receipt_state(desired: Option<&DesiredUnit>, observed: Option<&ObservedUnit>) -> ReceiptState {
    let _ = (desired, observed);
    todo!("run-model")
}

/// A unit's state from its persisted documents; `running` when a live run
/// ref names it.
pub fn unit_state(desired: &DesiredUnit, observed: Option<&ObservedUnit>, running: bool) -> UnitState {
    let _ = (desired, observed, running);
    todo!("run-model")
}

/// The `status -e` report of one environment from one snapshot of both refs
/// plus the lease and run refs: per-unit state, receipt, blockers, the
/// running unit and holder, and suggested next actions as warnings with fixes.
pub fn status(
    environment: &nyl_core::orchestration::ids::EnvironmentName,
    snapshot: &EnvironmentSnapshot,
    coordination: &Coordination,
) -> EnvironmentRunReport {
    let _ = (environment, snapshot, coordination);
    todo!("run-model")
}
