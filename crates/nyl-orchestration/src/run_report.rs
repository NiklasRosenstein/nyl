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

use nyl_core::orchestration::codes::{Code, Finding};
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

impl ExitCategory {
    /// The category an error finding contributes: a held lease is
    /// incomplete (2), a lost lease uncertain (4), anything else an error (1).
    pub fn of_finding(finding: &Finding) -> Self {
        match finding.code {
            Code::LeaseHeld => Self::Incomplete,
            Code::LeaseLost => Self::Uncertain,
            _ => Self::Error,
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

    /// Set `exit` and `complete` from the units, errors, and the report kind,
    /// combining with [`ExitCategory::most_severe`]:
    /// - each error finding contributes [`ExitCategory::of_finding`];
    /// - plan: 2 when a unit's state is blocked or awaiting approval, 3 only
    ///   when a driver plan failed in this invocation
    ///   ([`UnitReport::plan_error`]); a stored failed condition that will be
    ///   retried does not raise the exit;
    /// - reconcile: by each unit's `result` (failed 3, uncertain 4, blocked
    ///   or awaiting approval 2);
    /// - status: by each unit's `state` (uncertain 4, failed 3, and 2 for
    ///   every other state but current: blocked, ready, running, held,
    ///   awaiting approval, pending teardown, tearing down);
    /// - `complete` is true exactly when the exit is 0.
    pub fn finish(&mut self, kind: ReportKind) {
        let errors = self.errors.iter().map(ExitCategory::of_finding);
        let units = self.units.values().map(|unit| unit_exit(kind, unit));
        self.exit = errors.chain(units).fold(ExitCategory::Ok, ExitCategory::most_severe);
        self.complete = self.exit == ExitCategory::Ok;
    }

    /// The transition commit summary of a reconcile: the headline (such as
    /// `2 executed, 1 failed, 1 blocked`) and each unit's [`UnitResult`] in
    /// its flow form, such as `{"result":"failed","category":"tool-error","retryable":true}`.
    pub fn transition_summary(&self, operation: Operation) -> TransitionSummary {
        let results: Vec<(&UnitName, &UnitResult)> = self
            .units
            .iter()
            .filter_map(|(name, unit)| unit.result.as_ref().map(|result| (name, result)))
            .collect();
        let headline = HEADLINE_RESULTS
            .iter()
            .filter_map(|counted| {
                let count = results.iter().filter(|(_, result)| result.name() == *counted).count();
                (count > 0).then(|| format!("{count} {counted}"))
            })
            .collect::<Vec<_>>()
            .join(", ");
        TransitionSummary {
            operation,
            environment: self.name.clone(),
            headline,
            units: results
                .into_iter()
                .map(|(name, result)| {
                    (
                        name.clone(),
                        serde_json::to_value(result).expect("unit results serialize"),
                    )
                })
                .collect(),
            recovered: Vec::new(),
            imported: Vec::new(),
            lease_broken: None,
            details: BTreeMap::new(),
        }
    }
}

/// The results a transition commit's headline counts, in order.
const HEADLINE_RESULTS: [&str; 5] = ["executed", "failed", "uncertain", "blocked", "awaiting-approval"];

/// The exit category one unit contributes to a report of `kind`.
fn unit_exit(kind: ReportKind, unit: &UnitReport) -> ExitCategory {
    match kind {
        ReportKind::PlanReport => {
            if unit.plan_error.is_some() {
                ExitCategory::Failed
            } else if matches!(unit.state, Some(UnitState::Blocked | UnitState::AwaitingApproval))
                || !unit.blocked_by.is_empty()
            {
                ExitCategory::Incomplete
            } else {
                ExitCategory::Ok
            }
        }
        ReportKind::ReconcileReport => match unit.result {
            Some(UnitResult::Failed { .. }) => ExitCategory::Failed,
            Some(UnitResult::Uncertain { .. }) => ExitCategory::Uncertain,
            Some(UnitResult::Blocked { .. } | UnitResult::AwaitingApproval) => ExitCategory::Incomplete,
            Some(UnitResult::Current | UnitResult::Executed { .. }) | None => ExitCategory::Ok,
        },
        ReportKind::StatusReport => match unit.state {
            Some(UnitState::Uncertain) => ExitCategory::Uncertain,
            Some(UnitState::Failed) => ExitCategory::Failed,
            // A retained unit's deletion completed: its resources stay by policy.
            Some(UnitState::Current | UnitState::Retained) | None => ExitCategory::Ok,
            Some(_) => ExitCategory::Incomplete,
        },
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
    /// What its deletion will do, by its effective deletion policy.
    pub consequence: LeavingConsequence,
    /// For a `deselected` unit, the `unitSelector` labels its literal labels
    /// no longer match, so a selector typo reads as one.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// What happens to a unit leaving the ownership set, the groups of the plan's
/// leaving section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeavingConsequence {
    /// `deletionPolicy: Teardown`: its resources are torn down, which needs
    /// `--allow-teardown`.
    Teardown,
    /// Its kind cannot tear down and it declares no policy: it is dropped
    /// from state with its resources kept, and returns as a new incarnation.
    Dropped,
    /// `deletionPolicy: Retain` declared: it stays as a `retained` tombstone
    /// with its resources kept.
    Retained,
}

impl LeavingConsequence {
    pub const ALL: [Self; 3] = [Self::Teardown, Self::Dropped, Self::Retained];

    /// The heading of its group in the plan's leaving section.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Teardown => "teardown, needs --allow-teardown",
            Self::Dropped => "dropped from state, resources kept",
            Self::Retained => "retained as tombstones",
        }
    }
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
    /// The driver's plan failed in this invocation (plan reports only); this,
    /// not a stored condition, makes a plan exit 3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_error: Option<String>,
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
            plan_error: None,
            message: None,
            last_run: None,
            warnings: Vec::new(),
        }
    }
}

