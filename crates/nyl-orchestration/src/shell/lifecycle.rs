//! `nyl hold -e <env> --unit <u>` and `nyl resume -e <env> --unit <u>`.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit", [Recovery](../../../../design/orchestration-core.md#recovery),
//! "Holds on uncertain units", and [Commands](../../../../design/orchestration-core.md#commands)
//! (each writes one desired commit, operation `hold` or `resume`, with the
//! `Nyl-Requested-By` and `Nyl-Reason` trailers; an operation that changes
//! nothing writes nothing). Both take the lease and never take a broken
//! lease over (`NYL-LEASE-BROKEN`). Lifecycle changes go through
//! [`crate::rules::lifecycle::transition`].
//!
//! A hold on a unit with an `uncertain` condition writes the desired
//! document and uid of the unit's last receipt as its desired state, so
//! consumers keep resolving from that receipt; the condition and any
//! evidence ref stay, and `resume` does not clear the condition.

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
