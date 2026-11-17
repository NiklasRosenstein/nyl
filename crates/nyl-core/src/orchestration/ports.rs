//! The traits through which the orchestration shell performs every effect.
//!
//! Decisions are pure functions over snapshots; the shell in
//! `nyl-orchestration` reads and writes only through these traits, and
//! `nyl-cli` passes in the implementations from `nyl-state` and
//! `nyl-units`. Each trait has an in-memory or fake implementation used by
//! the scenario tests. See the implementation architecture's "Formats and
//! traits" (`design/implementation-architecture.md`).

use std::collections::BTreeMap;

use serde_json::Value;

use super::ids::{CommitId, EnvironmentName, PromotionPathName, RunId, Uid, UnitName};
use super::promotion::PromotionRecord;
use super::state::{
    ArtifactDocument, Attestation, DesiredUnit, DriftRecord, EnvironmentRecord, Lease, ObservationRecord, ObservedUnit,
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

/// A clock that stands still: `now` is fixed and a wait returns at once. For
/// calls that never wait, such as `nyl build`.
#[derive(Debug, Clone, Copy)]
pub struct FrozenClock(pub Timestamp);

impl Clock for FrozenClock {
    fn now(&self) -> Timestamp {
        self.0
    }

    fn wait(&self, _duration: Duration) {}
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

impl StateError {
    /// A [`StateError::Conflict`] on `reference`, which is at `actual`, or
    /// absent (`nothing`) for `None`.
    pub fn conflict(reference: impl Into<String>, actual: Option<impl std::fmt::Display>) -> Self {
        Self::Conflict {
            reference: reference.into(),
            actual: actual.map_or_else(|| "nothing".to_owned(), |actual| actual.to_string()),
        }
    }
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
    /// The latest drift detection per unit (`observed/units/<unit>/drift.yaml`),
    /// as `nyl detect-drift` writes it. Contract: [Drift](../../../../design/orchestration-core.md#drift).
    pub drift: BTreeMap<UnitName, DriftRecord>,
    /// The latest observed values per unit
    /// (`observed/units/<unit>/observations.yaml`), as the driver's `observe`
    /// read them for the unit's current receipt. Contract:
    /// [Observed values](../../../../design/orchestration-core.md#observed-values).
    pub observations: BTreeMap<UnitName, ObservationRecord>,
    /// Driver attestations by unit and name
    /// (`observed/units/<unit>/attestations.yaml`, one record per unit).
    pub attestations: BTreeMap<UnitName, BTreeMap<String, Attestation>>,
    /// The environment-wide attestations by name
    /// (`observed/attestations.yaml`), each naming the desired commit of the
    /// state it attests. Contract:
    /// [Attestations](../../../../design/orchestration-core.md#attestations).
    pub environment_attestations: BTreeMap<String, Attestation>,
    /// The PromotionRecord of each path that promoted into the environment
    /// (`desired/promotions/<path>.yaml`). Contract:
    /// [PromotionRecord](../../../../design/promotion.md#promotionrecord).
    pub promotions: BTreeMap<PromotionPathName, PromotionRecord>,
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
    /// A unit's latest drift detection; `None` removes it. Written by
    /// `nyl detect-drift`, and removed with a new receipt.
    pub drift: BTreeMap<UnitName, Option<DriftRecord>>,
    /// A unit's observed values; `None` removes them. Written by reconcile
    /// and `nyl observe`, and removed with a new receipt.
    pub observations: BTreeMap<UnitName, Option<ObservationRecord>>,
    /// The complete set of a unit's driver attestations by name; `None`
    /// removes all. Written by `nyl attest`, and by reconcile with a receipt.
    pub attestations: BTreeMap<UnitName, Option<BTreeMap<String, Attestation>>>,
    /// The complete set of the environment-wide attestations by name
    /// (`observed/attestations.yaml`); `Some` of an empty set removes the
    /// file, `None` leaves it alone. Written by `nyl attest --name` without
    /// `--unit`.
    pub environment_attestations: Option<BTreeMap<String, Attestation>>,
    /// A path's PromotionRecord on the desired side; `None` removes it.
    pub promotions: BTreeMap<PromotionPathName, Option<PromotionRecord>>,
    /// Remove every file of the environment's state on both refs before
    /// applying the other changes: `state.yaml`, `desired/`, and `observed/`
    /// beneath the location's `path`, leaving other environments' files of a
    /// shared ref alone. `nyl state delete` writes it together with
    /// `record`, whose last event is `state-deleted`, so `state.yaml` stays
    /// as the deleted marker and the refs and their history stay. Contract:
    /// [Instances](../../../../design/orchestration-core.md#instances).
    pub clear: bool,
    /// Write the observed side's commit even when no document changes, so
    /// the transition itself is recorded, such as the takeover of a broken
    /// lease whose import changed nothing. Contract:
    /// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 8.
    pub commit_unchanged: bool,
    /// Write the desired side's commit even when no document changes, so
    /// the transition's message is recorded in the desired history: the
    /// marker `nyl promote` writes into the target's state when the record
    /// itself is kept in source (`record: source`), whose `promote` summary
    /// reserves its sequence and lists it in `nyl get promotions`. Contract:
    /// [Promoting an environment's source](../../../../design/promotion.md#promoting-an-environments-source).
    pub commit_unchanged_desired: bool,
}

impl TransitionChanges {
    pub fn is_empty(&self) -> bool {
        !self.clear
            && !self.commit_unchanged
            && !self.commit_unchanged_desired
            && self.record.is_none()
            && self.environment.is_none()
            && self.desired.is_empty()
            && self.observed.is_empty()
            && self.artifacts.is_empty()
            && self.drift.is_empty()
            && self.observations.is_empty()
            && self.attestations.is_empty()
            && self.environment_attestations.is_none()
            && self.promotions.is_empty()
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
/// uid, and its receipt or condition with the receipt's artifacts,
/// attestations, and observed values.
///
/// The attestations and the observation record belong to the receipt in
/// `observed` and to no other: a run that imports the checkpoint takes them
/// only together with that exact receipt, by execution key, run, and
/// incarnation. A checkpoint of a failure or an `uncertain` outcome carries
/// none. Contract:
/// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 3.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkpoint {
    pub desired: DesiredUnit,
    pub observed: ObservedUnit,
    pub artifacts: BTreeMap<String, ArtifactDocument>,
    /// The receipt's attestations by name, as the run recorded them. Each
    /// keeps its own `at`: import never re-stamps one.
    pub attestations: BTreeMap<String, Attestation>,
    /// The observed values the run read for the receipt, with their own
    /// `observedAt` times.
    pub observations: Option<ObservationRecord>,
}

impl Checkpoint {
    /// A checkpoint of `observed` as `desired` produced it, with `artifacts`
    /// and no attestations or observed values.
    pub fn new(desired: DesiredUnit, observed: ObservedUnit, artifacts: BTreeMap<String, ArtifactDocument>) -> Self {
        Self {
            desired,
            observed,
            artifacts,
            attestations: BTreeMap::new(),
            observations: None,
        }
    }
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

/// One commit of the desired ref's first-parent history, with what only the
/// store can tell: whether it changed this location's `desired/` documents
/// (beneath `location.path`), which is what [`StateStore::state_commit`] asks
/// of each commit it passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredCommit {
    pub commit: CommitId,
    pub message: String,
    pub changed_desired: bool,
}

/// The first-parent histories of both state refs as one synced read of a
/// location: the tips the logs start at, the desired log (the whole ref,
/// newest first, each commit saying whether it changed the documents), and
/// the observed log back past the oldest desired commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateHistory {
    pub tips: StateTips,
    pub desired: Vec<DesiredCommit>,
    pub observed: Vec<LogEntry>,
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
    /// Prepare the backing repository for explicit state initialization, before
    /// taking its lease. Git stores create a missing local bare repository;
    /// stores without separately provisioned storage need no preparation.
    /// Contract: [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves).
    fn prepare_init(&self, _location: &StateLocation) -> StateResult<()> {
        Ok(())
    }

    /// Import and validate an explicitly identified worktree snapshot before a run uses it.
    fn prepare_worktree_snapshot(&self, _location: &StateLocation, _commit: &CommitId) -> StateResult<()> {
        Err(StateError::Unavailable(
            "this state store cannot retain worktree snapshots".to_owned(),
        ))
    }

    /// Read both state refs once.
    fn snapshot(&self, location: &StateLocation) -> StateResult<EnvironmentSnapshot>;

    /// The desired commit of the state at `tips`: the newest commit of the
    /// desired ref, from `tips.desired` back along its first parents, that
    /// changed this environment's `desired/` documents (beneath
    /// `location.path`). `None` when the environment has no desired
    /// documents. Observed-only commits, commits of other environments
    /// sharing the ref, and a ref shared by both sides move the ref's tip but
    /// not this commit, so it names the state an attestation is about.
    /// Contract: [Attestations](../../../../design/orchestration-core.md#attestations).
    fn state_commit(&self, location: &StateLocation, tips: &StateTips) -> StateResult<Option<CommitId>> {
        let _ = (location, tips);
        Err(StateError::Unavailable(
            "this state store cannot name the desired commit of a state".to_owned(),
        ))
    }

    /// The `state.yaml` of every directory directly beneath `location.path`
    /// that has one: how instances of an EnvironmentTemplate that share a
    /// ref are found. Stores that keep no shared refs list none.
    fn list_records(&self, _location: &StateLocation) -> StateResult<Vec<StateRecord>> {
        Ok(Vec::new())
    }

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

    /// Move the held lease at `version` to `lease` and write the run's
    /// record with it, recording `created` as [`Self::write_run_created`]
    /// does: the update at each phase start. A store that can writes both
    /// in one compare-and-swap, so both land or neither does; the default
    /// writes the lease and then the run ref. [`StateError::LeaseLost`] when
    /// the lease changed. On return `version` is the lease's current
    /// version, also when the run ref write failed after the lease moved.
    /// Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 2.
    fn update_lease_and_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        lease: &Lease,
        version: &mut String,
        run: &Run,
        created: &[DesiredUnit],
    ) -> StateResult<()> {
        update_lease_then_run(self, location, environment, lease, version, run, created)
    }

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

    /// Delete a run ref and its signal ref. With `expected` (a
    /// [`RunState::version`]), only while the run ref is still at that
    /// version; otherwise [`StateError::Conflict`], so a checkpoint written
    /// after the caller read the ref is never lost.
    fn delete_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()>;

    /// Retire a run ref a takeover imported: move its record, checkpoints,
    /// and created units to the run's evidence ref (`<prefix><env>/evidence/<run>`),
    /// appending its checkpoints when that ref exists already, then delete
    /// the run ref and its signal ref. Read back in
    /// [`Coordination::evidence`]. With `expected`, only while the run ref is
    /// still at that version, as for [`StateStore::delete_run`]. Contract:
    /// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), item 4.
    fn retire_run(
        &self,
        location: &StateLocation,
        environment: &EnvironmentName,
        run: &RunId,
        expected: Option<&str>,
    ) -> StateResult<()>;

    /// Delete a run's evidence ref, once no unit's condition needs it.
    fn delete_evidence(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()>;

    /// Read the signals sent to a run.
    fn signals(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId)
        -> StateResult<Vec<Signal>>;

    /// Send a signal to a run.
    fn send_signal(&self, location: &StateLocation, environment: &EnvironmentName, signal: &Signal) -> StateResult<()>;

    /// Delete a run's signal ref, such as a confirmation sent to a run that
    /// finished before it read it. Contract: [Kubernetes publication
    /// unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
    /// the signal path.
    fn delete_signals(&self, location: &StateLocation, environment: &EnvironmentName, run: &RunId) -> StateResult<()> {
        let _ = (location, environment, run);
        Err(StateError::Unavailable(
            "this state store cannot delete signal refs".to_owned(),
        ))
    }

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

    /// The histories of both state refs after one sync of the location, as
    /// [`StateStore::snapshot`] syncs it (a stale local tracking ref never
    /// stands for the remote), with every desired commit's
    /// `changed_desired`. One round trip to a remote, whatever the length of
    /// the history; [`StateStore::snapshots_at`] then reads the objects it
    /// fetched.
    fn history(&self, location: &StateLocation) -> StateResult<StateHistory> {
        let tips = self.snapshot(location)?.tips;
        let desired = self.log(location, StateSide::Desired, usize::MAX)?;
        let observed = self.log(location, StateSide::Observed, usize::MAX)?;
        let mut commits = Vec::with_capacity(desired.len());
        for entry in desired {
            let at = StateTips {
                desired: Some(entry.commit.clone()),
                observed: None,
            };
            let changed = self.state_commit(location, &at)?.as_ref() == Some(&entry.commit);
            commits.push(DesiredCommit {
                commit: entry.commit,
                message: entry.message,
                changed_desired: changed,
            });
        }
        Ok(StateHistory {
            tips,
            desired: commits,
            observed,
        })
    }

    /// The states as of each of `tips`, reading only what the preceding
    /// [`StateStore::history`] of the location made available, without
    /// contacting the remote again.
    fn snapshots_at(&self, location: &StateLocation, tips: &[StateTips]) -> StateResult<Vec<EnvironmentSnapshot>> {
        tips.iter().map(|tips| self.snapshot_at(location, tips)).collect()
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

/// A commit of a record's history whose changes a value's history lacks
/// ([`SourceResolver::missing_commits`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingCommit {
    pub commit: CommitId,
    /// The first line of its message.
    pub subject: String,
    /// Whether it has no parent, so no `<commit>^` names the history before it.
    pub root: bool,
}

/// A failure of source operations.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SourceError(pub String);

pub type SourceResult<T> = std::result::Result<T, SourceError>;

/// [`StateStore::update_lease_and_run`] as two writes: the lease, then the
/// run ref, for stores or locations that cannot write both at once.
pub fn update_lease_then_run<S: StateStore + ?Sized>(
    store: &S,
    location: &StateLocation,
    environment: &EnvironmentName,
    lease: &Lease,
    version: &mut String,
    run: &Run,
    created: &[DesiredUnit],
) -> StateResult<()> {
    *version = store.update_lease(location, environment, lease, version)?;
    if created.is_empty() {
        store.write_run(location, environment, run, None)
    } else {
        store.write_run_created(location, environment, run, created)
    }
}

/// Every Git operation on sources that decisions depend on.
pub trait SourceResolver: Send + Sync {
    /// Freeze Git-visible working files without changing HEAD or the user's index.
    /// `previous` may reuse an identical snapshot retained in state history.
    fn snapshot_worktree(
        &self,
        base: &CommitId,
        identity: &CommitIdentity,
        now: Timestamp,
        previous: Option<&CommitId>,
    ) -> SourceResult<WorktreeSnapshot> {
        let _ = (base, identity, now, previous);
        Err(SourceError(
            "this source resolver cannot snapshot working files".to_owned(),
        ))
    }

    /// Whether a commit is an explicitly marked worktree snapshot.
    fn is_worktree_snapshot(&self, commit: &CommitId) -> SourceResult<bool> {
        let _ = commit;
        Ok(false)
    }

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
    /// concurrency slot reuses one worktree across units. Contract:
    /// [Execution](../../../../design/orchestration-core.md#execution).
    fn reset_worktree(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        path: &std::path::Path,
    ) -> SourceResult<()>;
    /// A checkout restricted to declared worktree-rooted paths and globs.
    /// Adapters without sparse support preserve semantics with a full checkout.
    fn worktree_with_paths(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        path: &std::path::Path,
        paths: Option<&[String]>,
    ) -> SourceResult<()> {
        let _ = paths;
        self.worktree(repository, commit, path)
    }
    /// Reset a private concurrency-slot checkout with the same path selection.
    fn reset_worktree_with_paths(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        path: &std::path::Path,
        paths: Option<&[String]>,
    ) -> SourceResult<()> {
        let _ = paths;
        self.reset_worktree(repository, commit, path)
    }
    /// Ensure a declared source path can be reconstructed in a private checkout.
    /// With `recursive`, check descendants too; otherwise check the path itself
    /// and its ancestors. Paths may contain glob patterns.
    fn validate_source_path(
        &self,
        repository: &SourceRepository,
        commit: &CommitId,
        path: &str,
        recursive: bool,
    ) -> SourceResult<()> {
        let _ = (repository, commit, path, recursive);
        Ok(())
    }
    /// The repository's default branch, the default of `protectedRefs`.
    fn default_branch(&self, repository: &SourceRepository) -> SourceResult<String>;
    /// The commits reachable from `record` and not from `value` that have
    /// no patch-equivalent commit in `value`'s history since their merge
    /// base, oldest first, merge commits skipped: `git cherry <value>
    /// <record>`'s `+` lines. Empty means `value` contains `record`.
    /// Contract: [Several paths into one environment](../../../../design/promotion.md#several-paths-into-one-environment),
    /// "Rule". A resolver that cannot compare histories says so.
    fn missing_commits(
        &self,
        repository: &SourceRepository,
        value: &CommitId,
        record: &CommitId,
    ) -> SourceResult<Vec<MissingCommit>> {
        let _ = (repository, value, record);
        Err(SourceError(
            "this source resolver cannot compare commit histories".to_owned(),
        ))
    }
    /// Whether `commit` is available for history questions, fetched like a
    /// recorded commit when it is missing; `false` when it cannot be found.
    fn fetch_commit(&self, repository: &SourceRepository, commit: &CommitId) -> SourceResult<bool> {
        let _ = (repository, commit);
        Err(SourceError("this source resolver cannot fetch commits".to_owned()))
    }
    /// Whether the entry clone is shallow, so histories end early and
    /// [`Self::missing_commits`] cannot be trusted.
    fn is_shallow(&self) -> bool {
        false
    }
    /// Whether `ancestor` is `of` or an ancestor of it; `false` when either
    /// is not available.
    fn is_ancestor(&self, repository: &SourceRepository, ancestor: &CommitId, of: &CommitId) -> SourceResult<bool> {
        let _ = repository;
        Ok(ancestor == of)
    }
    /// The commits a revision range names, newest first: `A..B` every commit
    /// reachable from `B` and not from `A`, any other revision exactly one.
    fn commits_in(&self, repository: &SourceRepository, range: &str) -> SourceResult<Vec<CommitId>> {
        let _ = (repository, range);
        Err(SourceError("this source resolver cannot resolve ranges".to_owned()))
    }
    /// The credential-free URL a record names the repository by: a URL
    /// repository's own, and for this repository its `origin`, when it has one.
    fn repository_url(&self, repository: &SourceRepository) -> Option<String> {
        match repository {
            SourceRepository::This => None,
            SourceRepository::Url(url) => Some(url.clone()),
        }
    }
    /// Keep `commit` fetchable with the keep ref `reference` of this
    /// repository, a ref of its own for this commit that is never moved:
    /// created when absent, left alone when it already names `commit`, and an
    /// error when it names another commit. Contract:
    /// [Instances](../../../../design/orchestration-core.md#instances),
    /// `keepSource`. A resolver that keeps nothing succeeds without effect.
    fn keep(&self, reference: &str, commit: &CommitId) -> SourceResult<()> {
        let _ = (reference, commit);
        Ok(())
    }
    /// Remove the keep refs under `prefix` (`<prefix>/<commit>`), except the
    /// one for `except`, and return the refs it left alone because they do not
    /// name the commit their name says: they are not this instance's.
    fn release(&self, prefix: &str, except: Option<&CommitId>) -> SourceResult<Vec<String>> {
        let _ = (prefix, except);
        Ok(Vec::new())
    }
}

/// Local source-cache refs for worktree snapshots, keyed by base commit and tree.
/// State and run commits retain the snapshot as a parent in the configured state
/// repository before effects start. These cache refs are not pushed there.
/// Snapshots remain reachable while their cache refs or state history retain them;
/// automatic snapshot garbage collection is not implemented.
pub const WORKTREE_SNAPSHOT_PREFIX: &str = "refs/nyl-worktrees/";

/// Commit trailer distinguishing snapshots from committed-source release evidence.
pub const WORKTREE_SNAPSHOT_MARKER: &str = "Nyl-Worktree-Snapshot: true";

/// Recognize snapshot provenance even after a source is checked out or fetched.
pub fn is_worktree_snapshot_message(message: &str) -> bool {
    message.lines().any(|line| line == WORKTREE_SNAPSHOT_MARKER)
}

/// An immutable invocation-selected source, independent of the state destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    /// The entry checkout's committed HEAD.
    pub base: CommitId,
    /// The immutable snapshot commit whose tree the run consumes.
    pub commit: CommitId,
    /// Content-addressed local ref used by source selection and persisted provenance.
    pub revision: String,
    /// Whether an existing snapshot with the same base and tree was reused.
    pub reused: bool,
}

/// Why a `record: source` PromotionRecord could not be committed. Nothing
/// was written unless the variant says otherwise ([`Self::PushUnknown`],
/// [`Self::UndoFailed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordWriteError {
    /// The entry worktree has no branch checked out.
    Detached,
    /// The checked-out branch has no upstream branch on a remote.
    NoUpstream { branch: String },
    /// The checked-out branch has commits its upstream lacks, which the push
    /// would publish with the record.
    Ahead {
        branch: String,
        upstream: String,
        commits: u64,
    },
    /// The Environment's file is not tracked by Git.
    Untracked { file: String },
    /// The Environment's file has uncommitted changes.
    Dirty { file: String },
    /// The block cannot be placed in the Environment's document, such as a
    /// `fromPromotion` in flow style or one the Environment inherits.
    Unplaceable(String),
    /// The Environment is not declared in a file of the project, such as an
    /// instance of an EnvironmentTemplate.
    Undeclared(String),
    /// The remote refused the fast-forward: the branch moved since the
    /// checkout. The commit was undone.
    Rejected(String),
    /// The remote refused the push for another reason, such as branch
    /// protection or a hook. The commit was undone.
    RemoteRejected(String),
    /// The push failed without saying whether the remote took it, such as a
    /// network error after the upload. The commit is kept on the branch.
    PushUnknown { commit: String, message: String },
    /// The push was refused, and the commit could not be undone.
    UndoFailed { commit: String, message: String },
    /// The Environment trusts its block by signature (`verify`), and the
    /// promotion commit would not be trusted: signing is not configured, or
    /// the commit is not signed by a key the allowed signers file lists.
    /// Nothing was pushed.
    Unsigned(String),
    /// `git` failed otherwise.
    Failed(String),
}

impl std::fmt::Display for RecordWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Detached => formatter.write_str("the entry worktree has no branch checked out (detached HEAD)"),
            Self::NoUpstream { branch } => write!(formatter, "branch {branch} has no upstream branch on a remote"),
            Self::Ahead {
                branch,
                upstream,
                commits,
            } => write!(
                formatter,
                "branch {branch} is {commits} commit(s) ahead of {upstream}, which the push would publish with the record"
            ),
            Self::Untracked { file } => write!(formatter, "{file} is not tracked by Git"),
            Self::Dirty { file } => write!(formatter, "{file} has uncommitted changes"),
            Self::Unplaceable(message)
            | Self::Undeclared(message)
            | Self::Rejected(message)
            | Self::RemoteRejected(message)
            | Self::Unsigned(message)
            | Self::Failed(message) => formatter.write_str(message),
            Self::PushUnknown { commit, message } => {
                write!(formatter, "the push of commit {commit} failed, and may have reached the remote: {message}")
            }
            Self::UndoFailed { commit, message } => write!(
                formatter,
                "the push was refused, and commit {commit} could not be undone in the entry worktree: {message}"
            ),
        }
    }
}

