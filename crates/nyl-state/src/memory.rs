//! An in-memory [`StateStore`] with the same compare-and-swap semantics as
//! the Git store, for the contract walkthroughs and unit tests.
//!
//! State refs are kept per repository and ref name, each holding the
//! environment directories that `state.path` places in it, so environments
//! sharing a ref share its commit history as they would in Git. Commit IDs
//! are deterministic digests of a counter, never real Git objects.
//!
//! Like the Git store's clone, the store keeps local branches of remote
//! state refs under [`StateLocation::LOCAL_REPOSITORY`]: reading a remote
//! location fast-forwards (or creates) them unless they diverged, a
//! successful transition moves them along, and `--local` runs commit to
//! them alone ([`StateStore::commit_local`]). Contract:
//! [Local runs](../../../design/orchestration-core.md#local-runs).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::require_local_branches;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Committed, Coordination, Divergence, EnvironmentSnapshot, HeldLease, LocalDivergence, LogEntry,
    RunState, StateError, StateResult, StateSide, StateStore, StateTips, Transition, TransitionChanges,
};
use nyl_core::orchestration::state::{
    ArtifactDocument, Attestation, DesiredUnit, EnvironmentRecord, Lease, Observation, ObservationType, ObservedUnit,
    Run, Signal, StateLocation, StateRecord,
};

/// The state of one environment directory on one ref.
#[derive(Debug, Clone, Default, PartialEq)]
struct Directory {
    record: Option<StateRecord>,
    environment: Option<EnvironmentRecord>,
    desired: BTreeMap<UnitName, DesiredUnit>,
    observed: BTreeMap<UnitName, ObservedUnit>,
    artifacts: BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    observations: BTreeMap<UnitName, BTreeMap<ObservationType, Observation>>,
    attestations: BTreeMap<UnitName, BTreeMap<String, Attestation>>,
}

/// One ref: its tip and the directories it holds, by `state.path`.
#[derive(Debug, Clone, Default)]
struct StateRef {
    tip: Option<CommitId>,
    directories: BTreeMap<String, Directory>,
    /// Every commit, oldest first, with the directory it changed.
    log: Vec<(CommitId, String)>,
    /// The directories as of each commit, for `snapshot_at`.
    history: BTreeMap<CommitId, BTreeMap<String, Directory>>,
    /// Every commit's message, oldest first, for assertions on transition commits.
    messages: Vec<String>,
}

impl StateRef {
    /// Whether the commits after `base` left the directory at `path` alone.
    fn unchanged_since(&self, base: Option<&CommitId>, path: &str) -> bool {
        let after = match base {
            None => 0,
            Some(base) => match self.log.iter().position(|(commit, _)| commit == base) {
                Some(index) => index + 1,
                None => return false,
            },
        };
        self.log[after..].iter().all(|(_, changed)| changed != path)
    }
}

