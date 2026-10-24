//! The traits through which the orchestration shell performs every effect.
//!
//! Decisions are pure functions over snapshots; the shell in
//! `nyl-orchestration` reads and writes only through these traits, and
//! `nyl-cli` passes in the implementations from `nyl-state` and
//! `nyl-drivers`. Each trait has an in-memory or fake implementation used by
//! the scenario tests. See the implementation architecture's "Formats and
//! traits" (`design/implementation-architecture.md`).

use std::collections::BTreeMap;

use serde_json::Value;

use super::ids::{CommitId, EnvironmentName, RunId, Uid, UnitName};
use super::state::{
    ArtifactDocument, DesiredUnit, EnvironmentRecord, Lease, ObservedUnit, Run, Signal, StateLocation, StateRecord,
};
use super::time::{Duration, Timestamp};
use crate::resources::environment::Environment;
use crate::resources::units::UnitEnvelope;

/// The current time. The system clock in production; built with the
/// `test-clock` feature, `nyl-cli` reads `NYL_TEST_NOW` instead.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
    /// Wait, such as for a `delay` teardown wait. A test clock advances
    /// instead of sleeping.
    fn wait(&self, duration: Duration);
}

/// New identifiers: UUIDv7 uids for incarnations and run IDs.
pub trait IdGenerator: Send + Sync {
    fn uid(&self) -> Uid;
    fn run_id(&self) -> RunId;
}

/// A failure of the state store.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// A compare-and-swap of the lease lost: another run holds or took it.
    #[error("the lease was taken over or released by another run")]
    LeaseLost,
    /// A compare-and-swap that creates or takes over a lease found another one.
    #[error("the lease changed while it was being taken")]
    LeaseRace,
    /// The fenced push lost to a commit made outside this run.
    #[error("state ref {reference} moved to {actual} while this run held the lease")]
    Conflict { reference: String, actual: String },
    /// A state ref moved backwards or sideways, which only a manual rewrite produces.
    #[error("state ref {reference} is not a fast-forward of what Nyl last read; repair it before running Nyl")]
    NotFastForward { reference: String },
    /// A state document cannot be read.
    #[error("{0}")]
    Corrupt(String),
    /// The store cannot be reached or written.
    #[error("{0}")]
    Unavailable(String),
}

pub type StateResult<T> = std::result::Result<T, StateError>;

/// The commits a snapshot was read at; `None` for a ref that does not exist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateTips {
    pub desired: Option<CommitId>,
    pub observed: Option<CommitId>,
}

/// One consistent read of an environment's desired and observed state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvironmentSnapshot {
    pub tips: StateTips,
    /// `state.yaml`; `None` where no state was initialized.
    pub record: Option<StateRecord>,
    pub environment: Option<EnvironmentRecord>,
    pub desired: BTreeMap<UnitName, DesiredUnit>,
    pub observed: BTreeMap<UnitName, ObservedUnit>,
    /// Artifacts of the latest receipts, by unit and artifact name.
    pub artifacts: BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
}

/// What one operation writes: `Some` puts a document, `None` removes it.
/// The store maps the changes onto its layout and writes at most one commit
/// per state ref.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransitionChanges {
    pub record: Option<StateRecord>,
    pub environment: Option<EnvironmentRecord>,
    pub desired: BTreeMap<UnitName, Option<DesiredUnit>>,
    pub observed: BTreeMap<UnitName, Option<ObservedUnit>>,
    /// The complete artifact set of a unit's latest receipt; `None` removes all.
    pub artifacts: BTreeMap<UnitName, Option<BTreeMap<String, ArtifactDocument>>>,
}

impl TransitionChanges {
    pub fn is_empty(&self) -> bool {
        self.record.is_none()
            && self.environment.is_none()
            && self.desired.is_empty()
            && self.observed.is_empty()
            && self.artifacts.is_empty()
    }
}

/// A transition commit: its machine-parsed message and its changes.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// The commit message: summary line, machine-readable summary, and trailers.
    pub message: String,
    pub changes: TransitionChanges,
}

/// The commits a transition wrote; `None` for a ref it left unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Committed {
    pub desired: Option<CommitId>,
    pub observed: Option<CommitId>,
    /// The lease's version after the fenced push, which moved it, so that
    /// releasing the lease afterwards compares against it.
    pub lease_version: String,
}

/// A lease as read, with the version a compare-and-swap expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLease {
    pub lease: Lease,
    pub version: String,
}

/// A run ref as read: its record, its checkpoints, and its version.
#[derive(Debug, Clone, PartialEq)]
pub struct RunState {
    pub run: Run,
    pub checkpoints: Vec<Checkpoint>,
    pub version: String,
}

/// One finished unit of a run: the desired document it executed, with its
/// uid, and its receipt or condition with the receipt's artifacts.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub desired: DesiredUnit,
    pub observed: ObservedUnit,
    pub artifacts: BTreeMap<String, ArtifactDocument>,
}

