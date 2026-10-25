//! The orchestration shell: the only code that performs effects, and only
//! through the ports of [`nyl_core::orchestration::ports`].
//!
//! Decisions come from [`crate::rules`]; the shell reads snapshots, asks the
//! rules, and writes what they decide.

pub mod inspect;
pub mod lease;
pub mod local;
pub mod recover;
pub mod run;
pub mod state;
pub mod verify;

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
use nyl_core::orchestration::ports::{
    Clock, IdGenerator, StateError, StateStore, StateTips, Transition, TransitionChanges,
};
use nyl_core::orchestration::state::StateLocation;
use nyl_core::orchestration::time::Duration;

use crate::fault::FaultInjector;
use crate::transition::{self, Trailers, TransitionSummary};
use lease::HeldLease;

/// Defaults of `nyl.toml` that runs use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Deadline of resolution and of operations without units.
    pub resolution_timeout: Duration,
    /// Added to every deadline; absorbs clock skew.
    pub lease_grace: Duration,
    /// How often a run waiting for a lease reads it again.
    pub lease_poll: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution_timeout: Duration::from_mins(30),
            lease_grace: Duration::from_mins(10),
            lease_poll: Duration::from_secs(15),
        }
    }
}

/// What every operation of the shell runs with.
pub struct Context<'a> {
    pub store: &'a dyn StateStore,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdGenerator,
    /// Fault and pause points; [`crate::fault::NoFaults`] in production.
    pub faults: &'a dyn FaultInjector,
    /// Who runs it, such as a CI run URL or `user@host`.
    pub runner: String,
    /// Who requested the operation, recorded as `Nyl-Requested-By`.
    pub requested_by: String,
    /// Nyl's version, recorded as `Nyl-Version`.
    pub version: String,
    pub settings: Settings,
}

/// The finding for a state store failure of `environment`.
pub(crate) fn store_error(environment: &EnvironmentName, error: &StateError) -> Finding {
    match error {
        StateError::LeaseLost => lease::lost(environment),
        StateError::NotFastForward { reference } => Finding::new(
            Code::StateNotFastForward,
            format!("state ref {reference} of {environment} was rewritten outside Nyl"),
            "restore the ref to a descendant of what Nyl last wrote; Nyl never force-pushes state",
        ),
        StateError::Conflict { reference, actual } => Finding::new(
            Code::StateConflict,
            format!("{reference} of {environment} moved to {actual} while this operation ran, changing its state"),
            "run the command again; it reads the new state. Changes outside Nyl, such as a merged pull request, also cause this",
        ),
        other => Finding::new(
            Code::StateUnavailable,
            format!("state of {environment} is unavailable: {other}"),
            "check access to the state repository and retry",
        ),
    }
}

/// The trailers every transition commit written under `held` carries: its
/// run, the tips it read, the runner, and Nyl's version. Operations add
/// the source commit, the requester, the reason, or `Nyl-Local`.
pub(crate) fn trailers(context: &Context<'_>, held: &HeldLease, tips: &StateTips) -> Trailers {
    Trailers {
        run: held.lease.run.clone(),
        source_commit: None,
        read_desired: tips.desired.clone(),
        read_observed: tips.observed.clone(),
        runner: context.runner.clone(),
        evaluated_at: held.lease.taken_at,
        version: context.version.clone(),
        requested_by: None,
        reason: None,
        local: false,
    }
}

/// Write one transition on top of `tips`, fenced by `held`, recording the
/// lease break it took over. The fenced push moves the lease, so `held`
/// carries its new version afterwards. Returns the commits written, by
/// state ref side.
pub(crate) fn commit_transition(
    context: &Context<'_>,
    location: &StateLocation,
    held: &mut HeldLease,
    tips: &StateTips,
    mut summary: TransitionSummary,
    changes: TransitionChanges,
    trailers: &Trailers,
) -> Result<BTreeMap<&'static str, CommitId>, StateError> {
    summary.lease_broken.clone_from(&held.broken);
    let transition = Transition {
        message: transition::message(&summary, trailers),
        changes,
    };
    let committed = context
        .store
        .commit(location, &held.environment, tips, &transition, &held.version)?;
    held.version = committed.lease_version;
    Ok([("desired", committed.desired), ("observed", committed.observed)]
        .into_iter()
        .filter_map(|(side, commit)| Some((side, commit?)))
        .collect())
}
