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
//! - When a run holds the lease, `hold` does not refuse with
//!   `NYL-LEASE-HELD`: it delivers the hold through
//!   [`super::signal::deliver`] (`SignalAction::Hold`), reports
//!   `NYL-HOLD-DELIVERED`, and exits 0; if the run finished meanwhile, it
//!   applies the hold directly under its own lease (Q28).
//! - `resume` only lifts the hold and deletes nothing (Q26); a uid-less
//!   held file outside the ownership set is removed by the next reconcile.

use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::state::StateLocation;

use super::state::Requested;
use super::Context;
use crate::report::StateOperationReport;

/// `nyl hold`.
pub fn hold(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    requested: &Requested,
) -> StateOperationReport {
    let _ = (context, environment, location, unit, requested);
    todo!("operator")
}

/// `nyl resume`.
pub fn resume(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    requested: &Requested,
) -> StateOperationReport {
    let _ = (context, environment, location, unit, requested);
    todo!("operator")
}
