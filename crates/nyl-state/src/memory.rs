//! An in-memory [`StateStore`] with the same compare-and-swap semantics as
//! the Git store, for the contract walkthroughs and unit tests.
//!
//! State refs are kept per repository and ref name, each holding the
//! environment directories that `state.path` places in it, so environments
//! sharing a ref share its commit history as they would in Git. Commit IDs
//! are deterministic digests of a counter, never real Git objects.

use std::collections::BTreeMap;
use std::sync::Mutex;

use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Committed, Coordination, EnvironmentSnapshot, HeldLease, RunState, StateError, StateResult, StateStore,
    StateTips, Transition,
};
use nyl_core::orchestration::state::{
    ArtifactDocument, DesiredUnit, EnvironmentRecord, Lease, ObservedUnit, Run, Signal, StateLocation, StateRecord,
};

/// The state of one environment directory on one ref.
#[derive(Debug, Clone, Default, PartialEq)]
struct Directory {
    record: Option<StateRecord>,
    environment: Option<EnvironmentRecord>,
    desired: BTreeMap<UnitName, DesiredUnit>,
    observed: BTreeMap<UnitName, ObservedUnit>,
    artifacts: BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
}

/// One ref: its tip and the directories it holds, by `state.path`.
#[derive(Debug, Clone, Default)]
struct StateRef {
    tip: Option<CommitId>,
    commits: usize,
    directories: BTreeMap<String, Directory>,
}

#[derive(Debug, Default)]
struct Inner {
    counter: u64,
    refs: BTreeMap<(String, String), StateRef>,
    leases: BTreeMap<(String, String, EnvironmentName), HeldLease>,
    runs: BTreeMap<(String, String, EnvironmentName), BTreeMap<RunId, RunState>>,
    signals: BTreeMap<(String, String, EnvironmentName, RunId), Vec<Signal>>,
}

impl Inner {
    fn next_version(&mut self) -> String {
        self.counter += 1;
        self.counter.to_string()
    }

    fn next_commit(&mut self) -> CommitId {
        self.counter += 1;
        let digest = nyl_core::digest::sha256_hex(self.counter.to_string().as_bytes());
        CommitId::new(&digest[..40]).expect("a digest prefix is a commit ID")
    }
}

/// An in-memory state store. Clones share nothing; one store stands for one
/// set of repositories.
#[derive(Debug, Default)]
pub struct MemoryStateStore {
    inner: Mutex<Inner>,
}

fn ref_key(location: &StateLocation, reference: &str) -> (String, String) {
    (location.repository.clone(), reference.to_owned())
}

fn coordination_key(location: &StateLocation, environment: &EnvironmentName) -> (String, String, EnvironmentName) {
    (
        location.repository.clone(),
        location.coordination_ref_prefix.clone(),
        environment.clone(),
    )
}

impl MemoryStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many commits a state ref received, for assertions such as a
    /// transition commit per ref.
    pub fn commit_count(&self, repository: &str, reference: &str) -> usize {
        let inner = self.inner.lock().expect("store lock");
        inner
            .refs
            .get(&(repository.to_owned(), reference.to_owned()))
            .map_or(0, |state_ref| state_ref.commits)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("store lock")
    }
}

