//! Which units a wave executes.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution),
//! step 3 and `--unit`. A unit is ready when its desired document is ready
//! and active, it has no current receipt, every receipt its provenance cites
//! is still current, and it does not await approval. Uncertain units are
//! skipped unless their recovery policy is `converge`; non-retryable failed
//! units wait for `recover`; retryable failed units are selected again (once
//! per run). With `--unit`, only the named units execute: their
//! dependencies and dependents never do.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit};

/// What selection reads.
pub struct Candidates<'a> {
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The snapshot's observed documents overlaid with this run's checkpoints.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    pub drivers: &'a DriverSet,
    /// `--unit`/`--units`; `None` selects every unit.
    pub named: Option<&'a BTreeSet<UnitName>>,
    /// Units this run already executed, whatever the outcome.
    pub attempted: &'a BTreeSet<UnitName>,
}

/// Why a unit is not selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skipped {
    NotNamed,
    Blocked,
    Current,
    /// Uncertain, and its recovery policy is not `converge`.
    Uncertain,
    FailedNonRetryable,
    AwaitingApproval,
    AlreadyAttempted,
}

/// The ready units, by name, and why each other unit is skipped.
pub fn select(candidates: &Candidates<'_>) -> (Vec<UnitName>, BTreeMap<UnitName, Skipped>) {
    let _ = candidates;
    todo!("references-graph")
}

/// Split the ready units into batches of at most `concurrency` (at least 1)
/// that run in parallel within one wave.
pub fn batches(ready: &[UnitName], concurrency: usize) -> Vec<Vec<UnitName>> {
    let _ = (ready, concurrency);
    todo!("references-graph")
}
