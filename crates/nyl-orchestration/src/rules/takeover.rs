//! Run takeover: what a run that took over an expired or broken lease
//! imports from the leftover run refs.
//!
//! Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
//! items 3 (checkpoints), 4 (takeover), 6 (lost lease), and 8 (breaking a
//! lease), and the walkthrough
//! [Effects without a receipt](../../../../design/orchestration-core.md#walkthroughs).
//!
//! - Before desired documents are built, the run adopts the checkpointed or
//!   recorded desired document and uid of every unit a leftover run created
//!   and started, with or without a checkpoint ([`adoptions`]); a unit no
//!   longer selected then leaves the ownership set like any other.
//! - After resolving, a checkpointed result becomes the unit's receipt (or
//!   condition) when the new resolution has the same execution key, and the
//!   unit is listed under `imported` ([`import`]).
//! - Other checkpointed results are superseded: reported with
//!   `NYL-RUN-SUPERSEDED`, and a superseded result of a unit whose execution
//!   recovery policy is not `converge` gives it an `uncertain` condition.
//! - Units in a leftover run's `units` (or the replaced lease's) without a
//!   checkpoint get an `uncertain` condition, such as "run lost its lease";
//!   they are listed under `recovered`.
//! - A checkpoint of a unit that is already `uncertain` is a late checkpoint:
//!   evidence only (`NYL-RUN-LATE-CHECKPOINT`), never clearing the condition.
//! - Import is idempotent and never regresses state: a takeover condition
//!   records the dead run's id, and a unit whose condition already names
//!   that run is left alone; a checkpoint applies only when it is newer than
//!   the unit's observed document; a (run, unit) pair is handled once per
//!   run ([`ImportInputs::handled`]).
//! - Adopted units the new resolution no longer selects are imported too:
//!   their checkpoint becomes their receipt (there is no new key to compare)
//!   and, without one, they become `uncertain`, so the transition commit
//!   records state for every resource a dead run created.
//! - A `--local` run never imports, retires, or deletes run refs it read
//!   from the remote ([`RunState::foreign`]), and a replaced lease counts only
//!   when its operation has effects ([`leftovers`]).
//! - Every change goes through [`crate::rules::transitions::apply`]. After
//!   the transition commit lands, [`consumed`] decides which leftover run
//!   refs are deleted and which are retired to evidence refs, because a
//!   checkpoint on them is still evidence for an `uncertain` unit that
//!   `nyl recover --accept` may adopt.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{RunId, UnitName};
use nyl_core::orchestration::ports::{Coordination, RunState};
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Lease, ObservedUnit};
use nyl_core::orchestration::time::Timestamp;

/// What a run that took over the lease found of the runs before it.
#[derive(Debug, Clone)]
pub struct Leftovers<'a> {
    /// The run taking over; its own run ref is never a leftover.
    pub run: &'a RunId,
    /// The expired or broken lease it replaced, when its operation has
    /// effects (reconcile, teardown, exec) and the run may take its `units`
    /// as started; `None` otherwise, and always for a `--local` run that
    /// took the remote lease.
    pub replaced: Option<&'a Lease>,
    /// The run refs (`<prefix><env>/runs/*`) to import: every one read after
    /// taking the lease, except the run's own and foreign ones.
    pub runs: Vec<&'a RunState>,
    /// Retired run refs (`<prefix><env>/evidence/*`): never imported again,
    /// only consulted for late checkpoints and deleted once no unit needs them.
    pub evidence: Vec<&'a RunState>,
}

/// The leftovers of `coordination` for `run`: drops the run's own ref and
/// foreign ones, and keeps `replaced` only for an operation with effects.
pub fn leftovers<'a>(run: &'a RunId, replaced: Option<&'a Lease>, coordination: &'a Coordination) -> Leftovers<'a> {
    let _ = (run, replaced, coordination);
    todo!("takeover")
}

/// The desired documents, with their uids, to adopt for units leftover runs
/// created and started (their checkpoints' `desired` documents and
/// [`RunState::created`]), by unit. The shell overlays them on the
/// snapshot's desired documents passed to `rules::desired`, so their uids
/// are reused, but always diffs its transition against the real snapshot.
pub fn adoptions(leftovers: &Leftovers<'_>) -> BTreeMap<UnitName, DesiredUnit> {
    let _ = leftovers;
    todo!("takeover")
}

/// What import reads besides the leftovers.
pub struct ImportInputs<'a> {
    pub leftovers: &'a Leftovers<'a>,
    /// The time conditions are recorded at.
    pub now: Timestamp,
    /// The units as this run resolved them, with execution keys where resolved.
    pub resolved: &'a BTreeMap<UnitName, DesiredUnit>,
    /// Adopted units the run does not resolve (no longer selected): imported
    /// without a key comparison.
    pub adopted: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The observed documents as the run's snapshot read them.
    pub snapshot: &'a BTreeMap<UnitName, ObservedUnit>,
    /// The snapshot's observed documents plus everything imported so far.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    /// The (run, unit) pairs earlier calls in this run already handled
    /// (imported, superseded, marked uncertain, or reported as late), which
    /// this call neither applies nor reclassifies again.
    pub handled: &'a BTreeSet<(RunId, UnitName)>,
    /// For each unit's recovery policies.
    pub drivers: &'a DriverSet,
}

/// What a takeover imports into the run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Import {
    /// New observed documents, overlaid on the run's evidence and written in
    /// its transition commit.
    pub observed: BTreeMap<UnitName, ObservedUnit>,
    /// The artifact sets of imported receipts.
    pub artifacts: BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    /// Units whose checkpointed result became their receipt or condition:
    /// the summary's `imported`.
    pub imported: Vec<UnitName>,
    /// Units given an `uncertain` condition by the takeover (started without
    /// a checkpoint, or a superseded result): the summary's `recovered`,
    /// with the units the run then recovers by executing or inspecting.
    pub recovered: Vec<UnitName>,
    /// `NYL-RUN-SUPERSEDED` and `NYL-RUN-LATE-CHECKPOINT` warnings.
    pub warnings: Vec<Finding>,
    /// The (run, unit) pairs this call handled, for the shell to accumulate.
    pub handled: BTreeSet<(RunId, UnitName)>,
}

/// Import the leftover runs' checkpoints into a run that resolved `resolved`.
/// Units whose execution key is not resolved yet are left for a later call
/// at the next wave's resolution, so call it until it imports nothing new.
pub fn import(inputs: &ImportInputs<'_>) -> Import {
    let _ = inputs;
    todo!("takeover")
}

/// What happens to the leftover and retired run refs after the takeover's
/// transition commit lands (or after a run that changed nothing releases).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Consumption {
    /// Leftover run refs (and signal refs) to delete: nothing on them is
    /// evidence for a unit that is `uncertain` at the end of the run.
    pub delete: Vec<RunId>,
    /// Leftover run refs to retire to evidence refs: a checkpoint on them is
    /// evidence for a unit still `uncertain` at the end of the run.
    pub retire: Vec<RunId>,
    /// Evidence refs to delete: no unit's `uncertain` condition needs them.
    pub delete_evidence: Vec<RunId>,
}

/// Decide [`Consumption`] from the observed documents as the run's
/// transition commit wrote them. Called once, after the waves.
pub fn consumed(leftovers: &Leftovers<'_>, observed: &BTreeMap<UnitName, ObservedUnit>) -> Consumption {
    let _ = (leftovers, observed);
    todo!("takeover")
}