impl StateStore for MemoryStateStore {
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
        let inner = self.lock();
        let read = |reference: &str| inner.refs.get(&ref_key(location, reference));
        let desired = read(&location.desired_ref);
        let observed = read(&location.observed_ref);
        let desired_directory = desired.and_then(|state_ref| state_ref.directories.get(&location.path));
        let observed_directory = observed.and_then(|state_ref| state_ref.directories.get(&location.path));
        Ok(EnvironmentSnapshot {
            tips: StateTips {
                desired: desired.and_then(|state_ref| state_ref.tip.clone()),
                observed: observed.and_then(|state_ref| state_ref.tip.clone()),
            },
            record: desired_directory
                .and_then(|directory| directory.record.clone())
                .or_else(|| observed_directory.and_then(|directory| directory.record.clone())),
            environment: desired_directory.and_then(|directory| directory.environment.clone()),
            desired: desired_directory
                .map(|directory| directory.desired.clone())
                .unwrap_or_default(),
            observed: observed_directory
                .map(|directory| directory.observed.clone())
                .unwrap_or_default(),
            artifacts: observed_directory
                .map(|directory| directory.artifacts.clone())
                .unwrap_or_default(),
        })
    }

    fn coordination(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<Coordination> {
        let inner = self.lock();
        let key = coordination_key(location, environment);
        Ok(Coordination {
            lease: inner.leases.get(&key).cloned(),
            runs: inner
                .runs
                .get(&key)
                .map(|runs| runs.values().cloned().collect())
                .unwrap_or_default(),
        })
    }

    fn take_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: Option<&str>,
    ) -> StateResult<String> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        let current = inner.leases.get(&key).map(|held| held.version.as_str());
        if current != expected {
            return Err(StateError::LeaseRace);
        }
        let version = inner.next_version();
        inner.leases.insert(
            key,
            HeldLease {
                lease: lease.clone(),
                version: version.clone(),
            },
        );
        Ok(version)
    }

    fn update_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: &str,
    ) -> StateResult<String> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        match inner.leases.get(&key) {
            Some(held) if held.version == expected => {}
            _ => return Err(StateError::LeaseLost),
        }
        let version = inner.next_version();
        inner.leases.insert(
            key,
            HeldLease {
                lease: lease.clone(),
                version: version.clone(),
            },
        );
        Ok(version)
    }

    fn release_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        expected: &str,
    ) -> StateResult<()> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        match inner.leases.get(&key) {
            Some(held) if held.version == expected => {
                inner.leases.remove(&key);
                Ok(())
            }
            _ => Err(StateError::LeaseLost),
        }
    }

    fn break_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()> {
        self.lock().leases.remove(&coordination_key(location, environment));
        Ok(())
    }

    fn write_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        checkpoint: Option<&Checkpoint>,
    ) -> StateResult<()> {
        let mut inner = self.lock();
        let version = inner.next_version();
        let runs = inner.runs.entry(coordination_key(location, environment)).or_default();
        let state = runs.entry(run.run.clone()).or_insert_with(|| RunState {
            run: run.clone(),
            checkpoints: Vec::new(),
            version: String::new(),
        });
        state.run = run.clone();
        state.version = version;
        if let Some(checkpoint) = checkpoint {
            state.checkpoints.push(checkpoint.clone());
        }
        Ok(())
    }

    fn delete_run(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        if let Some(runs) = inner.runs.get_mut(&key) {
            runs.remove(run);
        }
        inner.signals.remove(&(key.0, key.1, key.2, run.clone()));
        Ok(())
    }

    fn signals(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
    ) -> StateResult<Vec<Signal>> {
        let inner = self.lock();
        let (repository, prefix, environment) = coordination_key(location, environment);
        Ok(inner
            .signals
            .get(&(repository, prefix, environment, run.clone()))
            .cloned()
            .unwrap_or_default())
    }

    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()> {
        let mut inner = self.lock();
        let (repository, prefix, environment) = coordination_key(location, environment);
        inner
            .signals
            .entry((repository, prefix, environment, signal.run.clone()))
            .or_default()
            .push(signal.clone());
        Ok(())
    }

    fn commit(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_version: &str,
    ) -> StateResult<Committed> {
        let mut inner = self.lock();
        match inner.leases.get(&coordination_key(location, environment)) {
            Some(held) if held.version == lease_version => {}
            _ => return Err(StateError::LeaseLost),
        }
        let same_ref = location.desired_ref == location.observed_ref;
        for (reference, expected) in [
            (&location.desired_ref, &base.desired),
            (&location.observed_ref, &base.observed),
        ] {
            let actual = inner
                .refs
                .get(&ref_key(location, reference))
                .and_then(|state_ref| state_ref.tip.clone());
            if &actual != expected {
                return Err(StateError::Conflict {
                    reference: reference.clone(),
                    actual: actual.map_or_else(|| "nothing".to_owned(), |commit| commit.to_string()),
                });
            }
        }
        let changes = &transition.changes;
        let desired_changed = changes.record.is_some() || changes.environment.is_some() || !changes.desired.is_empty();
        let observed_changed =
            changes.record.is_some() || !changes.observed.is_empty() || !changes.artifacts.is_empty();

        let mut committed = Committed::default();
        let apply = |inner: &mut Inner, reference: &str, desired_side: bool, observed_side: bool| -> CommitId {
            let commit = inner.next_commit();
            let state_ref = inner.refs.entry(ref_key(location, reference)).or_default();
            let directory = state_ref.directories.entry(location.path.clone()).or_default();
            if let Some(record) = &changes.record {
                directory.record = Some(record.clone());
            }
            if desired_side {
                if let Some(environment) = &changes.environment {
                    directory.environment = Some(environment.clone());
                }
                apply_map(&mut directory.desired, &changes.desired);
            }
            if observed_side {
                apply_map(&mut directory.observed, &changes.observed);
                apply_map(&mut directory.artifacts, &changes.artifacts);
            }
            state_ref.tip = Some(commit.clone());
            state_ref.commits += 1;
            commit
        };
        if same_ref {
            if desired_changed || observed_changed {
                let commit = apply(&mut inner, &location.desired_ref, true, true);
                committed.desired = Some(commit.clone());
                committed.observed = Some(commit);
            }
        } else {
            if desired_changed {
                committed.desired = Some(apply(&mut inner, &location.desired_ref, true, false));
            }
            if observed_changed {
                committed.observed = Some(apply(&mut inner, &location.observed_ref, false, true));
            }
        }
        Ok(committed)
    }
}

