//! Drift repair: which units with a current receipt execute again because
//! their latest drift observation is `drifted`, `--reapply`, the summary's
//! `driftRepaired` and `reapplied`, and how status shows drifted units that wait.
//!
//! Contract: [Drift](../../../../design/orchestration-core.md#drift) and
//! [Execution](../../../../design/orchestration-core.md#execution), step 3:
//! a unit with a current receipt is a candidate again when its latest drift
//! observation for that receipt is `drifted`; it is selected when its
//! recovery policy is `converge` and the environment's `driftRepair` is
//! `converge` (the default), and otherwise waits for `nyl reconcile
//! --reapply`. `--reapply <unit> --reason <text>` executes a unit with a
//! current receipt again, drifted or not; a unit whose recovery policy is
//! not `converge` needs an approval (`--approve <unit>` in a local run;
//! refused in remote runs until remote approvals exist). A repair's new
//! receipt replaces the one the observation was about, so the observation
//! no longer selects it.
//!
//! Skeleton note: [`repairs`], [`observation_changes`], and [`status_drift`]
//! are called on every run and every `status`; until package `drift`
//! implements them they return nothing, which is the behavior before drift
//! repair.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
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
    /// `--reapply` named it: listed under `reapplied`.
    Reapply,
}

/// Of the units [`super::selection::select`] made ready, those it selected
/// only for drift repair or `--reapply`.
pub fn repairs(candidates: &Candidates<'_>, ready: &[UnitName]) -> BTreeMap<UnitName, Repair> {
    let _ = (candidates, ready);
    BTreeMap::new()
}

/// Check `--reapply` before anything executes: every named unit exists in
/// the ownership set and is not excluded by `--unit`, has no condition
/// (`nyl recover` first), and, in a remote run, has a `converge` recovery
/// policy (`NYL-REAPPLY-APPROVAL-REQUIRED` otherwise); a `--local` run
/// (`local`) needs `--approve` for the others instead.
#[allow(clippy::too_many_arguments)] // Each is one input of the check.
pub fn check_reapply(
    environment: &EnvironmentName,
    reapply: &BTreeSet<UnitName>,
    named: Option<&BTreeSet<UnitName>>,
    desired: &BTreeMap<UnitName, DesiredUnit>,
    observed: &BTreeMap<UnitName, ObservedUnit>,
    drivers: &DriverSet,
    local: bool,
) -> Result<(), Finding> {
    let _ = (environment, reapply, named, desired, observed, drivers, local);
    todo!("drift")
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
/// current receipt, and whether a drifted unit waits for `--reapply`.
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

/// `NYL-UNIT-DRIFTED`: a drifted unit waits for `nyl reconcile --reapply`.
pub fn waiting_warning(environment: &EnvironmentName, unit: &UnitName) -> Finding {
    let _ = (environment, unit);
    todo!("drift")
}
