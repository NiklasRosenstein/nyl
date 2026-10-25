//! `--local` runs next to remote state, and the checks every run against
//! remote state makes on local transitions.
//!
//! Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
//! The decisions are in [`crate::rules::local`]; this module performs the
//! reads and writes:
//!
//! - [`LocalStore`] is the state store a `--local` run works through: it
//!   reads and writes the clone's local branches (the local view of the
//!   location) and the run refs there, takes the remote lease
//!   (`refs/nyl/<env>/lease`) or, with `--local-lease`, the local one, and
//!   commits with [`StateStore::commit_local`], never pushing state. The
//!   command line passes it as the [`super::Context`]'s store, so the run
//!   shell takes the same path as an ordinary run.
//! - [`check_remote_tips`] refuses a run against remote state whose state
//!   ref tip is a `Nyl-Local` commit.
//! - [`finish_local_report`] adds a local run's warnings.
//! - [`divergence`] reports diverged local branches for `nyl status`.

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, RunId};
use nyl_core::orchestration::ports::{
    Committed, Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry, StateResult, StateSide, StateStore,
    StateTips, Transition,
};
use nyl_core::orchestration::state::{DesiredUnit, Lease, Run, Signal, StateLocation};

use super::run::RunRequest;
use crate::run_report::EnvironmentRunReport;

/// Where a `--local` run takes its lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseScope {
    /// The remote lease, so a local run and a CI run never execute at once
    /// (the default; needs push access to the lease ref).
    Remote,
    /// `--local-lease`: the clone's own lease, accepting that CI may run at
    /// the same time.
    Local,
}

/// A `--local` run's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalRun {
    pub lease: LeaseScope,
}

/// The state store of a `--local` run, wrapping the remote location's store.
/// Every location it is called with is the remote (configured) location.
pub struct LocalStore<'s> {
    inner: &'s dyn StateStore,
    run: LocalRun,
}

impl<'s> LocalStore<'s> {
    pub fn new(inner: &'s dyn StateStore, run: LocalRun) -> Self {
        Self { inner, run }
    }
}

impl StateStore for LocalStore<'_> {
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
        let _ = (self.inner, self.run, location);
        todo!("local")
    }

    fn snapshot_at(&self, location: &StateLocation, tips: &StateTips) -> StateResult<EnvironmentSnapshot> {
        let _ = (location, tips);
        todo!("local")
    }

    /// The lease where [`LeaseScope`] says, and the run refs of both the
    /// remote and the local view, so a live CI run ref still blocks.
    fn coordination(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<Coordination> {
        let _ = (location, environment);
        todo!("local")
    }

    fn take_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: Option<&str>,
    ) -> StateResult<String> {
        let _ = (location, environment, lease, expected);
        todo!("local")
    }

    fn update_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: &str,
    ) -> StateResult<String> {
        let _ = (location, environment, lease, expected);
        todo!("local")
    }

    fn release_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        expected: &str,
    ) -> StateResult<()> {
        let _ = (location, environment, expected);
        todo!("local")
    }

    fn delete_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()> {
        let _ = (location, environment);
        todo!("local")
    }

    fn write_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        checkpoint: Option<&nyl_core::orchestration::ports::Checkpoint>,
    ) -> StateResult<()> {
        let _ = (location, environment, run, checkpoint);
        todo!("local")
    }

    fn write_run_created(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        let _ = (location, environment, run, created);
        todo!("local")
    }

    fn delete_run(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        let _ = (location, environment, run);
        todo!("local")
    }

    fn signals(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
    ) -> StateResult<Vec<Signal>> {
        let _ = (location, environment, run);
        todo!("local")
    }

    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()> {
        let _ = (location, environment, signal);
        todo!("local")
    }

    /// Commits to the local branches through [`StateStore::commit_local`].
    fn commit(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_version: &str,
    ) -> StateResult<Committed> {
        let _ = (location, environment, base, transition, lease_version);
        todo!("local")
    }

    fn import_history(&self, from: &StateLocation, to: &StateLocation) -> StateResult<()> {
        let _ = (from, to);
        todo!("local")
    }

    fn log(&self, location: &StateLocation, side: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
        let _ = (location, side, limit);
        todo!("local")
    }

    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        self.inner.local_divergence(location)
    }
}

/// Refuse when a state ref of the remote `location` has a `Nyl-Local`
/// commit at its tip (`NYL-STATE-LOCAL-TIP`), naming the last commit without
/// the trailer. A [`StateLocation::LOCAL_REPOSITORY`] location, whose local
/// branches are the state, passes. Every state-writing operation against
/// remote state calls it after reading its snapshot.
pub fn check_remote_tips(store: &dyn StateStore, location: &StateLocation) -> Result<(), Finding> {
    // Package `local` implements the check; until then every tip passes.
    let _ = (store, location);
    Ok(())
}

/// Add a `--local` run's warnings to its report: `NYL-LOCAL-RUN-UNRECORDED`
/// for every unit it executed, naming the remote command that records it.
pub fn finish_local_report(report: &mut EnvironmentRunReport, request: &RunRequest, local: &LocalRun) {
    let _ = (report, request, local);
    todo!("local")
}

/// `NYL-STATE-LOCAL-DIVERGED` when the clone's local branches of `location`
/// hold local transitions or are behind the remote; `None` otherwise.
pub fn divergence(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
) -> Result<Option<Finding>, Finding> {
    let _ = (store, location, environment);
    todo!("local")
}
