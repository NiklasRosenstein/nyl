//! The typed report of `plan`, `reconcile`, and `status`.
//!
//! Contract: [Planning](../../../../design/orchestration-core.md#planning),
//! [Inspection](../../../../design/orchestration-core.md#inspection) (the
//! `--output json` shape and exit categories), and
//! [Transition commits](../../../../design/orchestration-core.md#transition-commits)
//! (the per-unit summary). Every output of these commands is rendered from
//! one [`RunReport`]: the table, `--output json`, the transition commit's
//! `units:` block and headline, and the exit category. So what people read,
//! what CI parses, and what the commit records cannot disagree.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::Change;
use nyl_core::orchestration::ids::{ChangeDigest, CommitId, EnvironmentName, ExecutionKey, RunId, UnitName};
use nyl_core::orchestration::state::{FailureCategory, Operation};
use serde::Serialize;

use crate::transition::TransitionSummary;

/// API version of every orchestration report.
pub const REPORT_API_VERSION: &str = "gitops.nyl/v1";

/// The operation a report describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReportKind {
    PlanReport,
    ReconcileReport,
    StatusReport,
}

/// The report of one invocation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunReport {
    #[serde(rename = "apiVersion")]
    pub api_version: &'static str,
    pub kind: ReportKind,
    pub environments: Vec<EnvironmentRunReport>,
}

impl RunReport {
    pub fn new(kind: ReportKind) -> Self {
        Self {
            api_version: REPORT_API_VERSION,
            kind,
            environments: Vec::new(),
        }
    }

    /// The invocation's exit category: the most severe of its environments'.
    pub fn exit(&self) -> ExitCategory {
        self.environments
            .iter()
            .map(|environment| environment.exit)
            .fold(ExitCategory::Ok, ExitCategory::most_severe)
    }
}

/// Exit categories, shared by every orchestration command.
///
/// Contract: [Inspection](../../../../design/orchestration-core.md#inspection).
/// The most severe wins, in the order 4 > 3 > 1 > 2 > 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExitCategory {
    /// 0: complete.
    #[default]
    Ok,
    /// 1: an error, such as an invalid declaration or an unreachable source.
    Error,
    /// 2: incomplete: blocked, held, awaiting approval, or the lease is held.
    Incomplete,
    /// 3: an execution or a driver plan failed.
    Failed,
    /// 4: an execution is uncertain, or the run lost its lease.
    Uncertain,
}

impl ExitCategory {
    /// The process exit code.
    pub fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Error => 1,
            Self::Incomplete => 2,
            Self::Failed => 3,
            Self::Uncertain => 4,
        }
    }

    fn severity(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Incomplete => 1,
            Self::Error => 2,
            Self::Failed => 3,
            Self::Uncertain => 4,
        }
    }

    /// The more severe of two categories.
    #[must_use]
    pub fn most_severe(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

impl Serialize for ExitCategory {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.code())
    }
}

/// One environment of a report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentRunReport {
    pub name: EnvironmentName,
    pub exit: ExitCategory,
    /// Every selected unit is current (reconcile, status) or plannable (plan).
    pub complete: bool,
    /// The run, for reconcile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// The source commit S the units were rendered at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    /// Units leaving the ownership set; never filtered by `--unit`.
    pub leaving: Vec<LeavingUnit>,
    pub units: BTreeMap<UnitName, UnitReport>,
    /// The transition commits written, by state ref side (`desired`, `observed`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub commits: BTreeMap<&'static str, CommitId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Finding>,
    pub warnings: Vec<Finding>,
}

impl EnvironmentRunReport {
    pub fn new(name: EnvironmentName) -> Self {
        Self {
            name,
            exit: ExitCategory::Ok,
            complete: false,
            run: None,
            source_commit: None,
            leaving: Vec::new(),
            units: BTreeMap::new(),
            commits: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Set `exit` and `complete` from the units, errors, and the report kind:
    /// plan exits 2 when a unit is blocked or waits and 3 when a driver plan
    /// failed; reconcile and status by the most severe unit result or state.
    /// An error finding is exit 1.
    pub fn finish(&mut self, kind: ReportKind) {
        let _ = kind;
        todo!("run-model")
    }

    /// The transition commit summary of a reconcile: the headline (such as
    /// `2 executed, 1 failed, 1 blocked`) and each unit's [`UnitResult`] in
    /// its flow form, such as `{"result":"failed","category":"tool-error","retryable":true}`.
    pub fn transition_summary(&self, operation: Operation) -> TransitionSummary {
        let _ = operation;
        todo!("run-model")
    }
}

/// A unit leaving the ownership set, printed first with
/// [`Code::PlanUnitsLeaving`](nyl_core::orchestration::codes::Code::PlanUnitsLeaving).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeavingUnit {
    pub unit: UnitName,
    pub kind: String,
    /// Why it leaves: `deselected`, `disabled`, or `removed`.
    pub reason: String,
    /// Grouped consequences, such as the deletion policy that will apply.
    pub consequences: Vec<String>,
}

/// One unit of a report.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitReport {
    pub kind: String,
    /// What a reconcile did; `None` in plan and status reports.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub result: Option<UnitResult>,
    /// The unit's state for plan and status; `None` in reconcile reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<UnitState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ReceiptState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Change>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_digest: Option<ChangeDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<BlockedBy>,
    /// The failure or uncertainty message, kept out of the commit summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The run of the unit's latest receipt or condition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<RunId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Finding>,
}

impl UnitReport {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            result: None,
            state: None,
            receipt: None,
            changes: None,
            change_digest: None,
            summary: None,
            blocked_by: Vec::new(),
            message: None,
            last_run: None,
            warnings: Vec::new(),
        }
    }
}

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
}

impl UnitResult {
    /// The result name, as `expect` keys and headlines count it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Executed { .. } => "executed",
            Self::Failed { .. } => "failed",
            Self::Uncertain { .. } => "uncertain",
            Self::Blocked { .. } => "blocked",
            Self::AwaitingApproval => "awaiting-approval",
        }
    }
}

/// A unit's state in plan and status reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnitState {
    Blocked,
    Ready,
    AwaitingApproval,
    Running,
    Uncertain,
    Failed,
    Current,
    Held,
    PendingTeardown,
    TearingDown,
    Retained,
}

/// Whether a unit's latest receipt matches its desired document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    Current,
    Stale,
    None,
}

/// Why a unit is blocked at one pointer of its spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockedBy {
    pub pointer: String,
    /// The producer, when a reference waits for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<UnitName>,
    /// A stable reason, such as `no-current-receipt`.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_categories_combine_by_severity_not_by_code() {
        use ExitCategory::{Error, Failed, Incomplete, Ok, Uncertain};
        assert_eq!(Failed.most_severe(Incomplete), Failed);
        assert_eq!(Incomplete.most_severe(Error), Error);
        assert_eq!(Error.most_severe(Failed), Failed);
        assert_eq!(Failed.most_severe(Uncertain), Uncertain);
        assert_eq!(Ok.most_severe(Incomplete), Incomplete);
        assert_eq!(Uncertain.code(), 4);
    }
}
