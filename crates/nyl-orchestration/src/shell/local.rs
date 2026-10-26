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

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId};
use nyl_core::orchestration::ports::{
    update_lease_then_run, Checkpoint, Committed, Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry,
    StateError, StateResult, StateSide, StateStore, StateTips, Transition,
};
use nyl_core::orchestration::state::{DesiredUnit, Lease, Run, Signal, StateLocation};

use super::run::RunRequest;
use super::store_error;
use crate::rules;
use crate::run_report::{EnvironmentRunReport, UnitResult};

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

/// The state store of a `--local` run, wrapping the remote location's store.
/// Every location it is called with is the remote (configured) location;
/// it works on that location's local view ([`rules::local::view`]).
pub struct LocalStore<'s> {
    inner: &'s dyn StateStore,
    run: LocalRun,
}

impl<'s> LocalStore<'s> {
    pub fn new(inner: &'s dyn StateStore, run: LocalRun) -> Self {
        Self { inner, run }
    }

    /// The local view of `location`. A location without one fails here;
    /// the command line refuses it with its finding beforehand.
    fn view(location: &StateLocation) -> StateResult<StateLocation> {
        rules::local::view(location).map_err(|finding| StateError::Unavailable(finding.message))
    }

    /// Where the lease lives: the remote location, or with `--local-lease`
    /// its local view.
    fn lease_location(&self, location: &StateLocation) -> StateResult<StateLocation> {
        match self.run.lease {
            LeaseScope::Remote => Ok(location.clone()),
            LeaseScope::Local => Self::view(location),
        }
    }

    /// The lease as written: at the remote without `units`, because remote
    /// runs do not notice local effects, so a remote run taking over from a
    /// dead local run must mark nothing uncertain in remote state.
    fn lease_written(&self, lease: &Lease) -> Lease {
        let mut lease = lease.clone();
        if self.run.lease == LeaseScope::Remote {
            lease.units.clear();
        }
        lease
    }
}

