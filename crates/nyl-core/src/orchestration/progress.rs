//! Typed execution results and live unit progress.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution).
//! The shell supplies timestamps and results; observers perform presentation.

use super::ids::{CommitId, ExecutionKey, RunId, UnitAddress};
use super::state::{FailureCategory, Operation};
use super::time::Timestamp;
use serde::Serialize;

/// What a reconcile did with a unit, as the report and the transition
/// commit's `units:` block record it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum UnitResult {
    /// The unit already had a current receipt.
    Current,
    #[serde(rename_all = "camelCase")]
    Executed {
        execution_key: ExecutionKey,
    },
    /// A leftover run's checkpoint with the same execution key became the
    /// unit's receipt (`rules::takeover`).
    #[serde(rename_all = "camelCase")]
    Imported {
        execution_key: ExecutionKey,
        run: RunId,
    },
    /// The unit was uncertain, and this run recovered it: executed again
    /// under the `converge` policy, or `inspect` found its effects applied.
    #[serde(rename_all = "camelCase")]
    Recovered {
        execution_key: ExecutionKey,
    },
    Failed {
        category: FailureCategory,
        retryable: bool,
    },
    Uncertain {
        reason: String,
    },
    Blocked {
        reason: String,
    },
    AwaitingApproval,
    /// Held: the run changed nothing about it. `pending` names the source
    /// commit whose changes the hold keeps from applying. `teardown` is set
    /// when the unit has a pending teardown request (a `nyl teardown --unit`
    /// of the held unit, recorded and carried out after `nyl resume`);
    /// `recorded` when this run recorded that request, which is the whole
    /// of a teardown of a held unit and exits 0. Contract:
    /// [Deletion](../../../../design/orchestration-core.md#deletion), holds.
    Held {
        #[serde(skip_serializing_if = "Option::is_none")]
        pending: Option<CommitId>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        teardown: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        recorded: bool,
    },
    /// Current, but its latest drift observation is `drifted` and this run
    /// did not repair it (`driftRepair: manual`, or excluded by `--unit`).
    /// Contract: [Drift](../../../../design/orchestration-core.md#drift).
    Drifted,
    /// Deleting, and its teardown has not started: it awaits
    /// `--allow-teardown`, `--allow-dependents`, a dependent's teardown, an
    /// approval, or a readiness override.
    PendingTeardown {
        reason: String,
    },
    /// Its teardown started and waits, such as a publication under its `teardownWait`.
    TearingDown {
        reason: String,
    },
    /// Left the ownership set with `deletionPolicy: Retain`: its files became
    /// a `retained` tombstone; its resources stay.
    Retained,
    /// Its teardown completed, or its tombstone was torn down: its files were removed.
    Removed,
    /// Its kind cannot tear down: its resources were left in place, as
    /// `left` describes, and its files removed (or kept, held, without an
    /// incarnation).
    Released {
        #[serde(skip_serializing_if = "Option::is_none")]
        left: Option<String>,
    },
}

impl UnitResult {
    /// The result name, as `expect` keys and headlines count it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Executed { .. } => "executed",
            Self::Imported { .. } => "imported",
            Self::Recovered { .. } => "recovered",
            Self::Failed { .. } => "failed",
            Self::Uncertain { .. } => "uncertain",
            Self::Blocked { .. } => "blocked",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Held { .. } => "held",
            Self::Drifted => "drifted",
            Self::PendingTeardown { .. } => "pending-teardown",
            Self::TearingDown { .. } => "tearing-down",
            Self::Retained => "retained",
            Self::Removed => "removed",
            Self::Released { .. } => "released",
        }
    }
}

/// One live lifecycle event for a dispatched unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitProgress {
    pub unit: UnitAddress,
    pub kind: String,
    pub operation: Operation,
    #[serde(flatten)]
    pub phase: UnitPhase,
}

/// Why a unit was selected for execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionReason {
    NoReceipt,
    Retry,
    Recovery,
    Reapply,
    DriftRepair,
    OperatorModified,
    DriverBehaviorChanged,
    InputsChanged,
}

impl ExecutionReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::NoReceipt => "no receipt",
            Self::Retry => "retry after failure",
            Self::Recovery => "recover uncertain execution",
            Self::Reapply => "requested reapply",
            Self::DriftRepair => "repair drift",
            Self::OperatorModified => "reconcile operator changes",
            Self::DriverBehaviorChanged => "driver behavior version changed",
            Self::InputsChanged => "execution inputs changed",
        }
    }
}

/// Start and completion of a unit, including setup failures before tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum UnitPhase {
    Started {
        at: Timestamp,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<ExecutionReason>,
    },
    Finished {
        started: Timestamp,
        at: Timestamp,
        result: UnitResult,
        message: Option<String>,
    },
}

/// Capture of Git-visible working files before unit execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeCaptureProgress {
    Started {
        base: CommitId,
        at: Timestamp,
    },
    Finished {
        snapshot: super::ports::WorktreeSnapshot,
        location: super::state::StateLocation,
        started: Timestamp,
        at: Timestamp,
    },
}

/// Receives live events from concurrent unit executions. Observers must be thread-safe.
pub trait ProgressObserver: Send + Sync {
    fn observe(&self, progress: &UnitProgress);

    /// Receives worktree capture events before unit execution.
    fn observe_worktree_capture(&self, _progress: &WorktreeCaptureProgress) {}
}
