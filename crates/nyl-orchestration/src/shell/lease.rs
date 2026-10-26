//! Taking, holding, and releasing an environment's lease.
//!
//! Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases).

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::StateError;
use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, Request, StateLocation, StateV1};
use nyl_core::orchestration::time::{Duration, Timestamp};

use super::run::Cancellation;
use super::{store_error, Context, Settings};
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

/// Take the lease of `environment` at `location` for `operation`, with the
/// deadline `phase` from now plus the grace period. With `wait`, a held
/// lease is read again until it is released or `wait` has passed; a
/// `cancellation` set meanwhile (SIGINT or SIGTERM, "Runs and leases" item 7)
/// stops waiting with `NYL-RUN-CANCELLED`, having written nothing. Another
/// run holding it is `NYL-LEASE-HELD`, exit 2.
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
) -> Result<HeldLease, Finding> {
    let started = context.clock.now();
    loop {
        let now = context.clock.now();
        let coordination = context
            .store
            .coordination(location, environment)
            .map_err(|error| store_error(environment, &error))?;
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
                    Err(error) => return Err(store_error(environment, &error)),
                }
            }
            LeaseDecision::Held(holder) => {
                let waited_out = wait.is_none_or(|wait| now >= wait.after(started));
                if waited_out {
                    return Err(Finding::new(
                        Code::LeaseHeld,
                        format!("{environment} is held by {}", holder.describe()),
                        "wait for that run to finish, pass --wait-lease <duration>, or run `nyl lease break` if its runner is gone",
                    ));
                }
                if let Some(signal) = cancellation.signal() {
                    return Err(cancelled_waiting(environment, signal));
                }
                // A pause point: the scenario harness holds a waiting run here.
                let _ = context.faults.at(FaultPoint::WaitingLease, None);
                context.clock.wait(context.settings.lease_poll);
                if let Some(signal) = cancellation.signal() {
                    return Err(cancelled_waiting(environment, signal));
                }
            }
        }
    }
}

/// `now` plus a phase and the grace period: no phase runs without a deadline.
pub fn deadline(context: &Context<'_>, now: Timestamp, phase: Duration) -> Timestamp {
    phase.saturating_add(context.settings.lease_grace).after(now)
}

/// The latest deadline of work done under a lease with deadline `lease`:
/// the lease's deadline less the grace period, after which another runner
/// may take the lease over.
pub fn bound(settings: &Settings, lease: Timestamp) -> Timestamp {
    lease
        .checked_sub_signed(settings.lease_grace.to_chrono())
        .unwrap_or(lease)
}

/// Move the held lease to `units` and `deadline` by compare-and-swap; on
/// success `held` carries the new lease and version.
pub fn update(
    context: &Context<'_>,
    held: &mut HeldLease,
    units: Vec<UnitName>,
    deadline: Timestamp,
) -> Result<(), StateError> {
    let mut lease = held.lease.clone();
    lease.units = units;
    lease.deadline = deadline;
    held.version = context
        .store
        .update_lease(&held.location, &held.environment, &lease, &held.version)?;
    held.lease = lease;
    Ok(())
}

/// Release a lease this run holds. A lease that was taken over meanwhile is
/// not this run's to release.
///
/// A broken lease declares its holder gone only while it exists
/// (`rules::lease`). When this run took over a broken lease and the broken
/// run's ref is still on the remote, unconsumed, the break is put back by
/// compare-and-swap from this run's lease version instead of deleting the
/// lease ref: it keeps the original request and names the original run, so
/// the next run still takes over at once and records the break ("Runs and
/// leases" item 8).
pub fn release(context: &Context<'_>, held: &HeldLease) -> Result<(), StateError> {
    if let Some(broken) = held.replaced.as_ref().filter(|lease| lease.broken.is_some()) {
        let coordination = context.store.coordination(&held.location, &held.environment)?;
        let pending = coordination
            .runs
            .iter()
            .any(|state| !state.foreign && state.run.run == broken.run && !state.run.finished);
        if pending {
            return match context
                .store
                .take_lease(&held.location, &held.environment, broken, Some(&held.version))
            {
                Ok(_) | Err(StateError::LeaseRace | StateError::LeaseLost) => Ok(()),
                Err(error) => Err(error),
            };
        }
    }
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