#[derive(Debug, Default)]
struct Inner {
    counter: u64,
    refs: BTreeMap<(String, String), StateRef>,
    leases: BTreeMap<(String, String, EnvironmentName), HeldLease>,
    runs: BTreeMap<(String, String, EnvironmentName), BTreeMap<RunId, RunState>>,
    evidence: BTreeMap<(String, String, EnvironmentName), BTreeMap<RunId, RunState>>,
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

/// The clone's local branch of a remote state ref.
fn local_key(reference: &str) -> (String, String) {
    (StateLocation::LOCAL_REPOSITORY.to_owned(), reference.to_owned())
}

/// Fast-forward (or create) the local branch of a remote location's state
/// ref to the remote tip, unless the local branch has commits the remote
/// lacks, as the Git store does on every read and push.
fn follow(inner: &mut Inner, location: &StateLocation, reference: &str) {
    if location.is_local() || !StateLocation::is_branch_ref(reference) {
        return;
    }
    let Some(remote) = inner.refs.get(&ref_key(location, reference)) else {
        return;
    };
    if remote.tip.is_none() {
        return;
    }
    let forward = match inner
        .refs
        .get(&local_key(reference))
        .and_then(|local| local.tip.as_ref())
    {
        None => true,
        Some(tip) => remote.log.iter().any(|(commit, _)| commit == tip),
    };
    if forward {
        let copy = remote.clone();
        inner.refs.insert(local_key(reference), copy);
    }
}

/// Commits only `local` has, and commits only `remote` has, over their
/// linear histories.
fn divergence(local: &StateRef, remote: &StateRef) -> Divergence {
    let common = local
        .log
        .iter()
        .zip(&remote.log)
        .take_while(|((a, _), (b, _))| a == b)
        .count();
    Divergence {
        ahead: local.log.len() - common,
        behind: remote.log.len() - common,
    }
}

/// Apply one side's changes to the ref at `key`; commit only when the
/// directory changed, so an operation that changes nothing writes nothing.
fn apply_side(
    inner: &mut Inner,
    key: &(String, String),
    path: &str,
    transition: &Transition,
    (desired_side, observed_side): (bool, bool),
) -> Option<CommitId> {
    let changes: &TransitionChanges = &transition.changes;
    let before = inner
        .refs
        .get(key)
        .and_then(|state_ref| state_ref.directories.get(path))
        .cloned()
        .unwrap_or_default();
    let mut directory = before.clone();
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
        apply_map(&mut directory.observations, &changes.observations);
        apply_map(&mut directory.attestations, &changes.attestations);
    }
    if directory == before {
        return None;
    }
    let commit = inner.next_commit();
    let state_ref = inner.refs.entry(key.clone()).or_default();
    state_ref.directories.insert(path.to_owned(), directory);
    state_ref.tip = Some(commit.clone());
    state_ref.log.push((commit.clone(), path.to_owned()));
    let directories = state_ref.directories.clone();
    state_ref.history.insert(commit.clone(), directories);
    state_ref.messages.push(transition.message.clone());
    Some(commit)
}

/// Apply a transition to the refs `key` maps each state ref name to.
fn apply_transition(
    inner: &mut Inner,
    location: &StateLocation,
    transition: &Transition,
    key: impl Fn(&str) -> (String, String),
) -> Committed {
    let mut committed = Committed::default();
    if location.desired_ref == location.observed_ref {
        if let Some(commit) = apply_side(
            inner,
            &key(&location.desired_ref),
            &location.path,
            transition,
            (true, true),
        ) {
            committed.desired = Some(commit.clone());
            committed.observed = Some(commit);
        }
    } else {
        committed.desired = apply_side(
            inner,
            &key(&location.desired_ref),
            &location.path,
            transition,
            (true, false),
        );
        committed.observed = apply_side(
            inner,
            &key(&location.observed_ref),
            &location.path,
            transition,
            (false, true),
        );
    }
    committed
}

/// Move the held lease to a new version after a transition that wrote
/// something, as the Git store's fenced push does.
fn renew_lease(
    inner: &mut Inner,
    key: &(String, String, EnvironmentName),
    committed: &mut Committed,
    lease_version: &str,
) {
    committed.lease_version = if committed.desired.is_some() || committed.observed.is_some() {
        let version = inner.next_version();
        if let Some(held) = inner.leases.get_mut(key) {
            held.version.clone_from(&version);
        }
        version
    } else {
        lease_version.to_owned()
    };
}

/// Hold `lease` at `key` under a new version, which it returns.
fn put_lease(inner: &mut Inner, key: (String, String, EnvironmentName), lease: &Lease) -> String {
    let version = inner.next_version();
    inner.leases.insert(
        key,
        HeldLease {
            lease: lease.clone(),
            version: version.clone(),
        },
    );
    version
}

/// Merge `created` into a run's created units: one per unit name, sorted.
fn merge_created(into: &mut Vec<DesiredUnit>, created: impl IntoIterator<Item = DesiredUnit>) {
    for desired in created {
        into.retain(|existing| existing.unit.name != desired.unit.name);
        into.push(desired);
    }
    into.sort_by(|a, b| a.unit.name.cmp(&b.unit.name));
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("store lock")
    }
}

