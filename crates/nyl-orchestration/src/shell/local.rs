//! Local state inspection and guarded reset helpers.
//!
//! Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::ports::{StateError, StateSide, StateStore};
use nyl_core::orchestration::state::StateLocation;

use super::run::RunRequest;
use super::store_error;
use crate::rules;
use crate::run_report::{EnvironmentRunReport, UnitResult};

/// Lease location for an injected local run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseScope {
    /// Coordinate through the configured remote.
    Remote,
    /// Coordinate through local state.
    Local,
}

/// A `--local` run's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRun {
    pub lease: LeaseScope,
    /// `--approve <unit>`: units whose recovery policy is not `converge` that
    /// this local run may execute, recorded with the `local` approval
    /// identity (the workstation's Git user). Others wait as
    /// `awaiting-approval`.
    /// Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
    pub approve: std::collections::BTreeSet<nyl_core::orchestration::ids::UnitName>,
}

/// The state refs of a location, one when both are the same ref.
pub(crate) fn sides(location: &StateLocation) -> &'static [StateSide] {
    if location.desired_ref == location.observed_ref {
        &[StateSide::Desired]
    } else {
        &StateSide::ALL
    }
}

/// How far back guarded reset lists local transition commits.
pub const TIP_HISTORY: usize = 100;

/// State history is accepted independently of commit trailers.
pub fn check_remote_tips(
    _store: &dyn StateStore,
    _location: &StateLocation,
    _environment: &EnvironmentName,
) -> Result<(), Finding> {
    Ok(())
}

/// Check the configured state record.
pub fn check_record(
    _store: &dyn StateStore,
    record: Option<&nyl_core::orchestration::state::StateRecord>,
    environment: &EnvironmentName,
    location: &StateLocation,
) -> Result<(), Finding> {
    rules::state_location::check_record(record, environment, location)
}

/// Add a `--local` run's warnings to its report: `NYL-LOCAL-RUN-UNRECORDED`
/// for every unit it executed, naming the remote command that records it.
pub fn finish_local_report(report: &mut EnvironmentRunReport, request: &RunRequest) {
    let warnings = report
        .units
        .iter()
        .filter(|(_, unit)| {
            matches!(
                unit.result,
                Some(UnitResult::Executed { .. } | UnitResult::Recovered { .. })
            )
        })
        .map(|(name, _)| rules::local::unrecorded_warning(&request.environment, name))
        .collect::<Vec<_>>();
    report.warnings.extend(warnings);
}

/// `NYL-STATE-LOCAL-DIVERGED` when the clone's local branches of `location`
/// hold local transitions or are behind the remote; `None` otherwise, and
/// for a store that keeps no local branches (`StateError::Unavailable`) or
/// state that lives on them.
pub fn divergence(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
) -> Result<Option<Finding>, Finding> {
    if location.is_local() {
        return Ok(None);
    }
    match store.local_divergence(location) {
        Ok(divergence) => Ok(rules::local::divergence_warning(environment, &divergence)),
        Err(StateError::Unavailable(_)) => Ok(None),
        Err(error) => Err(store_error(environment, &error)),
    }
}