impl StateStore for LocalStore<'_> {
    /// The local view, after reading the remote so that local branches that
    /// follow it fast-forward first.
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
        let view = Self::view(location)?;
        self.inner.snapshot(location)?;
        self.inner.snapshot(&view)
    }

    fn snapshot_at(&self, location: &StateLocation, tips: &StateTips) -> StateResult<EnvironmentSnapshot> {
        self.inner.snapshot_at(&Self::view(location)?, tips)
    }

    /// [`LeaseScope::Remote`]: the remote lease and the run refs of both
    /// the view and the remote, the remote ones marked
    /// [`RunState::foreign`](nyl_core::orchestration::ports::RunState::foreign)
    /// so a live CI run ref still blocks but is never imported.
    /// [`LeaseScope::Local`]: the view's lease and only the view's run refs,
    /// accepting that CI may run at the same time. Evidence refs: the view's.
    fn coordination(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<Coordination> {
        let local = self.inner.coordination(&Self::view(location)?, environment)?;
        match self.run.lease {
            LeaseScope::Local => Ok(local),
            LeaseScope::Remote => {
                let remote = self.inner.coordination(location, environment)?;
                let mut runs = local.runs;
                runs.extend(remote.runs.into_iter().map(|mut state| {
                    state.foreign = true;
                    state
                }));
                Ok(Coordination {
                    lease: remote.lease,
                    runs,
                    evidence: local.evidence,
                })
            }
        }
    }

    /// Under [`LeaseScope::Remote`] the lease is written with its `units`
    /// cleared (as is every update), so a remote run taking over from a dead
    /// local run marks nothing uncertain in remote state.
    fn take_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: Option<&str>,
    ) -> StateResult<String> {
        self.inner.take_lease(
            &self.lease_location(location)?,
            environment,
            &self.lease_written(lease),
            expected,
        )
    }

    fn update_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: &str,
    ) -> StateResult<String> {
        self.inner.update_lease(
            &self.lease_location(location)?,
            environment,
            &self.lease_written(lease),
            expected,
        )
    }

    /// One write in the view under [`LeaseScope::Local`]; under
    /// [`LeaseScope::Remote`] the lease and the run ref live in different
    /// repositories, and are written one after the other.
    fn update_lease_and_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        version: &mut String,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        match self.run.lease {
            LeaseScope::Local => {
                self.inner
                    .update_lease_and_run(&Self::view(location)?, environment, lease, version, run, created)
            }
            LeaseScope::Remote => update_lease_then_run(self, location, environment, lease, version, run, created),
        }
    }

    fn release_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        expected: &str,
    ) -> StateResult<()> {
        self.inner
            .release_lease(&self.lease_location(location)?, environment, expected)
    }

    fn delete_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()> {
        self.inner.delete_lease(&self.lease_location(location)?, environment)
    }

    fn write_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        checkpoint: Option<&Checkpoint>,
    ) -> StateResult<()> {
        self.inner
            .write_run(&Self::view(location)?, environment, run, checkpoint)
    }

    fn write_run_created(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        self.inner
            .write_run_created(&Self::view(location)?, environment, run, created)
    }

    fn delete_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        self.inner
            .delete_run(&Self::view(location)?, environment, run, expected)
    }

    /// Retires the view's run ref; never a remote one.
    fn retire_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        self.inner
            .retire_run(&Self::view(location)?, environment, run, expected)
    }

    fn delete_evidence(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        self.inner.delete_evidence(&Self::view(location)?, environment, run)
    }

    fn signals(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
    ) -> StateResult<Vec<Signal>> {
        self.inner.signals(&Self::view(location)?, environment, run)
    }

    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()> {
        self.inner.send_signal(&Self::view(location)?, environment, signal)
    }

    fn delete_signals(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        self.inner.delete_signals(&Self::view(location)?, environment, run)
    }

    /// Commits to the local branches through [`StateStore::commit_local`],
    /// fenced by the lease where the run holds it.
    fn commit(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_version: &str,
    ) -> StateResult<Committed> {
        self.inner.commit_local(
            location,
            environment,
            base,
            transition,
            &self.lease_location(location)?,
            lease_version,
        )
    }

    /// Refused: state never moves during a `--local` run.
    fn import_history(&self, _from: &StateLocation, _to: &StateLocation) -> StateResult<()> {
        Err(StateError::Unavailable("a --local run never moves state".to_owned()))
    }

    /// The local branch's history.
    fn log(&self, location: &StateLocation, side: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
        self.inner.log(&Self::view(location)?, side, limit)
    }

    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        self.inner.local_divergence(location)
    }
}

/// The state refs of a location, one when both are the same ref.
pub(crate) fn sides(location: &StateLocation) -> &'static [StateSide] {
    if location.desired_ref == location.observed_ref {
        &[StateSide::Desired]
    } else {
        &StateSide::ALL
    }
}

/// How far back [`check_remote_tips`] first looks for the environment's
/// last commit without the `Nyl-Local` trailer.
pub const TIP_HISTORY: usize = 100;

/// How far back [`environment_log`] looks at most for an environment's
/// commits on a ref that several environments share.
const SHARED_HISTORY: usize = 10_000;

/// The commits of `environment` on one state ref of `location`, newest
/// first ([`rules::local::environment_log`]), read `limit` entries back. A
/// ref that several environments share through `path` is read further back
/// (up to [`SHARED_HISTORY`]) until it shows one of the environment's
/// commits, so another environment's recent history cannot hide its tip.
pub(crate) fn environment_log(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
    side: StateSide,
    limit: usize,
) -> StateResult<Vec<LogEntry>> {
    let mut limit = limit;
    loop {
        let log = store.log(location, side, limit)?;
        let own = rules::local::environment_log(environment, &log);
        if !own.is_empty() || log.len() < limit || limit >= SHARED_HISTORY {
            return Ok(own);
        }
        limit = (limit * 10).min(SHARED_HISTORY);
    }
}

