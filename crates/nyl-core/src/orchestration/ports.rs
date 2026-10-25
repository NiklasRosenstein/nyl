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
    ArtifactDocument, Attestation, DesiredUnit, EnvironmentRecord, Lease, Observation, ObservationType, ObservedUnit,
    Run, Signal, StateLocation, StateRecord,
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

/// Who writes state commits: their author and committer. The command line
/// resolves it once, from `--author` or Git's `user.name` and `user.email`,
/// and fails with [`Code::CommitIdentityMissing`](super::codes::Code::CommitIdentityMissing)
/// when neither names one; `nyl-state` never reads Git configuration and
/// takes each commit's time from the injected [`Clock`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIdentity {
    pub name: String,
    pub email: String,
}

impl std::str::FromStr for CommitIdentity {
    type Err = String;

    /// Parse `Name <email>`, as `--author` takes it.
    fn from_str(value: &str) -> std::result::Result<Self, String> {
        let invalid = || format!("{value:?} is not an identity of the form \"Name <email>\"");
        let (name, rest) = value.split_once('<').ok_or_else(invalid)?;
        let email = rest.strip_suffix('>').ok_or_else(invalid)?.trim();
        let name = name.trim();
        if name.is_empty() || email.is_empty() || email.contains(['<', '>']) {
            return Err(invalid());
        }
        Ok(Self {
            name: name.to_owned(),
            email: email.to_owned(),
        })
    }
}

impl std::fmt::Display for CommitIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} <{}>", self.name, self.email)
    }
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
    /// The latest observation per type, by unit
    /// (`observed/units/<unit>/observations/<type>.yaml`), as `nyl verify`
    /// writes them. Contract: [Commands](../../../../design/orchestration-core.md#commands).
    pub observations: BTreeMap<UnitName, BTreeMap<ObservationType, Observation>>,
    /// Driver attestations by unit and name
    /// (`observed/units/<unit>/attestations/<name>.yaml`).
    pub attestations: BTreeMap<UnitName, BTreeMap<String, Attestation>>,
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
    /// The complete set of a unit's latest observations, one per type;
    /// `None` removes all. Written by `nyl verify`.
    pub observations: BTreeMap<UnitName, Option<BTreeMap<ObservationType, Observation>>>,
    /// The complete set of a unit's driver attestations by name; `None`
    /// removes all. Written by `nyl verify`.
    pub attestations: BTreeMap<UnitName, Option<BTreeMap<String, Attestation>>>,
}

impl TransitionChanges {
    pub fn is_empty(&self) -> bool {
        self.record.is_none()
            && self.environment.is_none()
            && self.desired.is_empty()
            && self.observed.is_empty()
            && self.artifacts.is_empty()
            && self.observations.is_empty()
            && self.attestations.is_empty()
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
    /// The desired documents, with their new uids, of units the run created
    /// and started, recorded at each unit's start by
    /// [`StateStore::write_run_created`], so a run taking over adopts them
    /// whether or not a checkpoint followed. Contract:
    /// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 4.
    pub created: Vec<DesiredUnit>,
    pub version: String,
    /// Set by a `--local` run's store on run refs it read from the remote
    /// while the run works on the clone's local branches: such a run never
    /// imports, retires, or deletes them; they only block it while live.
    /// Stores themselves always return `false`. Contract:
    /// [Local runs](../../../../design/orchestration-core.md#local-runs).
    pub foreign: bool,
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
    /// Retired run refs (`<prefix><env>/evidence/<run>`): checkpoints a
    /// takeover already imported that are still evidence for an `uncertain`
    /// unit, kept for `nyl recover` and never imported again or counted as a
    /// lease. Contract:
    /// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 4.
    pub evidence: Vec<RunState>,
}

/// One of an environment's two state refs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateSide {
    Desired,
    Observed,
}

impl StateSide {
    pub const ALL: [Self; 2] = [Self::Desired, Self::Observed];

    /// `desired` or `observed`, as reports key commits.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desired => "desired",
            Self::Observed => "observed",
        }
    }
}

/// One commit of a state ref's first-parent history, with its raw message.
/// The store never interprets messages; `nyl-orchestration` parses their
/// trailers, such as `Nyl-Local`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub commit: CommitId,
    pub message: String,
}

/// How a clone's local state branch relates to the remote state ref it
/// follows: commits only the local branch has, and commits only the remote has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Divergence {
    pub ahead: usize,
    pub behind: usize,
}

