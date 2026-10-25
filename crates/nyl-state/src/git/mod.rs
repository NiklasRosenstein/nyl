//! The Git implementation of [`StateStore`].
//!
//! Contract: [State layout](../../../../design/orchestration-core.md#state-layout),
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
//! [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves),
//! and [Local runs](../../../../design/orchestration-core.md#local-runs).
//!
//! Desired and observed documents live in fixed directories beneath the
//! location's `path`, so the layout is the same whether the two refs
//! coincide. Leases, runs, and signals live on refs under the coordination
//! prefix. Every write is a compare-and-swap; the transition commit and a
//! move of the lease to a new commit land in one atomic push, so a run whose
//! lease was taken over never changes state.

mod refs;
mod tree;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::{Oid, Repository};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Checkpoint, Clock, CommitIdentity, Committed, Coordination, Divergence, EnvironmentSnapshot, HeldLease,
    LocalDivergence, LogEntry, RunState, StateError, StateResult, StateSide, StateStore, StateTips, Transition,
    TransitionChanges,
};
use nyl_core::orchestration::state::{
    ArtifactDocument, DesiredUnit, Lease, Observation, ObservationType, ObservedUnit, Run, Signal, StateLocation,
};
use nyl_git::GitAuth;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub use refs::{full_name, LOCAL_REPOSITORY};
use refs::{unavailable, Refs, Update, UpdateError};
use tree::Files;

const STATE_FILE: &str = "state.yaml";
const ENVIRONMENT_FILE: &str = "desired/environment.yaml";
const LEASE_FILE: &str = "lease.yaml";
const RUN_FILE: &str = "run.yaml";
/// How often a transition is rebuilt on tips that other environments moved.
const REBASE_ATTEMPTS: usize = 5;

/// State in Git repositories, worked on through the clone at `git_dir`.
pub struct GitStateStore {
    git_dir: PathBuf,
    auth: GitAuth,
    /// `None` for a read-only store: every write is refused.
    author: Option<CommitAuthor>,
    endpoints: BTreeMap<String, Endpoint>,
}

/// Who authors and commits every commit this store writes, and when: transition commits,
/// leases, run checkpoints, and signals. The command line resolves the
/// identity once and passes its injected clock, so the store reads neither
/// Git configuration nor the system clock.
#[derive(Clone)]
pub struct CommitAuthor {
    pub identity: CommitIdentity,
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for CommitAuthor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommitAuthor")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// How to reach a repository that state locations name by identity: the
/// credential-free URL `state.yaml` records may differ from the URL that
/// reads it, such as an `origin` with embedded credentials, and from the URL
/// that writes it, a GitRepository's `publishURL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub read: String,
    pub write: Option<String>,
}

impl GitStateStore {
    /// A store working through the repository at `git_dir`: the source
    /// checkout's clone, whose own branches are the state when a location
    /// names [`LOCAL_REPOSITORY`], and which keeps the fetched objects and
    /// the fast-forwarded local state branches otherwise.
    pub fn new(git_dir: impl Into<PathBuf>, auth: GitAuth, author: CommitAuthor) -> Self {
        Self {
            git_dir: git_dir.into(),
            auth,
            author: Some(author),
            endpoints: BTreeMap::new(),
        }
    }

    /// A store for commands that only read state (`plan`, `status`, `get`):
    /// they need no commit identity, and every write fails with
    /// [`StateError::Unavailable`](nyl_core::orchestration::ports::StateError::Unavailable).
    pub fn read_only(git_dir: impl Into<PathBuf>, auth: GitAuth) -> Self {
        Self {
            git_dir: git_dir.into(),
            auth,
            author: None,
            endpoints: BTreeMap::new(),
        }
    }

    /// Reach the repository that locations name `identity` through `endpoint`.
    #[must_use]
    pub fn with_endpoint(mut self, identity: impl Into<String>, endpoint: Endpoint) -> Self {
        self.endpoints.insert(identity.into(), endpoint);
        self
    }

    fn open(&self) -> StateResult<Repository> {
        Repository::open(&self.git_dir).map_err(unavailable)
    }