/// Refuse when the newest commit of `environment` on a state ref of the
/// remote `location` is a `Nyl-Local` commit (`NYL-STATE-LOCAL-TIP`), naming
/// its last commit without the trailer. Several environments may share the
/// refs through `path`, so only commits whose `Nyl-Environment` trailer names
/// `environment` count: another environment's pushed local transitions never
/// refuse this one, and a later commit of another environment never hides
/// this one's. A [`StateLocation::LOCAL_REPOSITORY`] location, whose local
/// branches are the state, passes. So does a store whose
/// [`StateStore::log`] returns `StateError::Unavailable`: a store without
/// history cannot hold `Nyl-Local` commits (the Git and memory stores
/// implement `log`). Every state-writing operation against remote state
/// calls it after reading its snapshot, except `state restore`, the fix.
pub fn check_remote_tips(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
) -> Result<(), Finding> {
    if location.is_local() {
        return Ok(());
    }
    for side in sides(location) {
        let log = match environment_log(store, location, environment, *side, TIP_HISTORY) {
            Ok(log) => log,
            Err(StateError::Unavailable(_)) => return Ok(()),
            Err(error) => {
                return Err(Finding::new(
                    Code::StateUnavailable,
                    format!(
                        "cannot read the history of the {} state ref {}: {error}",
                        side.as_str(),
                        location.state_ref(*side)
                    ),
                    "check access to the state repository and retry",
                ))
            }
        };
        rules::local::check_tip(environment, *side, &log)?;
    }
    Ok(())
}

