//! Taking, holding, and releasing an environment's lease.
//!
//! Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases).

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId};
use nyl_core::orchestration::ports::StateError;
use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, Request, StateLocation, StateV1};
use nyl_core::orchestration::time::{Duration, Timestamp};

use super::run::Cancellation;
use super::Context;
use crate::fault::FaultPoint;
use crate::rules::lease::{decide, LeaseDecision};

/// A lease this run holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLease {
    pub location: StateLocation,
    pub environment: EnvironmentName,
    pub lease: Lease,
    pub version: String,
    /// A `nyl lease break` this lease took over, for the transition commit.
    pub broken: Option<Request>,
    /// The expired or broken lease this run replaced, whose `units` the dead
    /// holder was executing; `None` when no lease ref existed. Run takeover
    /// (`rules::takeover`) reads it next to the leftover run refs.
    pub replaced: Option<Lease>,
}

/// Why a run did not get the lease.
#[derive(Debug)]
pub enum LeaseError {
    /// Another run holds it: `NYL-LEASE-HELD`, exit 2.
    Held(Finding),
    Store(StateError),
}

impl From<StateError> for LeaseError {
    fn from(error: StateError) -> Self {
        Self::Store(error)
    }
}

/// Take the lease of `environment` at `location` for `operation`, with the
/// deadline `phase` from now plus the grace period. With `wait`, a held
/// lease is read again until it is released or `wait` has passed; a
/// `cancellation` set meanwhile (SIGINT or SIGTERM, "Runs and leases" item 7)
/// stops waiting with `NYL-RUN-CANCELLED`, having written nothing.
#[allow(clippy::too_many_arguments)] // Each is one aspect of the request; a struct would only rename them.
pub fn acquire(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    run: &RunId,
    operation: Operation,
    phase: Duration,
    wait: Option<Duration>,
    cancellation: &Cancellation,
) -> Result<HeldLease, LeaseError> {
    let started = context.clock.now();
    loop {
        let now = context.clock.now();
        let coordination = context.store.coordination(location, environment)?;
        let replaced = coordination.lease.as_ref().map(|held| held.lease.clone());
        match decide(&coordination, now) {
            LeaseDecision::Take { expected, broken } => {
                let lease = Lease {
                    api_version: StateV1,
                    kind: LeaseKind,
                    run: run.clone(),
                    runner: context.runner.clone(),
                    operation,
                    units: Vec::new(),
                    deadline: deadline(context, now, phase),
                    taken_at: now,
                    broken: None,
                };
                match context
                    .store
                    .take_lease(location, environment, &lease, expected.as_deref())
                {
                    Ok(version) => {
                        return Ok(HeldLease {
                            location: location.clone(),
                            environment: environment.clone(),
                            lease,
                            version,
                            broken,
                            replaced,
                        })
                    }
                    // Another run took it between the read and the swap:
                    // read again.
                    Err(StateError::LeaseRace) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            LeaseDecision::Held(holder) => {
                let waited_out = wait.is_none_or(|wait| now >= wait.after(started));
                if waited_out {
                    return Err(LeaseError::Held(Finding::new(
                        Code::LeaseHeld,
                        format!("{environment} is held by {}", holder.describe()),
                        "wait for that run to finish, pass --wait-lease <duration>, or run `nyl lease break` if its runner is gone",
                    )));
                }
                if let Some(signal) = cancellation.signal() {
                    return Err(LeaseError::Held(cancelled_waiting(environment, signal)));
                }
                // A pause point: the scenario harness holds a waiting run here.
                let _ = context.faults.at(FaultPoint::WaitingLease, None);
                context.clock.wait(context.settings.lease_poll);
                if let Some(signal) = cancellation.signal() {
                    return Err(LeaseError::Held(cancelled_waiting(environment, signal)));
                }
            }
        }
    }
}

/// `now` plus a phase and the grace period: no phase runs without a deadline.
pub fn deadline(context: &Context<'_>, now: Timestamp, phase: Duration) -> Timestamp {
    phase.saturating_add(context.settings.lease_grace).after(now)
}

/// Release a lease this run holds. A lease that was taken over meanwhile is
/// not this run's to release.
pub fn release(context: &Context<'_>, held: &HeldLease) -> Result<(), StateError> {
    match context
        .store
        .release_lease(&held.location, &held.environment, &held.version)
    {
        Ok(()) | Err(StateError::LeaseLost) => Ok(()),
        Err(error) => Err(error),
    }
}

/// `NYL-RUN-CANCELLED` for an operation cancelled by `signal` while it
/// waited for the lease: it held nothing and wrote nothing.
pub fn cancelled_waiting(environment: &EnvironmentName, signal: i32) -> Finding {
    Finding::new(
        Code::RunCancelled,
        format!("cancelled by signal {signal} while waiting for the lease of {environment}; nothing was written"),
        "run the command again once the run holding the lease has finished",
    )
}

/// `NYL-LEASE-LOST`: the run pushed nothing.
pub fn lost(environment: &EnvironmentName) -> Finding {
    Finding::new(
        Code::LeaseLost,
        format!("the lease of {environment} was taken over or deleted; this run wrote no state"),
        "check `nyl status` for the run holding it; its next run imports this run's results",
    )
}
