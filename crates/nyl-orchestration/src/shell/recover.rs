//! `nyl recover -e <env> --unit <u> (--retry | --accept <run>)`.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery)
//! and [Commands](../../../../design/orchestration-core.md#commands)
//! (`recover` writes one observed commit). Takes the lease, decides with
//! [`crate::rules::recovery`], and writes one observed commit with
//! operation `recover`, the unit under the summary's `recovered`, and the
//! `Nyl-Requested-By` and `Nyl-Reason` trailers. It never executes. Exit: 0
//! done, 1 error or refusal, 2 lease held. The report's `details` show the
//! checkpoints the run refs hold for the unit
//! ([`crate::rules::recovery::evidence`]), next to its condition.

use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::state::StateLocation;

use super::state::Requested;
use super::Context;
use crate::report::StateOperationReport;
use crate::rules::recovery::RecoverAction;

/// `nyl recover`.
pub fn recover(
    context: &Context<'_>,
    environment: &EnvironmentName,
    location: &StateLocation,
    unit: &UnitName,
    action: &RecoverAction,
    requested: &Requested,
) -> StateOperationReport {
    let _ = (context, environment, location, unit, action, requested);
    todo!("recover-verify")
}