/// [`rules::state_location::check_record`], explaining missing remote state
/// whose clone holds it on local branches: the state started without a
/// remote, and `nyl state move -e <env> --from local` takes it there
/// ([`rules::local::missing_on_remote`]). Reading the local branches is best
/// effort: when it fails, the plain finding stands.
pub fn check_record(
    store: &dyn StateStore,
    record: Option<&nyl_core::orchestration::state::StateRecord>,
    environment: &EnvironmentName,
    location: &StateLocation,
) -> Result<(), Finding> {
    let Err(finding) = rules::state_location::check_record(record, environment, location) else {
        return Ok(());
    };
    if finding.code != Code::StateMissing || location.is_local() {
        return Err(finding);
    }
    let local = rules::local::view(location)
        .ok()
        .and_then(|view| store.snapshot(&view).ok())
        .and_then(|snapshot| snapshot.record);
    Err(rules::local::missing_on_remote(environment, location, local.as_ref()).unwrap_or(finding))
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use nyl_core::orchestration::ports::{HeldLease, RunState};
    use serde_json::json;

    use super::*;

    const RUN_A: &str = "00000000-0000-7000-9000-00000000000a";
    const RUN_B: &str = "00000000-0000-7000-9000-00000000000b";

    fn remote() -> StateLocation {
        StateLocation {
            repository: "https://example.com/state.git".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        }
    }

    fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    fn lease(units: &[&str]) -> Lease {
        serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "Lease", "run": RUN_A, "runner": "me",
            "operation": "reconcile", "units": units, "deadline": "2026-09-24T11:00:00Z",
            "takenAt": "2026-09-24T10:00:00Z"
        }))
        .unwrap()
    }

    fn run_state(run: &str) -> RunState {
        RunState {
            run: serde_json::from_value(json!({
                "apiVersion": "gitops.nyl/v1", "kind": "Run", "run": run, "runner": "ci",
                "operation": "reconcile", "deadline": "2026-09-24T11:00:00Z"
            }))
            .unwrap(),
            checkpoints: Vec::new(),
            created: Vec::new(),
            version: "v".to_owned(),
            foreign: false,
        }
    }

    /// A store recording which location every call reached, with one run
    /// ref per repository and the given history.
    #[derive(Default)]
    struct Recording {
        calls: Mutex<Vec<(String, String)>>,
        leases: Mutex<Vec<Lease>>,
        log: Vec<LogEntry>,
        log_unavailable: bool,
    }

    impl Recording {
        fn record(&self, call: &str, location: &StateLocation) {
            self.calls
                .lock()
                .unwrap()
                .push((call.to_owned(), location.repository.clone()));
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl StateStore for Recording {
        fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
            self.record("snapshot", location);
            Ok(EnvironmentSnapshot::default())
        }
        fn coordination(&self, location: &StateLocation, _: &EnvironmentName) -> StateResult<Coordination> {
            self.record("coordination", location);
            let local = location.is_local();
            Ok(Coordination {
                lease: (!local).then(|| HeldLease {
                    lease: lease(&[]),
                    version: "remote".to_owned(),
                }),
                runs: vec![run_state(if local { RUN_A } else { RUN_B })],
                evidence: vec![run_state(if local { RUN_A } else { RUN_B })],
            })
        }
        fn take_lease(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            lease: &Lease,
            _: Option<&str>,
        ) -> StateResult<String> {
            self.record("take_lease", location);
            self.leases.lock().unwrap().push(lease.clone());
            Ok("v1".to_owned())
        }
        fn update_lease(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            lease: &Lease,
            _: &str,
        ) -> StateResult<String> {
            self.record("update_lease", location);
            self.leases.lock().unwrap().push(lease.clone());
            Ok("v2".to_owned())
        }
        fn release_lease(&self, location: &StateLocation, _: &EnvironmentName, _: &str) -> StateResult<()> {
            self.record("release_lease", location);
            Ok(())
        }
        fn delete_lease(&self, location: &StateLocation, _: &EnvironmentName) -> StateResult<()> {
            self.record("delete_lease", location);
            Ok(())
        }
        fn write_run(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            _: &Run,
            _: Option<&Checkpoint>,
        ) -> StateResult<()> {
            self.record("write_run", location);
            Ok(())
        }
        fn delete_run(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            _: &RunId,
            _: Option<&str>,
        ) -> StateResult<()> {
            self.record("delete_run", location);
            Ok(())
        }
        fn retire_run(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            _: &RunId,
            _: Option<&str>,
        ) -> StateResult<()> {
            self.record("retire_run", location);
            Ok(())
        }
        fn delete_evidence(&self, location: &StateLocation, _: &EnvironmentName, _: &RunId) -> StateResult<()> {
            self.record("delete_evidence", location);
            Ok(())
        }
        fn signals(&self, location: &StateLocation, _: &EnvironmentName, _: &RunId) -> StateResult<Vec<Signal>> {
            self.record("signals", location);
            Ok(Vec::new())
        }
        fn send_signal(&self, location: &StateLocation, _: &EnvironmentName, _: &Signal) -> StateResult<()> {
            self.record("send_signal", location);
            Ok(())
        }
        fn commit(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            _: &StateTips,
            _: &Transition,
            _: &str,
        ) -> StateResult<Committed> {
            self.record("commit", location);
            Ok(Committed::default())
        }
        fn import_history(&self, from: &StateLocation, _: &StateLocation) -> StateResult<()> {
            self.record("import_history", from);
            Ok(())
        }
        fn log(&self, location: &StateLocation, _: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
            self.record("log", location);
            if self.log_unavailable {
                return Err(StateError::Unavailable("no history".to_owned()));
            }
            Ok(self.log.iter().take(limit).cloned().collect())
        }
        fn commit_local(
            &self,
            location: &StateLocation,
            _: &EnvironmentName,
            _: &StateTips,
            _: &Transition,
            lease_location: &StateLocation,
            _: &str,
        ) -> StateResult<Committed> {
            self.record("commit_local", location);
            self.record("commit_local_lease", lease_location);
            Ok(Committed::default())
        }
    }

    fn local_run(lease: LeaseScope) -> LocalRun {
        LocalRun {
            lease,
            approve: std::collections::BTreeSet::new(),
        }
    }

    const REMOTE: &str = "https://example.com/state.git";
    const LOCAL: &str = StateLocation::LOCAL_REPOSITORY;

    /// Catches a local run whose lease, run refs, or commits reach the
    /// wrong repository: a remote run ref imported as its own, a lease
    /// listing local units at the remote, or a commit pushed to the remote.
    #[test]
    fn test_local_store_routes_state_to_the_clone_and_the_lease_to_the_remote() {
        let inner = Recording::default();
        let store = LocalStore::new(&inner, local_run(LeaseScope::Remote));
        let location = remote();
        let coordination = store.coordination(&location, &dev()).unwrap();
        assert_eq!(coordination.lease.unwrap().version, "remote");
        let runs = coordination
            .runs
            .iter()
            .map(|state| (state.run.run.to_string(), state.foreign))
            .collect::<Vec<_>>();
        assert_eq!(runs, vec![(RUN_A.to_owned(), false), (RUN_B.to_owned(), true)]);
        assert_eq!(coordination.evidence.len(), 1);
        assert_eq!(coordination.evidence[0].run.run.as_str(), RUN_A);

        store.take_lease(&location, &dev(), &lease(&["network"]), None).unwrap();
        store
            .update_lease(&location, &dev(), &lease(&["network"]), "v1")
            .unwrap();
        let mut version = "v2".to_owned();
        let run = run_state(RUN_A).run;
        store
            .update_lease_and_run(&location, &dev(), &lease(&["network"]), &mut version, &run, &[])
            .unwrap();
        assert!(inner.leases.lock().unwrap().iter().all(|lease| lease.units.is_empty()));
        store
            .commit(
                &location,
                &dev(),
                &StateTips::default(),
                &Transition {
                    message: String::new(),
                    changes: nyl_core::orchestration::ports::TransitionChanges::default(),
                },
                "v2",
            )
            .unwrap();
        store
            .delete_run(&location, &dev(), &RunId::new(RUN_A).unwrap(), None)
            .unwrap();
        store
            .retire_run(&location, &dev(), &RunId::new(RUN_A).unwrap(), None)
            .unwrap();
        store.release_lease(&location, &dev(), "v3").unwrap();
        store.snapshot(&location).unwrap();
        assert!(store.import_history(&location, &location).is_err());
        let calls = inner.calls();
        let reached = |call: &str| {
            calls
                .iter()
                .filter(|(name, _)| name == call)
                .map(|(_, repository)| repository.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(reached("take_lease"), vec![REMOTE]);
        assert_eq!(reached("update_lease"), vec![REMOTE, REMOTE]);
        assert_eq!(reached("write_run"), vec![LOCAL]);
        assert_eq!(reached("release_lease"), vec![REMOTE]);
        assert_eq!(reached("commit"), Vec::<&str>::new());
        assert_eq!(reached("commit_local"), vec![REMOTE]);
        assert_eq!(reached("commit_local_lease"), vec![REMOTE]);
        assert_eq!(reached("delete_run"), vec![LOCAL]);
        assert_eq!(reached("retire_run"), vec![LOCAL]);
        // The remote first, so local branches following it fast-forward.
        assert_eq!(reached("snapshot"), vec![REMOTE, LOCAL]);
        assert_eq!(reached("import_history"), Vec::<&str>::new());
    }

    /// Catches `--local-lease` still blocking on, or importing, a CI run's
    /// remote refs, or keeping the units out of its own lease.
    #[test]
    fn test_local_lease_reads_and_writes_only_the_clone() {
        let inner = Recording::default();
        let store = LocalStore::new(&inner, local_run(LeaseScope::Local));
        let location = remote();
        let coordination = store.coordination(&location, &dev()).unwrap();
        assert!(coordination.lease.is_none());
        assert_eq!(coordination.runs.len(), 1);
        assert!(!coordination.runs[0].foreign);
        store.take_lease(&location, &dev(), &lease(&["network"]), None).unwrap();
        assert_eq!(inner.leases.lock().unwrap()[0].units.len(), 1);
        store
            .commit(
                &location,
                &dev(),
                &StateTips::default(),
                &Transition {
                    message: String::new(),
                    changes: nyl_core::orchestration::ports::TransitionChanges::default(),
                },
                "v1",
            )
            .unwrap();
        let calls = inner.calls();
        assert!(calls
            .iter()
            .all(|(name, repository)| repository == LOCAL || name == "commit_local"));
        assert!(calls.contains(&("commit_local_lease".to_owned(), LOCAL.to_owned())));
    }

    fn entry(digit: char, local: bool) -> LogEntry {
        entry_of("dev", &digit.to_string().repeat(40), local)
    }

    fn entry_of(environment: &str, commit: &str, local: bool) -> LogEntry {
        let mut message =
            format!("nyl: reconcile {environment}\n\nNyl-Run-Id: {RUN_A}\nNyl-Environment: {environment}\n");
        if local {
            message.push_str("Nyl-Local: true\n");
        }
        LogEntry {
            commit: nyl_core::orchestration::ids::CommitId::new(commit).unwrap(),
            message,
        }
    }

    /// Catches a run continuing on a pushed local transition, or a store
    /// without history refusing every run.
    #[test]
    fn test_check_remote_tips_refuses_a_local_tip_and_passes_without_history() {
        let pushed = Recording {
            log: vec![entry('2', true), entry('1', false)],
            ..Recording::default()
        };
        let finding = check_remote_tips(&pushed, &remote(), &dev()).unwrap_err();
        assert_eq!(finding.code, Code::StateLocalTip);
        assert!(finding.fix.contains(&"1".repeat(40)), "{}", finding.fix);
        let clean = Recording {
            log: vec![entry('1', false)],
            ..Recording::default()
        };
        assert!(check_remote_tips(&clean, &remote(), &dev()).is_ok());
        let without_history = Recording {
            log_unavailable: true,
            ..Recording::default()
        };
        assert!(check_remote_tips(&without_history, &remote(), &dev()).is_ok());
        // State on the clone's own branches has no remote to protect.
        let mut local = remote();
        local.repository = LOCAL.to_owned();
        assert!(check_remote_tips(&pushed, &local, &dev()).is_ok());
        assert_eq!(pushed.calls().iter().filter(|(name, _)| name == "log").count(), 1);
    }

    /// Catches environments sharing refs through `path` refusing each other:
    /// another environment's pushed local commit at the tip refusing this
    /// one (and naming the wrong environment), or a later commit of another
    /// environment, even beyond the first read, hiding this one's local tip.
    #[test]
    fn test_check_remote_tips_looks_at_the_environments_own_commits() {
        let preview = EnvironmentName::new("preview").unwrap();
        let shared = Recording {
            log: vec![
                entry_of("preview", &"b".repeat(40), true),
                entry_of("dev", &"a".repeat(40), false),
            ],
            ..Recording::default()
        };
        assert!(check_remote_tips(&shared, &remote(), &dev()).is_ok());
        let finding = check_remote_tips(&shared, &remote(), &preview).unwrap_err();
        assert_eq!(finding.code, Code::StateLocalTip);
        assert!(finding.message.contains("of preview"), "{}", finding.message);
        assert!(
            finding.fix.contains("nyl state restore -e preview --to <commit>"),
            "{}",
            finding.fix
        );

        let mut log = (0..TIP_HISTORY + 50)
            .map(|index| entry_of("preview", &format!("{index:040x}"), false))
            .collect::<Vec<_>>();
        log.push(entry_of("dev", &"c".repeat(40), true));
        log.push(entry_of("dev", &"d".repeat(40), false));
        let hidden = Recording {
            log,
            ..Recording::default()
        };
        let finding = check_remote_tips(&hidden, &remote(), &dev()).unwrap_err();
        assert_eq!(finding.code, Code::StateLocalTip);
        assert!(finding.fix.contains(&"d".repeat(40)), "{}", finding.fix);
        assert!(check_remote_tips(&hidden, &remote(), &preview).is_ok());
    }
}