/// Writes the PromotionRecord of an Environment with `record: source` into
/// its `source.fromPromotion.promoted` block: one commit on the branch the
/// entry worktree has checked out, pushed to that branch's upstream
/// fast-forward only, never forced (decision D3 of the M6 plan).
pub trait PromotionRecordWriter: Send + Sync {
    /// Whether `record` could be committed and pushed now, without writing:
    /// what `nyl promote --dry-run` checks.
    fn check(&self, record: &PromotionRecord) -> Result<(), RecordWriteError>;
    /// Commit `record` as the block with `message`, push it, and return the commit.
    fn commit_record(&self, record: &PromotionRecord, message: &str) -> Result<CommitId, RecordWriteError>;
    /// Whether the block in the entry worktree is the one its branch's
    /// upstream carries: a branch is checked out, it is not ahead of its
    /// upstream, and the Environment's file is unchanged. A marker is
    /// written for an existing block only then.
    fn published(&self) -> Result<(), RecordWriteError>;
}

/// Why the signature on a `record: source` block cannot be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockSignatureError {
    /// The commit that last changed the block carries no signature, or one
    /// `git verify-commit` does not accept against the allowed signers.
    Untrusted { commit: CommitId, message: String },
    /// The block's provenance cannot be read: the Environment's file is
    /// untracked or has uncommitted changes, the allowed signers file is
    /// missing, or `git` failed.
    Unreadable(String),
}

