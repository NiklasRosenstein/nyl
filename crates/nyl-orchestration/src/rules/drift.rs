//! Drift repair: which units with a current receipt execute again because
//! their latest drift observation is `drifted` or `--reapply` forces them,
//! the `drifted` unit state, and the summary's `driftRepaired` and
//! `reapplied`.
//!
//! Contract: [Drift](../../../../design/orchestration-core.md#drift) and
//! [Execution](../../../../design/orchestration-core.md#execution), step 3,
//! as amended by the part 5 decisions:
//! - `drifted` is a unit state in status, plan, and reports: work for the
//!   next reconcile, like a changed unit (`nyl status` exits 2; `nyl plan`
//!   lists it as a unit that would execute).
//! - `driftRepair: converge` (the default): the next reconcile executes
//!   drifted units whose recovery policy is `converge`. A drifted unit that
//!   is not safe to re-run is `awaiting-approval` (exit 2) while the rest
//!   of the run continues; a `--local` run executes it with `--approve`.
//! - `driftRepair: manual`: reconcile repairs no drift by itself; drifted
//!   units stay `drifted` until a forced run.
//! - `nyl reconcile --unit <u>… --reapply --reason <text>` forces the named
//!   units to execute although they are current; a non-converging named
//!   unit follows the approval rule above and never fails the run.
//! - A new receipt drops the unit's drift observation in the same commit.
//!
//! Skeleton note: [`repairs`], [`observation_changes`], and [`status_drift`]
//! are called on every run and every `status`; until package `drift`
//! implements them they return nothing, which is the behavior before drift
//! repair.

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{DesiredUnit, Observation, ObservationType, ObservedUnit};
use nyl_core::resources::environment::{DriftRepair, Environment};

use super::selection::Candidates;
use crate::run_report::DriftAttribute;

/// The environment's effective `driftRepair`: as S declares it (group
/// defaults applied by `effective_environment`), else `converge`.
pub fn drift_repair(environment: &Environment) -> DriftRepair {
    environment.spec.drift_repair.unwrap_or_default()
}

/// Why a unit with a current receipt was selected again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Its latest drift observation is `drifted`, under `driftRepair: converge`
    /// and a `converge` recovery policy: listed under `driftRepaired`.
    Drift,
    /// `--reapply` forced it (a unit named with `--unit`): listed under `reapplied`.
    Reapply,
}

/// Of the units [`super::selection::select`] made ready, those it selected
/// only for drift repair or `--reapply`.
pub fn repairs(candidates: &Candidates<'_>, ready: &[UnitName]) -> BTreeMap<UnitName, Repair> {
    let _ = (candidates, ready);
    BTreeMap::new()
}

/// The observation changes of a transition: a unit whose receipt this run
/// replaced loses the drift observation about the old receipt.
pub fn observation_changes(
    snapshot: &EnvironmentSnapshot,
    observed: &BTreeMap<UnitName, ObservedUnit>,
) -> BTreeMap<UnitName, Option<BTreeMap<ObservationType, Observation>>> {
    let _ = (snapshot, observed);
    BTreeMap::new()
}

/// The drift attribute `status` and `get units -e` show for a unit with a
/// current receipt: its latest drift observation's result. A unit whose
/// result is `drifted` has the unit state `drifted`
/// ([`crate::run_report::UnitState::Drifted`]); `rules::unit_state` sets it
/// from this attribute.
pub fn status_drift(
    desired: &DesiredUnit,
    observed: Option<&ObservedUnit>,
    observations: Option<&BTreeMap<ObservationType, Observation>>,
    drivers: &DriverSet,
    repair: DriftRepair,
) -> Option<DriftAttribute> {
    let _ = (desired, observed, observations, drivers, repair);
    None
}