impl StateStore for MemoryStateStore {
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
        let mut inner = self.lock();
        follow(&mut inner, location, &location.desired_ref);
        follow(&mut inner, location, &location.observed_ref);
        let read = |reference: &str| inner.refs.get(&ref_key(location, reference));
        let desired = read(&location.desired_ref);
        let observed = read(&location.observed_ref);
        let desired_directory = desired.and_then(|state_ref| state_ref.directories.get(&location.path));
        let observed_directory = observed.and_then(|state_ref| state_ref.directories.get(&location.path));
        Ok(snapshot_of(
            StateTips {
                desired: desired.and_then(|state_ref| state_ref.tip.clone()),
                observed: observed.and_then(|state_ref| state_ref.tip.clone()),
            },
            desired_directory,
            observed_directory,
        ))
    }

    fn snapshot_at(&self, location: &StateLocation, tips: &StateTips) -> StateResult<EnvironmentSnapshot> {
        let inner = self.lock();
        let read = |reference: &str, tip: Option<&CommitId>| -> StateResult<Option<Directory>> {
            let Some(tip) = tip else {
                return Ok(None);
            };
            let directories = inner
                .refs
                .get(&ref_key(location, reference))
                .and_then(|state_ref| state_ref.history.get(tip))
                .ok_or_else(|| {
                    StateError::Unavailable(format!("state revision {tip} is not a commit of {reference}"))
                })?;
            Ok(directories.get(&location.path).cloned())
        };
        let desired = read(&location.desired_ref, tips.desired.as_ref())?;
        let observed = read(&location.observed_ref, tips.observed.as_ref())?;
        Ok(snapshot_of(tips.clone(), desired.as_ref(), observed.as_ref()))
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
            evidence: inner
                .evidence
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
        Ok(put_lease(&mut inner, key, lease))
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
        Ok(put_lease(&mut inner, key, lease))
    }

    /// Both under one lock: the lease and the run ref move together.
    fn update_lease_and_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        version: &mut String,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        match inner.leases.get(&key) {
            Some(held) if held.version == *version => {}
            _ => return Err(StateError::LeaseLost),
        }
        *version = put_lease(&mut inner, key, lease);
        let state = run_state(&mut inner, location, environment, run);
        merge_created(&mut state.created, created.iter().cloned());
        Ok(())
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

    fn delete_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()> {
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
        let state = run_state(&mut inner, location, environment, run);
        if let Some(checkpoint) = checkpoint {
            state.checkpoints.push(checkpoint.clone());
        }
        Ok(())
    }

    fn write_run_created(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        let mut inner = self.lock();
        let state = run_state(&mut inner, location, environment, run);
        merge_created(&mut state.created, created.iter().cloned());
        Ok(())
    }

    fn delete_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        if let Some(runs) = inner.runs.get_mut(&key) {
            stale_run(runs.get(run), run, expected)?;
            runs.remove(run);
        }
        inner.signals.remove(&(key.0, key.1, key.2, run.clone()));
        Ok(())
    }

    fn retire_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        let mut inner = self.lock();
        let key = coordination_key(location, environment);
        stale_run(inner.runs.get(&key).and_then(|runs| runs.get(run)), run, expected)?;
        let Some(retired) = inner.runs.get_mut(&key).and_then(|runs| runs.remove(run)) else {
            return Ok(());
        };
        inner
            .signals
            .remove(&(key.0.clone(), key.1.clone(), key.2.clone(), run.clone()));
        let version = inner.next_version();
        let evidence = inner.evidence.entry(key).or_default();
        match evidence.get_mut(run) {
            Some(existing) => {
                existing.run = retired.run;
                existing.checkpoints.extend(retired.checkpoints);
                merge_created(&mut existing.created, retired.created);
                existing.version = version;
            }
            None => {
                evidence.insert(run.clone(), RunState { version, ..retired });
            }
        }
        Ok(())
    }

    fn delete_evidence(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        let mut inner = self.lock();
        if let Some(evidence) = inner.evidence.get_mut(&coordination_key(location, environment)) {
            evidence.remove(run);
        }
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
        let key = coordination_key(location, environment);
        match inner.leases.get(&key) {
            Some(held) if held.version == lease_version => {}
            _ => return Err(StateError::LeaseLost),
        }
        for (reference, expected) in [
            (&location.desired_ref, &base.desired),
            (&location.observed_ref, &base.observed),
        ] {
            let actual = inner
                .refs
                .get(&ref_key(location, reference))
                .and_then(|state_ref| state_ref.tip.clone());
            // Commits of environments sharing the ref through `path` are
            // rebased past; any other commit is a conflict.
            let rebased = inner
                .refs
                .get(&ref_key(location, reference))
                .is_some_and(|state_ref| state_ref.unchanged_since(expected.as_ref(), &location.path));
            if &actual != expected && !rebased {
                return Err(StateError::conflict(reference.clone(), actual));
            }
        }
        let mut committed = apply_transition(&mut inner, location, transition, |reference| {
            ref_key(location, reference)
        });
        renew_lease(&mut inner, &key, &mut committed, lease_version);
        follow(&mut inner, location, &location.desired_ref);
        follow(&mut inner, location, &location.observed_ref);
        Ok(committed)
    }

    fn import_history(&self, from: &StateLocation, to: &StateLocation) -> StateResult<()> {
        let mut inner = self.lock();
        for (source, target) in [
            (&from.desired_ref, &to.desired_ref),
            (&from.observed_ref, &to.observed_ref),
        ] {
            let Some(copied) = inner.refs.get(&ref_key(from, source)).cloned() else {
                continue;
            };
            match inner.refs.get(&ref_key(to, target)) {
                None => {
                    inner.refs.insert(ref_key(to, target), copied);
                }
                Some(existing) if existing.tip == copied.tip => {}
                Some(existing) => return Err(StateError::conflict(target.clone(), existing.tip.as_ref())),
            }
            follow(&mut inner, to, target);
        }
        Ok(())
    }

    fn log(&self, location: &StateLocation, side: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
        let inner = self.lock();
        let reference = location.state_ref(side);
        let Some(state_ref) = inner.refs.get(&ref_key(location, reference)) else {
            return Ok(Vec::new());
        };
        Ok(state_ref
            .log
            .iter()
            .zip(&state_ref.messages)
            .rev()
            .take(limit)
            .map(|((commit, _), message)| LogEntry {
                commit: commit.clone(),
                message: message.clone(),
            })
            .collect())
    }

    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        if location.is_local() {
            return Ok(LocalDivergence::default());
        }
        let inner = self.lock();
        let side = |reference: &str| -> Option<Divergence> {
            if !StateLocation::is_branch_ref(reference) {
                return None;
            }
            let remote = inner
                .refs
                .get(&ref_key(location, reference))
                .filter(|state_ref| state_ref.tip.is_some())?;
            let local = inner
                .refs
                .get(&local_key(reference))
                .filter(|state_ref| state_ref.tip.is_some())?;
            Some(divergence(local, remote))
        };
        Ok(LocalDivergence {
            desired: side(&location.desired_ref),
            observed: side(&location.observed_ref),
        })
    }

    fn reset_local(&self, location: &StateLocation, expected: &StateTips) -> StateResult<StateTips> {
        if location.is_local() {
            return Err(StateError::Unavailable(
                "the state lives on the clone's own branches, which have no remote state to reset to".to_owned(),
            ));
        }
        require_local_branches(location)?;
        let mut inner = self.lock();
        let mut moves = vec![(&location.desired_ref, &expected.desired)];
        if location.observed_ref != location.desired_ref {
            moves.push((&location.observed_ref, &expected.observed));
        } else if expected.observed != expected.desired {
            return Err(StateError::conflict(
                location.observed_ref.clone(),
                expected.desired.as_ref(),
            ));
        }
        for (reference, expected) in &moves {
            let current = inner
                .refs
                .get(&local_key(reference))
                .and_then(|state_ref| state_ref.tip.clone());
            if &current != *expected {
                return Err(StateError::conflict((*reference).clone(), current));
            }
        }
        for (reference, _) in &moves {
            match inner.refs.get(&ref_key(location, reference)).cloned() {
                Some(remote) if remote.tip.is_some() => {
                    inner.refs.insert(local_key(reference), remote);
                }
                _ => {
                    inner.refs.remove(&local_key(reference));
                }
            }
        }
        let tip = |reference: &str| {
            inner
                .refs
                .get(&local_key(reference))
                .and_then(|state_ref| state_ref.tip.clone())
        };
        Ok(StateTips {
            desired: tip(&location.desired_ref),
            observed: tip(&location.observed_ref),
        })
    }

    fn commit_local(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_location: &StateLocation,
        lease_version: &str,
    ) -> StateResult<Committed> {
        if location.is_local() {
            return Err(StateError::Unavailable(
                "local transition commits go to the local branches of remote state; this state lives on the clone's own branches".to_owned(),
            ));
        }
        require_local_branches(location)?;
        let mut inner = self.lock();
        for (reference, expected) in [
            (&location.desired_ref, &base.desired),
            (&location.observed_ref, &base.observed),
        ] {
            let actual = inner
                .refs
                .get(&local_key(reference))
                .and_then(|state_ref| state_ref.tip.clone());
            if &actual != expected {
                return Err(StateError::conflict(reference.clone(), actual));
            }
        }
        let key = coordination_key(lease_location, environment);
        match inner.leases.get(&key) {
            Some(held) if held.version == lease_version => {}
            _ => return Err(StateError::LeaseLost),
        }
        let mut committed = apply_transition(&mut inner, location, transition, local_key);
        renew_lease(&mut inner, &key, &mut committed, lease_version);
        Ok(committed)
    }
}