    fn refs<'a>(&'a self, repository: &'a Repository, location: &'a StateLocation) -> Refs<'a> {
        if location.repository == LOCAL_REPOSITORY {
            return Refs {
                repository,
                url: None,
                push_url: None,
                auth: &self.auth,
            };
        }
        let endpoint = self.endpoints.get(&location.repository);
        Refs {
            repository,
            url: Some(endpoint.map_or(location.repository.as_str(), |endpoint| endpoint.read.as_str())),
            push_url: endpoint.and_then(|endpoint| endpoint.write.as_deref()),
            auth: &self.auth,
        }
    }

    /// Rewrite a run ref with `run` as its record and `change` applied to
    /// its files, by compare-and-swap against the tip it read.
    fn update_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        change: impl FnOnce(&mut Files) -> StateResult<()>,
    ) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = run_ref(location, environment, &run.run);
        let heads = refs.heads(std::slice::from_ref(&name))?;
        refs.fetch(&heads, false)?;
        let tip = heads.get(&name).and_then(|_| refs.local(&name));
        let mut files = tree::read(&repository, tip)?;
        files.insert(RUN_FILE.to_owned(), to_yaml(run)?);
        change(&mut files)?;
        let parents = tip.into_iter().collect::<Vec<_>>();
        let message = format!("nyl: run {} of {environment}", run.run);
        let Some(commit) = tree::commit(&repository, self.author.as_ref(), &files, &message, &parents, true)? else {
            return Ok(());
        };
        match refs.update(&[Update {
            name: name.clone(),
            new: Some(commit),
            expected: heads.get(&name).copied(),
        }]) {
            Ok(()) => Ok(()),
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: name,
                actual: "another commit".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

fn lease_ref(location: &StateLocation, environment: &EnvironmentName) -> String {
    format!("{}{environment}/lease", location.coordination_ref_prefix)
}

fn run_ref(location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> String {
    format!("{}{environment}/runs/{run}", location.coordination_ref_prefix)
}

fn evidence_ref(location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> String {
    format!("{}{environment}/evidence/{run}", location.coordination_ref_prefix)
}

/// The file name of an observation type, as `nyl verify` writes it.
fn observation_name(observation_type: ObservationType) -> String {
    match observation_type {
        ObservationType::Drift => "drift",
        ObservationType::Health => "health",
    }
    .to_owned()
}

fn signal_ref(location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> String {
    format!("{}{environment}/signals/{run}", location.coordination_ref_prefix)
}

fn to_yaml(document: &impl Serialize) -> StateResult<Vec<u8>> {
    serde_saphyr::to_string(document)
        .map(String::into_bytes)
        .map_err(|error| StateError::Corrupt(format!("cannot write a state document: {error}")))
}

fn from_yaml<T: DeserializeOwned>(path: &str, bytes: &[u8]) -> StateResult<T> {
    let text = std::str::from_utf8(bytes).map_err(|error| StateError::Corrupt(format!("{path}: {error}")))?;
    serde_saphyr::from_str(text).map_err(|error| StateError::Corrupt(format!("{path}: {error}")))
}

fn commit_id(oid: Oid) -> CommitId {
    CommitId::new(oid.to_string()).expect("Git commit IDs are 40 lowercase hex digits")
}

fn oid(commit: &CommitId) -> StateResult<Oid> {
    Oid::from_str(commit.as_str()).map_err(unavailable)
}

/// The unit documents in a directory of state files, by unit name.
fn units<T: DeserializeOwned>(files: &Files, directory: &str) -> StateResult<BTreeMap<UnitName, T>> {
    let mut units = BTreeMap::new();
    for (path, bytes) in files {
        let Some(file) = path.strip_prefix(directory) else {
            continue;
        };
        let Some(name) = file.strip_suffix(".yaml").filter(|name| !name.contains('/')) else {
            continue;
        };
        let unit = UnitName::new(name).map_err(|error| StateError::Corrupt(format!("{path}: {error}")))?;
        units.insert(unit, from_yaml(path, bytes)?);
    }
    Ok(units)
}

fn artifacts(files: &Files) -> StateResult<BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>> {
    per_unit(files, "artifacts")
}

/// The documents in each unit's `observed/units/<unit>/<kind>/` directory,
/// by unit and file name.
fn per_unit<T: DeserializeOwned>(files: &Files, kind: &str) -> StateResult<BTreeMap<UnitName, BTreeMap<String, T>>> {
    let marker = format!("/{kind}/");
    let mut documents: BTreeMap<UnitName, BTreeMap<String, T>> = BTreeMap::new();
    for (path, bytes) in files {
        let Some(rest) = path.strip_prefix("observed/units/") else {
            continue;
        };
        let Some((unit, file)) = rest.split_once(&marker) else {
            continue;
        };
        let Some(name) = file.strip_suffix(".yaml").filter(|name| !name.contains('/')) else {
            continue;
        };
        let unit = UnitName::new(unit).map_err(|error| StateError::Corrupt(format!("{path}: {error}")))?;
        documents
            .entry(unit)
            .or_default()
            .insert(name.to_owned(), from_yaml(path, bytes)?);
    }
    Ok(documents)
}

/// The latest observations by unit and type.
fn observations(files: &Files) -> StateResult<BTreeMap<UnitName, BTreeMap<ObservationType, Observation>>> {
    Ok(per_unit::<Observation>(files, "observations")?
        .into_iter()
        .map(|(unit, documents)| {
            let by_type = documents
                .into_values()
                .map(|observation| (observation.observation_type, observation))
                .collect();
            (unit, by_type)
        })
        .collect())
}

/// Replace a unit's directory of documents of one kind with `set`, or
/// remove it for `None`.
fn replace_set<T: Serialize>(
    files: &mut Files,
    unit: &UnitName,
    kind: &str,
    set: Option<impl Iterator<Item = (String, T)>>,
) -> StateResult<()> {
    let directory = format!("observed/units/{unit}/{kind}/");
    files.retain(|path, _| !path.starts_with(&directory));
    for (name, document) in set.into_iter().flatten() {
        files.insert(format!("{directory}{name}.yaml"), to_yaml(&document)?);
    }
    Ok(())
}

/// Apply one side's changes to the files beneath a location's directory.
fn apply_changes(files: &mut Files, changes: &TransitionChanges, desired: bool, observed: bool) -> StateResult<()> {
    if let Some(record) = &changes.record {
        files.insert(STATE_FILE.to_owned(), to_yaml(record)?);
    }
    if desired {
        if let Some(environment) = &changes.environment {
            files.insert(ENVIRONMENT_FILE.to_owned(), to_yaml(environment)?);
        }
        for (unit, document) in &changes.desired {
            let path = format!("desired/units/{unit}.yaml");
            match document {
                Some(document) => files.insert(path, to_yaml(document)?),
                None => files.remove(&path),
            };
        }
    }
    if observed {
        for (unit, document) in &changes.observed {
            let path = format!("observed/units/{unit}.yaml");
            match document {
                Some(document) => files.insert(path, to_yaml(document)?),
                None => files.remove(&path),
            };
        }
        for (unit, set) in &changes.artifacts {
            let set = set
                .as_ref()
                .map(|set| set.iter().map(|(name, document)| (name.clone(), document)));
            replace_set(files, unit, "artifacts", set)?;
        }
        for (unit, set) in &changes.observations {
            let set = set.as_ref().map(|set| {
                set.iter()
                    .map(|(observation_type, document)| (observation_name(*observation_type), document))
            });
            replace_set(files, unit, "observations", set)?;
        }
        for (unit, set) in &changes.attestations {
            let set = set
                .as_ref()
                .map(|set| set.iter().map(|(name, document)| (name.clone(), document)));
            replace_set(files, unit, "attestations", set)?;
        }
    }
    Ok(())
}

/// A whole tree with the files beneath `directory` replaced by `inner`; the
/// rest of a shared ref, such as other environments' directories, stays.
fn replace_beneath(all: &Files, directory: &str, inner: &Files) -> Files {
    let prefix = tree::prefix(directory);
    let mut files = all
        .iter()
        .filter(|(path, _)| !path.starts_with(&prefix))
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect::<Files>();
    for (path, bytes) in inner {
        files.insert(format!("{prefix}{path}"), bytes.clone());
    }
    files
}

/// A snapshot from the files beneath a location's directory on each side.
fn snapshot_of(tips: StateTips, desired: &Files, observed: &Files) -> StateResult<EnvironmentSnapshot> {
    let record = match desired.get(STATE_FILE).or_else(|| observed.get(STATE_FILE)) {
        Some(bytes) => Some(from_yaml(STATE_FILE, bytes)?),
        None => None,
    };
    Ok(EnvironmentSnapshot {
        tips,
        record,
        environment: desired
            .get(ENVIRONMENT_FILE)
            .map(|bytes| from_yaml(ENVIRONMENT_FILE, bytes))
            .transpose()?,
        desired: units(desired, "desired/units/")?,
        observed: units(observed, "observed/units/")?,
        artifacts: artifacts(observed)?,
        observations: observations(observed)?,
        attestations: per_unit(observed, "attestations")?,
    })
}

impl StateStore for GitStateStore {
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let desired_name = full_name(&location.desired_ref);
        let observed_name = full_name(&location.observed_ref);
        let heads = refs.heads(&[desired_name.clone(), observed_name.clone()])?;
        refs.fetch(&heads, true)?;
        for (name, oid) in &heads {
            refs.fast_forward_local_branch(name, *oid)?;
        }
        let desired_tip = heads.get(&desired_name).copied();
        let observed_tip = heads.get(&observed_name).copied();
        let desired = tree::beneath(
            &tree::read(&repository, desired_tip.and_then(|_| refs.local(&desired_name)))?,
            &location.path,
        );
        let observed = tree::beneath(
            &tree::read(&repository, observed_tip.and_then(|_| refs.local(&observed_name)))?,
            &location.path,
        );
        snapshot_of(
            StateTips {
                desired: desired_tip.map(commit_id),
                observed: observed_tip.map(commit_id),
            },
            &desired,
            &observed,
        )
    }

    /// Read the state as of `tips`, for `nyl get --state-revision`: the
    /// state refs' history is fetched, and each tip must be a commit of it.
    fn snapshot_at(&self, location: &StateLocation, tips: &StateTips) -> StateResult<EnvironmentSnapshot> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let names = [full_name(&location.desired_ref), full_name(&location.observed_ref)];
        let heads = refs.heads(&names)?;
        refs.fetch(&heads, true)?;
        let read = |tip: Option<&CommitId>| -> StateResult<Files> {
            let Some(tip) = tip else {
                return Ok(Files::new());
            };
            let commit = oid(tip)?;
            if repository.find_commit(commit).is_err() {
                return Err(StateError::Unavailable(format!(
                    "state revision {tip} is not a commit of {} or {}",
                    location.desired_ref, location.observed_ref
                )));
            }
            Ok(tree::beneath(&tree::read(&repository, Some(commit))?, &location.path))
        };
        let desired = read(tips.desired.as_ref())?;
        let observed = read(tips.observed.as_ref())?;
        snapshot_of(tips.clone(), &desired, &observed)
    }

    fn coordination(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<Coordination> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let lease_name = lease_ref(location, environment);
        let prefix = format!("{}{environment}/", location.coordination_ref_prefix);
        let runs_prefix = format!("{prefix}runs/");
        let evidence_prefix = format!("{prefix}evidence/");
        let heads = refs.heads(&[
            lease_name.clone(),
            format!("{runs_prefix}*"),
            format!("{evidence_prefix}*"),
        ])?;
        refs.fetch(&heads, false)?;
        let lease = match heads.get(&lease_name) {
            Some(oid) => {
                let files = tree::read(&repository, refs.local(&lease_name).or(Some(*oid)))?;
                let bytes = files
                    .get(LEASE_FILE)
                    .ok_or_else(|| StateError::Corrupt(format!("{lease_name} has no {LEASE_FILE}")))?;
                Some(HeldLease {
                    lease: from_yaml(&lease_name, bytes)?,
                    version: oid.to_string(),
                })
            }
            None => None,
        };
        let mut runs = Vec::new();
        let mut evidence = Vec::new();
        for (name, oid) in &heads {
            let target = if name.starts_with(&runs_prefix) {
                &mut runs
            } else if name.starts_with(&evidence_prefix) {
                &mut evidence
            } else {
                continue;
            };
            let files = tree::read(&repository, refs.local(name).or(Some(*oid)))?;
            target.push(read_run(name, &files, oid.to_string())?);
        }
        Ok(Coordination { lease, runs, evidence })
    }

    fn take_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: Option<&str>,
    ) -> StateResult<String> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = lease_ref(location, environment);
        let expected = expected.map(Oid::from_str).transpose().map_err(unavailable)?;
        write_lease(
            &repository,
            self.author.as_ref(),
            &refs,
            &name,
            lease,
            expected,
            StateError::LeaseRace,
        )
    }

    fn update_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: &str,
    ) -> StateResult<String> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = lease_ref(location, environment);
        let expected = Oid::from_str(expected).map_err(unavailable)?;
        write_lease(
            &repository,
            self.author.as_ref(),
            &refs,
            &name,
            lease,
            Some(expected),
            StateError::LeaseLost,
        )
    }

    fn release_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        expected: &str,
    ) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let expected = Oid::from_str(expected).map_err(unavailable)?;
        match refs.update(&[Update {
            name: lease_ref(location, environment),
            new: None,
            expected: Some(expected),
        }]) {
            Ok(()) => Ok(()),
            Err(UpdateError::Stale) => Err(StateError::LeaseLost),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    fn delete_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = lease_ref(location, environment);
        let Some(current) = refs.heads(std::slice::from_ref(&name))?.get(&name).copied() else {
            return Ok(());
        };
        match refs.update(&[Update {
            name,
            new: None,
            expected: Some(current),
        }]) {
            Ok(()) | Err(UpdateError::Stale) => Ok(()),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    fn write_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        checkpoint: Option<&Checkpoint>,
    ) -> StateResult<()> {
        self.update_run(location, environment, run, |files| {
            if let Some(checkpoint) = checkpoint {
                let index = checkpoint_count(files);
                insert_checkpoint(files, index, checkpoint)?;
            }
            Ok(())
        })
    }

    fn write_run_created(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        self.update_run(location, environment, run, |files| {
            for desired in created {
                files.insert(format!("created/{}.yaml", desired.unit.name), to_yaml(desired)?);
            }
            Ok(())
        })
    }

    fn retire_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let run_name = run_ref(location, environment, run);
        let evidence_name = evidence_ref(location, environment, run);
        let signal_name = signal_ref(location, environment, run);
        let heads = refs.heads(&[run_name.clone(), evidence_name.clone(), signal_name.clone()])?;
        refs.fetch(&heads, false)?;
        let Some(run_oid) = heads.get(&run_name).copied() else {
            return Ok(());
        };
        stale_run(&run_name, run_oid, expected)?;
        let run_files = tree::read(&repository, refs.local(&run_name).or(Some(run_oid)))?;
        let evidence_tip = heads.get(&evidence_name).and_then(|_| refs.local(&evidence_name));
        let mut files = tree::read(&repository, evidence_tip)?;
        // The run ref's record and created units replace the evidence ref's;
        // its checkpoints are appended after the ones retired before.
        let retired = read_run(&run_name, &run_files, run_oid.to_string())?;
        files.insert(RUN_FILE.to_owned(), to_yaml(&retired.run)?);
        for desired in &retired.created {
            files.insert(format!("created/{}.yaml", desired.unit.name), to_yaml(desired)?);
        }
        let first = checkpoint_count(&files);
        for (index, checkpoint) in (first..).zip(&retired.checkpoints) {
            insert_checkpoint(&mut files, index, checkpoint)?;
        }
        let parents = evidence_tip.into_iter().collect::<Vec<_>>();
        let message = format!("nyl: evidence of run {run} of {environment}");
        let commit = tree::commit(&repository, self.author.as_ref(), &files, &message, &parents, true)?
            .expect("allow_empty commits");
        let mut updates = vec![
            Update {
                name: evidence_name.clone(),
                new: Some(commit),
                expected: heads.get(&evidence_name).copied(),
            },
            Update {
                name: run_name.clone(),
                new: None,
                expected: Some(run_oid),
            },
        ];
        if let Some(signal) = heads.get(&signal_name) {
            updates.push(Update {
                name: signal_name,
                new: None,
                expected: Some(*signal),
            });
        }
        match refs.update(&updates) {
            Ok(()) => Ok(()),
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: run_name,
                actual: "another commit".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    fn delete_evidence(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = evidence_ref(location, environment, run);
        let heads = refs.heads(std::slice::from_ref(&name))?;
        let Some(current) = heads.get(&name).copied() else {
            return Ok(());
        };
        match refs.update(&[Update {
            name: name.clone(),
            new: None,
            expected: Some(current),
        }]) {
            Ok(()) => Ok(()),
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: name,
                actual: "another commit".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    fn delete_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let names = [
            run_ref(location, environment, run),
            signal_ref(location, environment, run),
        ];
        let heads = refs.heads(&names)?;
        if let Some(oid) = heads.get(&names[0]) {
            stale_run(&names[0], *oid, expected)?;
        }
        let updates = heads
            .iter()
            .map(|(name, oid)| Update {
                name: name.clone(),
                new: None,
                expected: Some(*oid),
            })
            .collect::<Vec<_>>();
        if updates.is_empty() {
            return Ok(());
        }
        match refs.update(&updates) {
            Ok(()) => Ok(()),
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: names[0].clone(),
                actual: "another commit".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    fn signals(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
    ) -> StateResult<Vec<Signal>> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = signal_ref(location, environment, run);
        let heads = refs.heads(std::slice::from_ref(&name))?;
        refs.fetch(&heads, false)?;
        let files = tree::read(&repository, heads.get(&name).and_then(|_| refs.local(&name)))?;
        files
            .iter()
            .filter(|(path, _)| path.starts_with("signals/"))
            .map(|(path, bytes)| from_yaml(path, bytes))
            .collect()
    }

    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = signal_ref(location, environment, &signal.run);
        let heads = refs.heads(std::slice::from_ref(&name))?;
        refs.fetch(&heads, false)?;
        let tip = heads.get(&name).and_then(|_| refs.local(&name));
        let mut files = tree::read(&repository, tip)?;
        let index = files.len();
        files.insert(format!("signals/{index:04}.yaml"), to_yaml(signal)?);
        let parents = tip.into_iter().collect::<Vec<_>>();
        let commit = tree::commit(&repository, self.author.as_ref(), &files, "nyl: signal", &parents, true)?
            .expect("allow_empty commits");
        match refs.update(&[Update {
            name: name.clone(),
            new: Some(commit),
            expected: heads.get(&name).copied(),
        }]) {
            Ok(()) => Ok(()),
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: name,
                actual: "another signal".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    fn commit(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_version: &str,
    ) -> StateResult<Committed> {
        let repository = self.open()?;
        let author = self.author.as_ref();
        let refs = self.refs(&repository, location);
        let desired_name = full_name(&location.desired_ref);
        let observed_name = full_name(&location.observed_ref);
        let lease_name = lease_ref(location, environment);
        let lease_oid = Oid::from_str(lease_version).map_err(unavailable)?;
        let base_desired = base.desired.as_ref().map(oid).transpose()?;
        let base_observed = base.observed.as_ref().map(oid).transpose()?;
        let same_ref = desired_name == observed_name;
        let side = |tip: Option<Oid>, desired: bool, observed: bool| {
            transition_commit(&repository, author, location, transition, tip, (desired, observed))
        };
        // The lease moves to a new commit with the same document, so that
        // the push checks it: git never compares an unchanged ref.
        let lease_files = tree::read(&repository, Some(lease_oid)).map_err(|_| StateError::LeaseLost)?;
        let renewed = tree::commit(&repository, author, &lease_files, "nyl: lease", &[lease_oid], true)?
            .expect("allow_empty commits");

        // Each attempt builds on the tips the last one lost to.
        let (mut tip_desired, mut tip_observed) = (base_desired, base_observed);
        for _ in 0..REBASE_ATTEMPTS {
            let mut updates = Vec::new();
            let mut committed = Committed::default();
            if same_ref {
                if let Some(commit) = side(tip_desired, true, true)? {
                    updates.push(Update {
                        name: desired_name.clone(),
                        new: Some(commit),
                        expected: tip_desired,
                    });
                    committed.desired = Some(commit_id(commit));
                    committed.observed = Some(commit_id(commit));
                }
            } else {
                if let Some(commit) = side(tip_desired, true, false)? {
                    updates.push(Update {
                        name: desired_name.clone(),
                        new: Some(commit),
                        expected: tip_desired,
                    });
                    committed.desired = Some(commit_id(commit));
                }
                if let Some(commit) = side(tip_observed, false, true)? {
                    updates.push(Update {
                        name: observed_name.clone(),
                        new: Some(commit),
                        expected: tip_observed,
                    });
                    committed.observed = Some(commit_id(commit));
                }
            }
            if updates.is_empty() {
                // Nothing to push, but a run that lost its lease still learns so.
                if refs.heads(std::slice::from_ref(&lease_name))?.get(&lease_name) != Some(&lease_oid) {
                    return Err(StateError::LeaseLost);
                }
                lease_version.clone_into(&mut committed.lease_version);
                return Ok(committed);
            }
            updates.push(Update {
                name: lease_name.clone(),
                new: Some(renewed),
                expected: Some(lease_oid),
            });
            match refs.update(&updates) {
                Ok(()) => {
                    for update in &updates {
                        if let Some(new) = update.new {
                            if update.name != lease_name {
                                refs.fast_forward_local_branch(&update.name, new)?;
                            }
                        }
                    }
                    committed.lease_version = renewed.to_string();
                    return Ok(committed);
                }
                Err(UpdateError::Stale) => {
                    (tip_desired, tip_observed) = rebase_tips(
                        &repository,
                        &refs,
                        location,
                        (&lease_name, lease_oid),
                        [(&desired_name, base_desired), (&observed_name, base_observed)],
                    )?;
                }
                Err(UpdateError::Other(error)) => return Err(error),
            }
        }
        Err(StateError::Conflict {
            reference: desired_name,
            actual: "commits that kept moving".to_owned(),
        })
    }

    fn import_history(&self, from: &StateLocation, to: &StateLocation) -> StateResult<()> {
        let repository = self.open()?;
        let source = self.refs(&repository, from);
        let target = self.refs(&repository, to);
        let pairs = [
            (full_name(&from.desired_ref), full_name(&to.desired_ref)),
            (full_name(&from.observed_ref), full_name(&to.observed_ref)),
        ];
        let source_heads = source.heads(&pairs.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>())?;
        source.fetch(&source_heads, true)?;
        let target_heads = target.heads(&pairs.iter().map(|(_, name)| name.clone()).collect::<Vec<_>>())?;
        let mut updates = Vec::new();
        for (from_name, to_name) in &pairs {
            let Some(tip) = source_heads.get(from_name) else {
                continue;
            };
            match target_heads.get(to_name) {
                Some(existing) if existing == tip => {}
                Some(existing) => {
                    return Err(StateError::Conflict {
                        reference: to_name.clone(),
                        actual: existing.to_string(),
                    })
                }
                None if updates.iter().any(|update: &Update| &update.name == to_name) => {}
                None => updates.push(Update {
                    name: to_name.clone(),
                    new: Some(*tip),
                    expected: None,
                }),
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        match target.update(&updates) {
            Ok(()) => {
                for update in &updates {
                    if let Some(new) = update.new {
                        target.fast_forward_local_branch(&update.name, new)?;
                    }
                }
                Ok(())
            }
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: updates[0].name.clone(),
                actual: "another commit".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }

    /// A remote location reads the tracking ref as last fetched, fetching
    /// it first when the clone has never fetched it; a local location reads
    /// the clone's branch.
    fn log(&self, location: &StateLocation, side: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let name = full_name(match side {
            StateSide::Desired => &location.desired_ref,
            StateSide::Observed => &location.observed_ref,
        });
        let mut tip = refs.local(&name);
        if tip.is_none() && refs.url.is_some() {
            let heads = refs.heads(std::slice::from_ref(&name))?;
            refs.fetch(&heads, true)?;
            for (name, oid) in &heads {
                refs.fast_forward_local_branch(name, *oid)?;
            }
            tip = heads.get(&name).and_then(|_| refs.local(&name));
        }
        match tip {
            Some(tip) => first_parent_log(&repository, tip, limit),
            None => Ok(Vec::new()),
        }
    }

    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        if location.repository == LOCAL_REPOSITORY {
            return Ok(LocalDivergence::default());
        }
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let names = [full_name(&location.desired_ref), full_name(&location.observed_ref)];
        let heads = refs.heads(&names)?;
        refs.fetch(&heads, true)?;
        let side = |configured: &str| -> StateResult<Option<Divergence>> {
            if !has_local_branch(configured) {
                return Ok(None);
            }
            let name = full_name(configured);
            let remote = heads.get(&name).and_then(|_| refs.local(&name));
            let (Some(remote), Ok(local)) = (remote, repository.refname_to_id(&name)) else {
                return Ok(None);
            };
            let (ahead, behind) = repository.graph_ahead_behind(local, remote).map_err(unavailable)?;
            Ok(Some(Divergence { ahead, behind }))
        };
        Ok(LocalDivergence {
            desired: side(&location.desired_ref)?,
            observed: side(&location.observed_ref)?,
        })
    }

    fn reset_local(&self, location: &StateLocation, expected: &StateTips) -> StateResult<StateTips> {
        if location.repository == LOCAL_REPOSITORY {
            return Err(StateError::Unavailable(
                "the state lives on the clone's own branches, which have no remote state to reset to".to_owned(),
            ));
        }
        require_local_branches(location)?;
        if self.author.is_none() {
            return Err(StateError::Unavailable(
                "the state store was opened read-only, without a commit identity".to_owned(),
            ));
        }
        let repository = self.open()?;
        let refs = self.refs(&repository, location);
        let desired_name = full_name(&location.desired_ref);
        let observed_name = full_name(&location.observed_ref);
        let heads = refs.heads(&[desired_name.clone(), observed_name.clone()])?;
        refs.fetch(&heads, true)?;
        let remote = |name: &String| heads.get(name).and_then(|_| refs.local(name));
        let expected_desired = expected.desired.as_ref().map(oid).transpose()?;
        let expected_observed = expected.observed.as_ref().map(oid).transpose()?;
        let mut moves = vec![(desired_name.clone(), expected_desired, remote(&desired_name))];
        if observed_name != desired_name {
            moves.push((observed_name.clone(), expected_observed, remote(&observed_name)));
        } else if expected_observed != expected_desired {
            return Err(StateError::Conflict {
                reference: observed_name,
                actual: expected_desired.map_or_else(|| "nothing".to_owned(), |oid| oid.to_string()),
            });
        }
        // A compare-and-swap of both local branches in one transaction.
        let mut transaction = repository.transaction().map_err(unavailable)?;
        for (name, _, _) in &moves {
            transaction.lock_ref(name).map_err(unavailable)?;
        }
        for (name, expected, _) in &moves {
            let current = repository.refname_to_id(name).ok();
            if current != *expected {
                return Err(StateError::Conflict {
                    reference: name.clone(),
                    actual: current.map_or_else(|| "nothing".to_owned(), |oid| oid.to_string()),
                });
            }
        }
        for (name, current, new) in &moves {
            match new {
                Some(new) => transaction
                    .set_target(name, *new, None, "nyl: reset local state")
                    .map_err(unavailable)?,
                None if current.is_some() => transaction.remove(name).map_err(unavailable)?,
                None => {}
            }
        }
        transaction.commit().map_err(unavailable)?;
        Ok(StateTips {
            desired: remote(&desired_name).map(commit_id),
            observed: remote(&observed_name).map(commit_id),
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
        if location.repository == LOCAL_REPOSITORY {
            return Err(StateError::Unavailable(
                "local transition commits go to the local branches of remote state; this state lives on the clone's own branches".to_owned(),
            ));
        }
        require_local_branches(location)?;
        let repository = self.open()?;
        let author = self.author.as_ref();
        let local = Refs {
            repository: &repository,
            url: None,
            push_url: None,
            auth: &self.auth,
        };
        let desired_name = full_name(&location.desired_ref);
        let observed_name = full_name(&location.observed_ref);
        let base_desired = base.desired.as_ref().map(oid).transpose()?;
        let base_observed = base.observed.as_ref().map(oid).transpose()?;
        // Checked before the lease moves, so a stale base costs nothing.
        for (name, base) in [(&desired_name, base_desired), (&observed_name, base_observed)] {
            let current = repository.refname_to_id(name).ok();
            if current != base {
                return Err(StateError::Conflict {
                    reference: name.clone(),
                    actual: current.map_or_else(|| "nothing".to_owned(), |oid| oid.to_string()),
                });
            }
        }
        let side = |tip: Option<Oid>, desired: bool, observed: bool| {
            transition_commit(&repository, author, location, transition, tip, (desired, observed))
        };
        let mut updates = Vec::new();
        let mut committed = Committed::default();
        if desired_name == observed_name {
            if let Some(commit) = side(base_desired, true, true)? {
                updates.push(Update {
                    name: desired_name.clone(),
                    new: Some(commit),
                    expected: base_desired,
                });
                committed.desired = Some(commit_id(commit));
                committed.observed = Some(commit_id(commit));
            }
        } else {
            if let Some(commit) = side(base_desired, true, false)? {
                updates.push(Update {
                    name: desired_name.clone(),
                    new: Some(commit),
                    expected: base_desired,
                });
                committed.desired = Some(commit_id(commit));
            }
            if let Some(commit) = side(base_observed, false, true)? {
                updates.push(Update {
                    name: observed_name.clone(),
                    new: Some(commit),
                    expected: base_observed,
                });
                committed.observed = Some(commit_id(commit));
            }
        }

        // The fence: renew the lease by compare-and-swap before any local
        // branch moves, as `commit` moves it in the same push.
        let lease_refs = self.refs(&repository, lease_location);
        let lease_name = lease_ref(lease_location, environment);
        let lease_oid = Oid::from_str(lease_version).map_err(unavailable)?;
        if updates.is_empty() {
            if lease_refs.heads(std::slice::from_ref(&lease_name))?.get(&lease_name) != Some(&lease_oid) {
                return Err(StateError::LeaseLost);
            }
            lease_version.clone_into(&mut committed.lease_version);
            return Ok(committed);
        }
        let lease_files = tree::read(&repository, Some(lease_oid)).map_err(|_| StateError::LeaseLost)?;
        let renewed = tree::commit(&repository, author, &lease_files, "nyl: lease", &[lease_oid], true)?
            .expect("allow_empty commits");
        match lease_refs.update(&[Update {
            name: lease_name,
            new: Some(renewed),
            expected: Some(lease_oid),
        }]) {
            Ok(()) => {}
            Err(UpdateError::Stale) => return Err(StateError::LeaseLost),
            Err(UpdateError::Other(error)) => return Err(error),
        }
        committed.lease_version = renewed.to_string();
        match local.update(&updates) {
            Ok(()) => Ok(committed),
            Err(UpdateError::Stale) => Err(StateError::Conflict {
                reference: desired_name,
                actual: "another commit".to_owned(),
            }),
            Err(UpdateError::Other(error)) => Err(error),
        }
    }
}

/// One side's transition commit on top of `tip`, or `None` when the
/// location's directory would not change.
fn transition_commit(
    repository: &Repository,
    author: Option<&CommitAuthor>,
    location: &StateLocation,
    transition: &Transition,
    tip: Option<Oid>,
    (desired, observed): (bool, bool),
) -> StateResult<Option<Oid>> {
    let all = tree::read(repository, tip)?;
    let mut inner = tree::beneath(&all, &location.path);
    apply_changes(&mut inner, &transition.changes, desired, observed)?;
    let files = replace_beneath(&all, &location.path, &inner);
    let parents = tip.into_iter().collect::<Vec<_>>();
    tree::commit(repository, author, &files, &transition.message, &parents, false)
}

/// Whether a configured state ref name has a local branch in the clone: a
/// short name or a `refs/heads/` name. Other full `refs/…` names have none.
fn has_local_branch(configured: &str) -> bool {
    !configured.starts_with("refs/") || configured.starts_with("refs/heads/")
}

/// Refuse a location whose state refs have no local branches.
fn require_local_branches(location: &StateLocation) -> StateResult<()> {
    for configured in [&location.desired_ref, &location.observed_ref] {
        if !has_local_branch(configured) {
            return Err(StateError::Unavailable(format!(
                "state ref {configured} is a full ref name outside refs/heads/, which has no local branch in the clone"
            )));
        }
    }
    Ok(())
}

/// The newest `limit` commits of `tip`'s first-parent history, newest first.
fn first_parent_log(repository: &Repository, tip: Oid, limit: usize) -> StateResult<Vec<LogEntry>> {
    let mut entries = Vec::new();
    let mut next = Some(tip);
    while let Some(oid) = next {
        if entries.len() >= limit {
            break;
        }
        let commit = repository.find_commit(oid).map_err(unavailable)?;
        entries.push(LogEntry {
            commit: commit_id(oid),
            message: String::from_utf8_lossy(commit.message_raw_bytes()).into_owned(),
        });
        next = commit.parent_id(0).ok();
    }
    Ok(entries)
}

/// After a lost push: the new tips to rebuild a transition on, when the
/// lease still holds and the commits since `bases` left the location's
/// directory as it was read. Those are commits of another environment
/// sharing the ref through `path`; any other commit is a conflict.
fn rebase_tips(
    repository: &Repository,
    refs: &Refs<'_>,
    location: &StateLocation,
    (lease_name, lease_oid): (&String, Oid),
    bases: [(&String, Option<Oid>); 2],
) -> StateResult<(Option<Oid>, Option<Oid>)> {
    let heads = refs.heads(&[lease_name.clone(), bases[0].0.clone(), bases[1].0.clone()])?;
    if heads.get(lease_name) != Some(&lease_oid) {
        return Err(StateError::LeaseLost);
    }
    let wanted = bases
        .iter()
        .filter_map(|(name, _)| heads.get(*name).map(|head| ((*name).clone(), *head)))
        .collect::<BTreeMap<_, _>>();
    refs.fetch(&wanted, true)?;
    for (name, base) in bases {
        let head = heads.get(name).copied();
        if !directory_unchanged(repository, location, base, head)? {
            return Err(StateError::Conflict {
                reference: name.clone(),
                actual: head.map_or_else(|| "nothing".to_owned(), |head| head.to_string()),
            });
        }
    }
    Ok((heads.get(bases[0].0).copied(), heads.get(bases[1].0).copied()))
}

/// Whether `head` descends from `base` without changing the location's directory.
fn directory_unchanged(
    repository: &Repository,
    location: &StateLocation,
    base: Option<Oid>,
    head: Option<Oid>,
) -> StateResult<bool> {
    match (base, head) {
        _ if base == head => Ok(true),
        // A deleted ref took the directory with it.
        (_, None) => Ok(false),
        // Another environment created the shared ref; ours had nothing yet.
        (None, Some(head)) => Ok(tree::beneath(&tree::read(repository, Some(head))?, &location.path).is_empty()),
        (Some(base), Some(head)) => Ok(repository.graph_descendant_of(head, base).map_err(unavailable)?
            && tree::beneath(&tree::read(repository, Some(base))?, &location.path)
                == tree::beneath(&tree::read(repository, Some(head))?, &location.path)),
    }
}

fn write_lease(
    repository: &Repository,
    author: Option<&CommitAuthor>,
    refs: &Refs<'_>,
    name: &str,
    lease: &Lease,
    expected: Option<Oid>,
    stale: StateError,
) -> StateResult<String> {
    let files = Files::from([(LEASE_FILE.to_owned(), to_yaml(lease)?)]);
    let message = format!("nyl: lease for run {}", lease.run);
    let parents = expected
        .filter(|oid| repository.find_commit(*oid).is_ok())
        .into_iter()
        .collect::<Vec<_>>();
    let commit = tree::commit(repository, author, &files, &message, &parents, true)?.expect("allow_empty commits");
    match refs.update(&[Update {
        name: name.to_owned(),
        new: Some(commit),
        expected,
    }]) {
        Ok(()) => Ok(commit.to_string()),
        Err(UpdateError::Stale) => Err(stale),
        Err(UpdateError::Other(error)) => Err(error),
    }
}

/// How many checkpoints a run ref's files hold.
/// Refuse to consume a run ref that moved since the caller read it at `expected`.
fn stale_run(name: &str, current: git2::Oid, expected: Option<&str>) -> StateResult<()> {
    match expected {
        Some(expected) if current.to_string() != expected => Err(StateError::Conflict {
            reference: name.to_owned(),
            actual: current.to_string(),
        }),
        _ => Ok(()),
    }
}

fn checkpoint_count(files: &Files) -> usize {
    files
        .keys()
        .filter(|path| path.starts_with("checkpoints/") && path.ends_with("/desired.yaml"))
        .count()
}

/// Add a checkpoint as `checkpoints/<index>-<unit>/`.
fn insert_checkpoint(files: &mut Files, index: usize, checkpoint: &Checkpoint) -> StateResult<()> {
    let directory = format!("checkpoints/{index:04}-{}", checkpoint.desired.unit.name);
    files.insert(format!("{directory}/desired.yaml"), to_yaml(&checkpoint.desired)?);
    files.insert(format!("{directory}/observed.yaml"), to_yaml(&checkpoint.observed)?);
    for (artifact, document) in &checkpoint.artifacts {
        files.insert(format!("{directory}/artifacts/{artifact}.yaml"), to_yaml(document)?);
    }
    Ok(())
}

fn read_run(name: &str, files: &Files, version: String) -> StateResult<RunState> {
    let run: Run = from_yaml(
        name,
        files
            .get(RUN_FILE)
            .ok_or_else(|| StateError::Corrupt(format!("{name} has no {RUN_FILE}")))?,
    )?;
    let mut directories = files
        .keys()
        .filter_map(|path| path.strip_prefix("checkpoints/"))
        .filter_map(|rest| rest.split_once('/').map(|(directory, _)| directory.to_owned()))
        .collect::<Vec<_>>();
    directories.sort();
    directories.dedup();
    let mut checkpoints = Vec::new();
    for directory in directories {
        let read = |file: &str| -> StateResult<&Vec<u8>> {
            let path = format!("checkpoints/{directory}/{file}");
            files
                .get(&path)
                .ok_or_else(|| StateError::Corrupt(format!("{name}: {path} is missing")))
        };
        let desired: DesiredUnit = from_yaml(name, read("desired.yaml")?)?;
        let observed: ObservedUnit = from_yaml(name, read("observed.yaml")?)?;
        let prefix = format!("checkpoints/{directory}/artifacts/");
        let artifacts = files
            .iter()
            .filter_map(|(path, bytes)| {
                path.strip_prefix(&prefix)
                    .and_then(|file| file.strip_suffix(".yaml"))
                    .map(|artifact| (artifact.to_owned(), bytes))
            })
            .map(|(artifact, bytes)| Ok((artifact, from_yaml(name, bytes)?)))
            .collect::<StateResult<BTreeMap<_, _>>>()?;
        checkpoints.push(Checkpoint {
            desired,
            observed,
            artifacts,
        });
    }
    let created = files
        .iter()
        .filter(|(path, _)| {
            path.strip_prefix("created/")
                .and_then(|file| file.strip_suffix(".yaml"))
                .is_some_and(|unit| !unit.contains('/'))
        })
        .map(|(path, bytes)| from_yaml(&format!("{name}: {path}"), bytes))
        .collect::<StateResult<Vec<DesiredUnit>>>()?;
    Ok(RunState {
        run,
        checkpoints,
        created,
        version,
        foreign: false,
    })
}
