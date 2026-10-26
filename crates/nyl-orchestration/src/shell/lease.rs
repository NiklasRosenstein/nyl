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
use crate::rules::lease::{decide, BrokenLease, LeaseDecision};

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
/// run holding it is `NYL-LEASE-HELD`, exit 2. A broken lease whose run is
/// not imported yet is taken over only with [`BrokenLease::TakeOver`];
/// otherwise it is `NYL-LEASE-BROKEN`, exit 2, waited for like a held lease.
#[allow(clippy::too_many_arguments)] // Each is one aspect of the request; a struct would only rename them.
pub fn acquire(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    run: &RunId,
    operation: Operation,
    broken: BrokenLease,
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
        match decide(&coordination, now, broken) {
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
            decision @ (LeaseDecision::Held(_) | LeaseDecision::Broken(_)) => {
                let waited_out = wait.is_none_or(|wait| now >= wait.after(started));
                if waited_out {
                    return Err(match decision {
                        LeaseDecision::Broken(lease) => broken_lease(environment, &lease),
                        LeaseDecision::Held(holder) => Finding::new(
                            Code::LeaseHeld,
                            format!("{environment} is held by {}", holder.describe()),
                            "wait for that run to finish, pass --wait-lease <duration>, or run `nyl lease break` if its runner is gone",
                        ),
                        LeaseDecision::Take { .. } => unreachable!("matched held or broken"),
                    });
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

/// `NYL-LEASE-BROKEN`: the broken lease's run waits for an operation that
/// imports it.
pub fn broken_lease(environment: &EnvironmentName, lease: &Lease) -> Finding {
    Finding::new(
        Code::LeaseBroken,
        format!(
            "the lease of {environment} was broken and its run {} has not been imported yet",
            lease.run
        ),
        format!(
            "run `nyl recover -e {environment}` to import it (a `nyl reconcile` or `nyl teardown` of {environment} imports it too), then run this command again"
        ),
    )
}

/// Whether a run on the shared run path takes a broken lease over: a
/// `reconcile`, `teardown`, or `recover` imports the broken run, except a
/// `--local` run under the remote lease, which never imports remote run
/// refs and so refuses a broken remote lease with `NYL-LEASE-BROKEN`
/// (a `--local-lease` run takes only the clone's own lease). Contract:
/// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
/// item 1, and [Local runs](../../../../design/orchestration-core.md#local-runs).
pub fn run_takeover(local: Option<&super::local::LocalRun>) -> BrokenLease {
    match local {
        Some(local) if local.lease == super::local::LeaseScope::Remote => BrokenLease::Refuse,
        _ => BrokenLease::TakeOver,
    }
}

/// Whether a run holding `held` must write a transition commit although
/// nothing changed, to record the lease break it took over once
/// ([`TransitionChanges::commit_unchanged`](nyl_core::orchestration::ports::TransitionChanges::commit_unchanged)):
/// exactly when it took a broken lease over. Contract:
/// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases) item 8.
pub fn records_break(held: &HeldLease) -> bool {
    held.broken.is_some()
}

/// Release a lease this run holds. A lease that was taken over meanwhile is
/// not this run's to release. `landed` says whether the operation's
/// transition commit landed (or it ended without changes after importing).
///
/// An importer that took a broken lease over and ends before that, such as
/// a reconcile failing during resolution or a cancelled one, puts the broken
/// lease back by compare-and-swap from its own lease version instead of
/// deleting the lease ref: it keeps the original request and names the
/// original run, so the break stays in effect until an import is committed
/// ("Runs and leases" item 8). Once the commit landed, the break is
/// recorded and the lease is released normally; a broken holder that is
/// still alive and checkpoints afterwards leaves its run ref for the next
/// import.
pub fn release(context: &Context<'_>, held: &HeldLease, landed: bool) -> Result<(), StateError> {
    if let Some(broken) = held.replaced.as_ref().filter(|lease| !landed && lease.broken.is_some()) {
        return match context
            .store
            .take_lease(&held.location, &held.environment, broken, Some(&held.version))
        {
            Ok(_) | Err(StateError::LeaseRace | StateError::LeaseLost) => Ok(()),
            Err(error) => Err(error),
        };
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