impl UnitReport {
    /// The name `expect` keys, tables, and headlines use: the `result` of a
    /// reconcile report, else the `state` of a plan or status report.
    pub fn outcome_name(&self) -> Option<&'static str> {
        self.result
            .as_ref()
            .map(UnitResult::name)
            .or_else(|| self.state.map(UnitState::name))
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

impl UnitState {
    /// The kebab-case name, as reports serialize it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Ready => "ready",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Running => "running",
            Self::Uncertain => "uncertain",
            Self::Failed => "failed",
            Self::Current => "current",
            Self::Held => "held",
            Self::PendingTeardown => "pending-teardown",
            Self::TearingDown => "tearing-down",
            Self::Retained => "retained",
        }
    }
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

    #[test]
    fn test_lease_findings_keep_their_exit_categories() {
        let finding = |code| Finding::new(code, "m", "f");
        assert_eq!(
            ExitCategory::of_finding(&finding(Code::LeaseHeld)),
            ExitCategory::Incomplete
        );
        assert_eq!(
            ExitCategory::of_finding(&finding(Code::LeaseLost)),
            ExitCategory::Uncertain
        );
        assert_eq!(
            ExitCategory::of_finding(&finding(Code::UnitUnknown)),
            ExitCategory::Error
        );
    }

    fn unit_name(name: &str) -> UnitName {
        UnitName::new(name).unwrap()
    }

    fn report(units: Vec<(&str, UnitReport)>) -> EnvironmentRunReport {
        let mut report = EnvironmentRunReport::new(EnvironmentName::new("dev").unwrap());
        report.units = units.into_iter().map(|(name, unit)| (unit_name(name), unit)).collect();
        report
    }

    fn with_state(state: UnitState) -> UnitReport {
        UnitReport {
            state: Some(state),
            ..UnitReport::new("Command")
        }
    }

    fn with_result(result: UnitResult) -> UnitReport {
        UnitReport {
            result: Some(result),
            ..UnitReport::new("Command")
        }
    }

    fn failed() -> UnitResult {
        UnitResult::Failed {
            category: FailureCategory::ToolError,
            retryable: true,
        }
    }

    /// Catches a plan exiting 3 for a stored failure it will retry (CI would
    /// fail a plan that is fine) or 0 while a unit cannot be planned.
    #[test]
    fn test_plan_exit_is_3_only_for_a_plan_error() {
        // The shell reports a unit with a stored failed-retryable condition as ready.
        let mut plan = report(vec![
            ("web", with_state(UnitState::Ready)),
            ("database", with_state(UnitState::Blocked)),
        ]);
        plan.finish(ReportKind::PlanReport);
        assert_eq!((plan.exit, plan.complete), (ExitCategory::Incomplete, false));

        let mut waiting = UnitReport::new("Command");
        waiting.blocked_by.push(BlockedBy {
            pointer: "/values/vpc".to_owned(),
            unit: Some(unit_name("network")),
            reason: "no-current-receipt".to_owned(),
        });
        let mut plan = report(vec![("database", waiting)]);
        plan.finish(ReportKind::PlanReport);
        assert_eq!(plan.exit, ExitCategory::Incomplete);

        let mut broken = with_state(UnitState::Ready);
        broken.plan_error = Some("tofu plan exited 1".to_owned());
        let mut plan = report(vec![("network", broken), ("database", with_state(UnitState::Blocked))]);
        plan.finish(ReportKind::PlanReport);
        assert_eq!(plan.exit, ExitCategory::Failed);

        let mut plan = report(vec![("network", with_state(UnitState::Current))]);
        plan.finish(ReportKind::PlanReport);
        assert_eq!((plan.exit, plan.complete), (ExitCategory::Ok, true));
    }

    /// Catches a reconcile hiding an uncertain execution behind a failure,
    /// or an error behind a blocked unit.
    #[test]
    fn test_reconcile_exit_follows_results_by_severity() {
        let mut reconcile = report(vec![
            ("a", with_result(UnitResult::Current)),
            ("b", with_result(UnitResult::Blocked { reason: "x".to_owned() })),
        ]);
        reconcile.finish(ReportKind::ReconcileReport);
        assert_eq!(reconcile.exit, ExitCategory::Incomplete);
        reconcile.errors.push(Finding::new(Code::DependencyCycle, "m", "f"));
        reconcile.finish(ReportKind::ReconcileReport);
        assert_eq!(reconcile.exit, ExitCategory::Error);
        reconcile.units.insert(unit_name("c"), with_result(failed()));
        reconcile.finish(ReportKind::ReconcileReport);
        assert_eq!(reconcile.exit, ExitCategory::Failed);
        reconcile.units.insert(
            unit_name("d"),
            with_result(UnitResult::Uncertain {
                reason: "lost".to_owned(),
            }),
        );
        reconcile.finish(ReportKind::ReconcileReport);
        assert_eq!(reconcile.exit, ExitCategory::Uncertain);
    }

    /// Catches `status` exiting 0 while a unit still waits to run, which
    /// would let CI treat an unreconciled environment as done.
    #[test]
    fn test_status_exit_is_0_only_when_every_unit_is_current() {
        let mut status = report(vec![("a", with_state(UnitState::Current))]);
        status.finish(ReportKind::StatusReport);
        assert_eq!((status.exit, status.complete), (ExitCategory::Ok, true));
        for state in [
            UnitState::Ready,
            UnitState::Running,
            UnitState::Held,
            UnitState::Blocked,
        ] {
            let mut status = report(vec![("a", with_state(UnitState::Current)), ("b", with_state(state))]);
            status.finish(ReportKind::StatusReport);
            assert_eq!(status.exit, ExitCategory::Incomplete, "{state:?}");
        }
        let mut status = report(vec![
            ("a", with_state(UnitState::Failed)),
            ("b", with_state(UnitState::Uncertain)),
        ]);
        status.finish(ReportKind::StatusReport);
        assert_eq!(status.exit, ExitCategory::Uncertain);
    }

    /// Catches a transition commit whose summary disagrees with the report
    /// or cannot be parsed back from the commit message.
    #[test]
    fn test_transition_summary_counts_results_and_parses_as_yaml() {
        let report = report(vec![
            ("a", with_result(UnitResult::Current)),
            (
                "b",
                with_result(UnitResult::Executed {
                    execution_key: ExecutionKey::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
                }),
            ),
            (
                "c",
                with_result(UnitResult::Executed {
                    execution_key: ExecutionKey::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
                }),
            ),
            ("d", with_result(failed())),
            (
                "e",
                with_result(UnitResult::Blocked {
                    reason: "a has no current receipt".to_owned(),
                }),
            ),
            ("f", with_state(UnitState::Ready)),
        ]);
        let summary = report.transition_summary(Operation::Reconcile);
        assert_eq!(summary.headline, "2 executed, 1 failed, 1 blocked");
        assert_eq!(summary.units.len(), 5);
        let message = crate::transition::message(
            &summary,
            &crate::transition::Trailers {
                run: RunId::new("00000000-0000-7000-8000-000000000001").unwrap(),
                source_commit: None,
                read_desired: None,
                read_observed: None,
                runner: "ci".to_owned(),
                evaluated_at: "2026-01-01T00:00:00Z".parse().unwrap(),
                version: "0.0.0".to_owned(),
                requested_by: None,
                reason: None,
                local: false,
            },
        );
        let body = message.split("\n\n").nth(1).unwrap();
        let parsed: serde_json::Value = serde_saphyr::from_str(body).unwrap();
        assert_eq!(
            parsed["units"]["d"],
            serde_json::json!({"result": "failed", "category": "tool-error", "retryable": true})
        );
        assert_eq!(parsed["units"]["a"], serde_json::json!({"result": "current"}));
        assert_eq!(
            parsed["units"]["b"]["executionKey"],
            serde_json::json!(format!("sha256:{}", "a".repeat(64)))
        );
    }

    #[test]
    fn test_unit_state_names_match_their_serialized_form() {
        for state in [
            UnitState::Blocked,
            UnitState::Ready,
            UnitState::AwaitingApproval,
            UnitState::Running,
            UnitState::Uncertain,
            UnitState::Failed,
            UnitState::Current,
            UnitState::Held,
            UnitState::PendingTeardown,
            UnitState::TearingDown,
            UnitState::Retained,
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), state.name());
        }
    }
}