/// The coordination refs of one environment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Coordination {
    pub lease: Option<HeldLease>,
    pub runs: Vec<RunState>,
}

/// An environment's state, wherever it lives. The Git implementation is
/// `nyl-state`; an in-memory one serves the scenario tests.
///
/// Every write that changes state goes through [`StateStore::commit`],
/// fenced by the run's lease, so a run whose lease was taken over can never
/// overwrite state.
pub trait StateStore: Send + Sync {
    /// Read both state refs once.
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot>;

    /// Read the lease and every run ref.
    fn coordination(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<Coordination>;

    /// Create the lease, or replace an expired or broken one, by compare-and-swap
    /// against `expected` (`None`: no lease exists). Returns the new version.
    fn take_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: Option<&str>,
    ) -> StateResult<String>;

    /// Update the held lease by compare-and-swap; [`StateError::LeaseLost`] when it changed.
    fn update_lease(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        expected: &str,
    ) -> StateResult<String>;

    /// Delete the held lease by compare-and-swap.
    fn release_lease(&self, location: &StateLocation, environment: &EnvironmentName, expected: &str)
        -> StateResult<()>;

    /// Delete a lease regardless of its holder, as someone with push access
    /// could. `nyl lease break` instead replaces the lease with a broken one,
    /// so the next transition commit records the break.
    fn delete_lease(&self, location: &StateLocation, environment: &EnvironmentName) -> StateResult<()>;

    /// Write a run's record, adding a checkpoint when given.
    fn write_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        checkpoint: Option<&Checkpoint>,
    ) -> StateResult<()>;

    /// Delete a run ref and its signal ref.
    fn delete_run(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()>;

    /// Read the signals sent to a run.
    fn signals(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId)
        -> StateResult<Vec<Signal>>;

    /// Send a signal to a run.
    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()>;

    /// Write one transition commit per changed state ref on top of `base`,
    /// atomically with a compare-and-swap of the lease at `lease_version`.
    /// The lease moves to a new version in the same push, because a lease
    /// that does not change is never checked.
    fn commit(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_version: &str,
    ) -> StateResult<Committed>;

    /// Give `to` the history of `from`'s state refs, for `nyl state move`:
    /// each target ref is created at its source's tip, or left alone when it
    /// already is there, so an interrupted move completes when run again. A
    /// target ref at another commit is a conflict. Both locations keep their
    /// own `path`; a location that shares its refs through `path` has its
    /// directory moved with a commit instead, by the caller.
    fn import_history(&self, from: &StateLocation, to: &StateLocation) -> StateResult<()>;
}

/// The repository a source names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceRepository {
    /// The repository a run works in.
    This,
    /// Another repository, by credential-free URL.
    Url(String),
}

/// A failure of source operations.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SourceError(pub String);

pub type SourceResult<T> = std::result::Result<T, SourceError>;

/// Every Git operation on sources that decisions depend on.
pub trait SourceResolver: Send + Sync {
    /// The commit a revision names now.
    fn resolve_revision(&self, repository: &SourceRepository, revision: &str) -> SourceResult<CommitId>;
    /// The first of `refs` (names or globs) whose tip contains `commit`, or
    /// `None` when none does.
    fn reachable_from(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        refs: &[String],
    ) -> SourceResult<Option<String>>;
    /// A detached checkout of `commit` in a directory the caller owns.
    fn worktree(&self, repository: &SourceRepository, commit: &CommitId, path: &std::path::Path) -> SourceResult<()>;
    /// The repository's default branch, the default of `protectedRefs`.
    fn default_branch(&self, repository: &SourceRepository) -> SourceResult<String>;
}

/// One unit as an environment's source renders it.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedUnit {
    pub envelope: UnitEnvelope,
    /// Worktree-rooted path of the file declaring the unit.
    pub source_path: String,
    /// Whether the environment's `unitSelector` selects the unit.
    pub selected: bool,
    /// The rendered spec; `None` when the document rendered to nothing.
    pub spec: Option<Value>,
}

/// An environment's source at one commit: its resolved Environment and every
/// unit discovery found, selected or not.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedSource {
    pub source_commit: CommitId,
    /// The Environment with group defaults applied.
    pub environment: Environment,
    pub units: BTreeMap<UnitName, RenderedUnit>,
}

/// Renders an environment's units from a worktree of its source commit.
pub trait SourceRenderer: Send + Sync {
    fn render(
        &self,
        worktree: &std::path::Path,
        environment: &EnvironmentName,
        commit: &CommitId,
    ) -> SourceResult<RenderedSource>;
}

/// What an Argo CD observer reports for a target's workload Applications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationsObservation {
    /// The Applications that still exist; empty when all are gone.
    Remaining(Vec<String>),
    /// The observer could not read them, such as for missing credentials.
    Failed(String),
}

/// Reads Argo CD for teardown waits and health evidence.
pub trait ArgoObserver: Send + Sync {
    fn workload_applications(&self, target: &str) -> ApplicationsObservation;
}