impl std::fmt::Display for BlockSignatureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Untrusted { commit, message } => write!(
                formatter,
                "commit {commit}, which last changed the block, has no signature by an allowed signer: {message}"
            ),
            Self::Unreadable(message) => formatter.write_str(message),
        }
    }
}

/// Checks who changed an Environment's `record: source` block last, for
/// `source.fromPromotion.verify` (Contract: [Promoting an environment's
/// source](../../../../design/promotion.md#promoting-an-environments-source)):
/// the commit of the entry clone that last changed the block, as the entry
/// worktree declares it, must carry a signature `git verify-commit` accepts
/// against the allowed signers file. The command line implements it with
/// Git; a run without one cannot trust a block by its signature.
pub trait PromotionBlockVerifier: Send + Sync {
    /// Verify the block of Environment `environment` against the allowed
    /// signers file at the project path `allowed_signers`; the signed commit.
    fn verify_block(&self, environment: &str, allowed_signers: &str) -> Result<CommitId, BlockSignatureError>;
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
    /// Named Git read URLs declared at this source revision.
    pub repositories: BTreeMap<String, String>,
    pub source_commit: CommitId,
    /// Worktree-rooted directory containing `nyl.toml` at the source commit,
    /// such as `/nyl`, or `/` at the worktree root. Drivers resolve local
    /// paths against it; resolved specs record paths worktree-rooted.
    pub project_dir: String,
    /// The Environment with group defaults applied.
    pub environment: Environment,
    /// The template's terms when the environment is an instance, read at this commit.
    pub instance: Option<super::instance::InstanceTerms>,
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

/// Reads a named project secret at execution, never during key calculation.
/// Errors must not contain secret values. Strings remain strings; other
/// values are serialized to compact JSON by the shell.
pub trait SecretProvider: Send + Sync {
    fn get(&self, key: &str) -> Result<Value, String>;
}

/// The terminal of a run that waits on a teardown: when its stdin is a
/// terminal, it shows the remaining time and accepts Enter as a
/// confirmation from the person running it. Contract: [Kubernetes
/// publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
/// the signal path. Absent when stdin is not a terminal.
pub trait OperatorPrompt: Send + Sync {
    /// Show `message` and wait up to `timeout` for Enter; whether it came.
    fn confirm(&self, message: &str, timeout: Duration) -> bool;
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