/// A snapshot from one directory of each side.
fn snapshot_of(tips: StateTips, desired: Option<&Directory>, observed: Option<&Directory>) -> EnvironmentSnapshot {
    EnvironmentSnapshot {
        tips,
        record: desired
            .and_then(|directory| directory.record.clone())
            .or_else(|| observed.and_then(|directory| directory.record.clone())),
        environment: desired.and_then(|directory| directory.environment.clone()),
        desired: desired.map(|directory| directory.desired.clone()).unwrap_or_default(),
        observed: observed.map(|directory| directory.observed.clone()).unwrap_or_default(),
        artifacts: observed
            .map(|directory| directory.artifacts.clone())
            .unwrap_or_default(),
        observations: observed
            .map(|directory| directory.observations.clone())
            .unwrap_or_default(),
        attestations: observed
            .map(|directory| directory.attestations.clone())
            .unwrap_or_default(),
    }
}

/// The run ref of `run`, created when missing, with `run` as its record and
/// a new version.
fn run_state<'i>(
    inner: &'i mut Inner,
    location: &StateLocation,
    environment: &EnvironmentName,
    run: &Run,
) -> &'i mut RunState {
    let version = inner.next_version();
    let runs = inner.runs.entry(coordination_key(location, environment)).or_default();
    let state = runs.entry(run.run.clone()).or_insert_with(|| RunState {
        run: run.clone(),
        checkpoints: Vec::new(),
        created: Vec::new(),
        version: String::new(),
        foreign: false,
    });
    state.run = run.clone();
    state.version = version;
    state
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

/// Refuse to consume a run ref that moved since the caller read it at `expected`.
fn stale_run(current: Option<&RunState>, run: &RunId, expected: Option<&str>) -> StateResult<()> {
    match (current, expected) {
        (Some(current), Some(expected)) if current.version != expected => Err(StateError::Conflict {
            reference: format!("runs/{run}"),
            actual: current.version.clone(),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::ports::TransitionChanges;
    use nyl_core::orchestration::state::{LeaseKind, Operation, StateRecordKind, StateV1};

    fn location() -> StateLocation {
        StateLocation {
            repository: "file:///project.git".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
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
            broken: None,
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
            let location = location();
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
                        if let Ok(committed) = result {
                            believed[run] = Some(committed.lease_version);
                        }
                    }
                    Step::Release(run) => {
                        let Some(version) = believed[run].take() else { continue };
                        let result = store.release_lease(&location, &dev(), &version);
                        proptest::prop_assert_eq!(result.is_ok(), current.as_deref() == Some(version.as_str()));
                    }
                    Step::Break => store.delete_lease(&location, &dev()).unwrap(),
                }
            }
        }
    }
}