fn apply_map<V: Clone>(target: &mut BTreeMap<UnitName, V>, changes: &BTreeMap<UnitName, Option<V>>) {
    for (unit, change) in changes {
        match change {
            Some(value) => {
                target.insert(unit.clone(), value.clone());
            }
            None => {
                target.remove(unit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::ports::TransitionChanges;
    use nyl_core::orchestration::state::{LeaseKind, Operation, StateRecordKind, StateV1};

    fn location(shared: bool) -> StateLocation {
        StateLocation {
            repository: "file:///project.git".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: if shared { "nyl/dev/desired" } else { "nyl/dev/observed" }.to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        }
    }

    fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    fn lease(run: &str) -> Lease {
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: RunId::new(run).unwrap(),
            runner: "test".to_owned(),
            operation: Operation::StateInit,
            units: Vec::new(),
            deadline: at,
            taken_at: at,
        }
    }

    fn init(location: &StateLocation) -> Transition {
        Transition {
            message: "nyl: state-init dev".to_owned(),
            changes: TransitionChanges {
                record: Some(StateRecord {
                    api_version: StateV1,
                    kind: StateRecordKind,
                    environment: dev(),
                    template: None,
                    params: BTreeMap::new(),
                    expires_at: None,
                    location: location.clone(),
                    events: Vec::new(),
                }),
                ..TransitionChanges::default()
            },
        }
    }

    const RUN_A: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
    const RUN_B: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91";

    #[test]
    fn test_lease_compare_and_swap_admits_one_holder() {
        let store = MemoryStateStore::new();
        let location = location(false);
        let version = store.take_lease(&location, &dev(), &lease(RUN_A), None).unwrap();
        // A second run expecting no lease loses the race.
        assert!(matches!(
            store.take_lease(&location, &dev(), &lease(RUN_B), None),
            Err(StateError::LeaseRace)
        ));
        // A takeover names the version it replaces.
        let taken = store
            .take_lease(&location, &dev(), &lease(RUN_B), Some(&version))
            .unwrap();
        // The first holder's update now fails: it lost the lease.
        assert!(matches!(
            store.update_lease(&location, &dev(), &lease(RUN_A), &version),
            Err(StateError::LeaseLost)
        ));
        store.release_lease(&location, &dev(), &taken).unwrap();
        assert!(store.coordination(&location, &dev()).unwrap().lease.is_none());
    }

    #[test]
    fn test_commit_is_fenced_by_the_lease_and_the_read_tips() {
        let store = MemoryStateStore::new();
        let location = location(false);
        let version = store.take_lease(&location, &dev(), &lease(RUN_A), None).unwrap();
        store.break_lease(&location, &dev()).unwrap();
        // A holder whose lease was deleted pushes nothing.
        assert!(matches!(
            store.commit(&location, &dev(), &StateTips::default(), &init(&location), &version),
            Err(StateError::LeaseLost)
        ));
        let version = store.take_lease(&location, &dev(), &lease(RUN_B), None).unwrap();
        let committed = store
            .commit(&location, &dev(), &StateTips::default(), &init(&location), &version)
            .unwrap();
        assert!(committed.desired.is_some() && committed.observed.is_some());
        // A commit built on tips that have moved since is a conflict.
        assert!(matches!(
            store.commit(&location, &dev(), &StateTips::default(), &init(&location), &version),
            Err(StateError::Conflict { .. })
        ));
        let snapshot = store.snapshot(&location).unwrap();
        assert_eq!(snapshot.record.unwrap().environment, dev());
    }

    #[test]
    fn test_one_commit_per_ref_and_one_commit_when_the_refs_coincide() {
        for (shared, observed_commits) in [(false, 1), (true, 1)] {
            let store = MemoryStateStore::new();
            let location = location(shared);
            let version = store.take_lease(&location, &dev(), &lease(RUN_A), None).unwrap();
            store
                .commit(&location, &dev(), &StateTips::default(), &init(&location), &version)
                .unwrap();
            assert_eq!(store.commit_count(&location.repository, &location.desired_ref), 1);
            assert_eq!(
                store.commit_count(&location.repository, &location.observed_ref),
                observed_commits
            );
        }
    }

    /// A run's view: the lease version it believes it holds.
    #[derive(Debug, Clone, Copy)]
    enum Step {
        Take(usize),
        Update(usize),
        Commit(usize),
        Release(usize),
        Break,
    }

    fn step() -> impl proptest::strategy::Strategy<Value = Step> {
        use proptest::prelude::*;
        prop_oneof![
            (0..2usize).prop_map(Step::Take),
            (0..2usize).prop_map(Step::Update),
            (0..2usize).prop_map(Step::Commit),
            (0..2usize).prop_map(Step::Release),
            Just(Step::Break),
        ]
    }

    proptest::proptest! {
        /// Whatever two runs and a lease-breaking operator do, a run's update,
        /// commit, or release succeeds exactly when it still holds the lease.
        #[test]
        fn test_only_the_current_holder_writes(steps in proptest::collection::vec(step(), 1..40)) {
            let store = MemoryStateStore::new();
            let location = location(false);
            let runs = [RUN_A, RUN_B];
            let mut believed: [Option<String>; 2] = [None, None];
            for step in steps {
                let current = store.coordination(&location, &dev()).unwrap().lease.map(|held| held.version);
                match step {
                    Step::Take(run) => {
                        // A run takes over the lease it last saw, as after expiry.
                        let result = store.take_lease(&location, &dev(), &lease(runs[run]), current.as_deref());
                        believed[run] = Some(result.unwrap());
                    }
                    Step::Update(run) => {
                        let Some(version) = believed[run].clone() else { continue };
                        let result = store.update_lease(&location, &dev(), &lease(runs[run]), &version);
                        proptest::prop_assert_eq!(result.is_ok(), current.as_deref() == Some(version.as_str()));
                        believed[run] = result.ok();
                    }
                    Step::Commit(run) => {
                        let Some(version) = believed[run].clone() else { continue };
                        let base = store.snapshot(&location).unwrap().tips;
                        let result = store.commit(&location, &dev(), &base, &init(&location), &version);
                        proptest::prop_assert_eq!(result.is_ok(), current.as_deref() == Some(version.as_str()));
                    }
                    Step::Release(run) => {
                        let Some(version) = believed[run].take() else { continue };
                        let result = store.release_lease(&location, &dev(), &version);
                        proptest::prop_assert_eq!(result.is_ok(), current.as_deref() == Some(version.as_str()));
                    }
                    Step::Break => store.break_lease(&location, &dev()).unwrap(),
                }
            }
        }
    }
}
