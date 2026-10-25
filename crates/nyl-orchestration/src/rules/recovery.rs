//! `nyl recover`: clearing a condition, or adopting a checkpointed result.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery)
//! and [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases)
//! (late checkpoints).
//!
//! - `--retry` clears an uncertain condition or a failure; it never
//!   executes, so the unit becomes ready for the next `reconcile`.
//! - `--accept <run>` adopts the receipt, outputs, and artifacts `<run>`
//!   checkpointed for the unit, allowed only when the checkpoint's execution
//!   key and uid equal the unit's current desired document's.
//! - Both refuse a unit without a condition (`NYL-RECOVER-NOTHING`); accept
//!   refuses a run without such a checkpoint (`NYL-RECOVER-CHECKPOINT-MISSING`)
//!   or with another key (`NYL-RECOVER-KEY-MISMATCH`).
//! - Changes go through [`crate::rules::transitions::apply`].

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId, UnitName};
use nyl_core::orchestration::ports::RunState;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, ObservedUnit};
use serde::Serialize;

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoverAction {
    /// `--retry`.
    Retry,
    /// `--accept <run>`.
    Accept { run: RunId },
}

/// What `recover` writes for one unit.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovery {
    pub observed: ObservedUnit,
    /// The accepted checkpoint's artifact set; `None` leaves artifacts as they are.
    pub artifacts: Option<BTreeMap<String, ArtifactDocument>>,
}

/// Decide `action` for `unit` from its current documents and the run refs.
pub fn decide(
    environment: &EnvironmentName,
    unit: &UnitName,
    desired: Option<&DesiredUnit>,
    observed: Option<&ObservedUnit>,
    action: &RecoverAction,
    runs: &[RunState],
) -> Result<Recovery, Finding> {
    let _ = (environment, unit, desired, observed, action, runs);
    todo!("recover-verify")
}

/// A checkpoint of a unit on a run ref, shown next to the unit's condition
/// as evidence for `--accept`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointEvidence {
    pub run: RunId,
    /// `receipt`, `failed`, or `uncertain`.
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_key: Option<ExecutionKey>,
    /// Whether `--accept` would adopt it: its key and uid equal the current
    /// desired document's.
    pub acceptable: bool,
}

/// Every checkpoint of `unit` on the run refs, newest run first.
pub fn evidence(unit: &UnitName, desired: Option<&DesiredUnit>, runs: &[RunState]) -> Vec<CheckpointEvidence> {
    let _ = (unit, desired, runs);
    todo!("recover-verify")
}
