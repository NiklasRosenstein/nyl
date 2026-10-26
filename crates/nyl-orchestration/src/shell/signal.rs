//! `--confirm-removed` delivery: confirm a publication's removal directly,
//! or send the confirmation to the run waiting on that unit's teardown.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! the signal path: with no lease held, confirm directly; when the lease
//! holder waits on that unit's teardown, push a `Signal` to its signal ref
//! naming the operator, reason, time, run ID, and phase 1 commit, report
//! where it went, and exit 0; when the holder does anything else, exit 2 as
//! for a held lease. After pushing, check that the lease still belongs to
//! that run; if the run finished meanwhile, delete the signal and confirm
//! directly. A confirmation for a unit the run is not waiting on is
//! rejected when it is sent (`NYL-CONFIRMATION-REJECTED`).

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::state::StateLocation;

use super::Context;

/// Where a confirmation goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// No run waits on the unit: the invocation confirms directly, through a
    /// teardown run of its own.
    Direct,
    /// Delivered to the waiting run, which continues the teardown.
    Delivered { run: RunId, runner: String },
}

/// Decide where `--confirm-removed` for `unit` goes, sending the signal when
/// a run waits on it.
pub fn deliver(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    unit: &UnitName,
    reason: Option<&str>,
) -> Result<Delivery, Finding> {
    let _ = (context, location, environment, unit, reason);
    todo!("publication")
}
