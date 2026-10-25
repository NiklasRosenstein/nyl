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
//! - Every change goes through [`crate::rules::transitions::apply`]. The
//!   imported run refs, and their signal refs, are deleted by the importing
//!   run after its transition commit lands.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{RunId, UnitName};
use nyl_core::orchestration::ports::RunState;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Lease, ObservedUnit};
use nyl_core::orchestration::time::Timestamp;

/// What a run that took over the lease found of the runs before it.
#[derive(Debug, Clone, Copy)]
pub struct Leftovers<'a> {
    /// The run taking over; its own run ref is never a leftover.
    pub run: &'a RunId,
    /// The expired or broken lease it replaced, if a lease ref existed.
    pub replaced: Option<&'a Lease>,
    /// Every run ref read after taking the lease, its own excluded by the rule.
    pub runs: &'a [RunState],
}

/// The desired documents, with their uids, to adopt for units leftover runs
/// created and started, by unit; the shell overlays them on the snapshot's
/// desired documents before `rules::desired` runs, so their uids are reused.
pub fn adoptions(leftovers: &Leftovers<'_>) -> BTreeMap<UnitName, DesiredUnit> {
    let _ = leftovers;
    todo!("takeover")
}

/// What import reads besides the leftovers.
pub struct ImportInputs<'a> {
    pub leftovers: Leftovers<'a>,
    /// The time conditions are recorded at.
    pub now: Timestamp,
    /// The units as this run resolved them, with execution keys where resolved.
    pub resolved: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The observed documents as of the snapshot plus anything imported so far.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
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
    /// Units given an `uncertain` condition by the takeover: the summary's
    /// `recovered` (with the units the run then recovers by executing).
    pub recovered: Vec<UnitName>,
    /// `NYL-RUN-SUPERSEDED` and `NYL-RUN-LATE-CHECKPOINT` warnings.
    pub warnings: Vec<Finding>,
    /// The leftover runs this import consumed, deleted after the commit lands.
    pub consumed: Vec<RunId>,
}

/// Import the leftover runs' checkpoints into a run that resolved `resolved`.
/// Units whose execution key is not resolved yet are left for a later call
/// at the next wave's resolution, so call it until it imports nothing new.
pub fn import(inputs: &ImportInputs<'_>) -> Import {
    let _ = inputs;
    todo!("takeover")
}