/// The divergence of both local state branches; `None` for a side where
/// the local branch or the remote ref does not exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalDivergence {
    pub desired: Option<Divergence>,
    pub observed: Option<Divergence>,
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

    /// Read the state as of `tips` instead of the refs' current tips, for
    /// `nyl get --state-revision`. A tip of `None` reads that side as empty.
    fn snapshot_at(&self, location: &StateLocation, tips: &StateTips) -> StateResult<EnvironmentSnapshot> {
        let _ = (location, tips);
        Err(StateError::Unavailable(
            "this state store cannot read past state revisions".to_owned(),
        ))
    }

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

    /// Retire a run ref a takeover imported: move its record, checkpoints,
    /// and created units to the run's evidence ref (`<prefix><env>/evidence/<run>`),
    /// appending its checkpoints when that ref exists already, then delete
    /// the run ref and its signal ref. Read back in
    /// [`Coordination::evidence`]. Contract:
    /// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 4.
    fn retire_run(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()>;

    /// Delete a run's evidence ref, once no unit's condition needs it.
    fn delete_evidence(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()>;

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

    /// Record on the run ref the desired documents, with their new uids, of
    /// units the run created and is about to start, together with the run
    /// record; read back as [`RunState::created`]. Contract:
    /// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), items 3 and 4.
    fn write_run_created(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        let _ = (location, environment, run, created);
        Err(StateError::Unavailable(
            "this state store cannot record created units on run refs".to_owned(),
        ))
    }

    /// The newest `limit` commits of one state ref's first-parent history,
    /// newest first, as last fetched (a remote location) or as the local
    /// branch holds them (a [`StateLocation::LOCAL_REPOSITORY`] location).
    /// Contract: [Local runs](../../../../design/orchestration-core.md#local-runs).
    fn log(&self, location: &StateLocation, side: StateSide, limit: usize) -> StateResult<Vec<LogEntry>> {
        let _ = (location, side, limit);
        Err(StateError::Unavailable(
            "this state store cannot read state history".to_owned(),
        ))
    }

    /// How the clone's local branches of `location`'s state refs relate to
    /// the remote refs, as of the last fetch. Contract:
    /// [Local runs](../../../../design/orchestration-core.md#local-runs).
    fn local_divergence(&self, location: &StateLocation) -> StateResult<LocalDivergence> {
        let _ = location;
        Err(StateError::Unavailable(
            "this state store keeps no local state branches".to_owned(),
        ))
    }

    /// Move the clone's local branches of `location`'s (remote) state refs
    /// back to the remote tips, by compare-and-swap against `expected`, the
    /// local tips the caller read: `nyl state reset --local`, the only write
    /// that moves a local state branch other than forward. Refused for a
    /// [`StateLocation::LOCAL_REPOSITORY`] location, whose branches are the
    /// state. Returns the new local tips. Contract:
    /// [Local runs](../../../../design/orchestration-core.md#local-runs).
    fn reset_local(&self, location: &StateLocation, expected: &StateTips) -> StateResult<StateTips> {
        let _ = (location, expected);
        Err(StateError::Unavailable(
            "this state store keeps no local state branches".to_owned(),
        ))
    }

    /// A `--local` run's transition commit: one commit per changed side on
    /// the clone's local branches of `location`'s state refs on top of
    /// `base`, never pushed, fenced by first renewing the lease held at
    /// `lease_location` (the remote location, or the local view with
    /// `--local-lease`) by compare-and-swap at `lease_version`. A lost lease
    /// writes nothing and returns [`StateError::LeaseLost`]. Contract:
    /// [Local runs](../../../../design/orchestration-core.md#local-runs).
    fn commit_local(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        base: &StateTips,
        transition: &Transition,
        lease_location: &StateLocation,
        lease_version: &str,
    ) -> StateResult<Committed> {
        let _ = (location, environment, base, transition, lease_location, lease_version);
        Err(StateError::Unavailable(
            "this state store cannot commit to local state branches".to_owned(),
        ))
    }
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
    /// Make `path` exactly a fresh detached checkout of `commit`: create it
    /// when missing or empty, and otherwise reset a worktree this resolver
    /// made earlier, removing untracked and ignored files, so a run's
    /// concurrency slot reuses one worktree across units. The default
    /// removes the directory and checks out again. Contract:
    /// [Execution](../../../../design/orchestration-core.md#execution).
    fn reset_worktree(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        path: &std::path::Path,
    ) -> SourceResult<()> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(SourceError(format!("cannot remove {}: {error}", path.display()))),
        }
        std::fs::create_dir_all(path)
            .map_err(|error| SourceError(format!("cannot create {}: {error}", path.display())))?;
        self.worktree(repository, commit, path)
    }
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
    /// Worktree-rooted directory containing `nyl.toml` at the source commit,
    /// such as `/nyl`, or `/` at the worktree root. Drivers resolve local
    /// paths against it; resolved specs record paths worktree-rooted.
    pub project_dir: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_identity_parses_the_author_form_and_rejects_others() {
        let identity: CommitIdentity = "Jane Doe <jane@example.com>".parse().unwrap();
        assert_eq!(identity.name, "Jane Doe");
        assert_eq!(identity.email, "jane@example.com");
        assert_eq!(identity.to_string(), "Jane Doe <jane@example.com>");
        for invalid in ["Jane Doe", "<jane@example.com>", "Jane <>", "Jane <a<b>"] {
            assert!(invalid.parse::<CommitIdentity>().is_err(), "{invalid}");
        }
    }
}
