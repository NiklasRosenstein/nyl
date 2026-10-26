//! The scenario file format of `design/reference-scenarios.md#scenario-files`.
//!
//! The harness knows every step kind and field the format documents,
//! including those of later milestones, and validates every scenario against
//! them at every milestone; an undocumented field or step kind fails, so the
//! files cannot drift from the format.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// A milestone of the roadmap, such as `M3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Milestone(pub u8);

impl<'de> Deserialize<'de> for Milestone {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.strip_prefix('M')
            .and_then(|number| number.parse().ok())
            .map(Milestone)
            .ok_or_else(|| serde::de::Error::custom(format!("milestone {text:?} must look like M3")))
    }
}

impl fmt::Display for Milestone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "M{}", self.0)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// The rules the scenario proves: a design file and heading anchor.
    pub contract: String,
    /// The first milestone whose harness runs it.
    pub milestone: Milestone,
    /// The tiers that run it; default both.
    #[serde(default = "default_tiers")]
    pub tiers: Vec<u8>,
    /// The project directory, relative to the scenario directory.
    #[serde(default = "default_project")]
    pub project: String,
    pub steps: Vec<Step>,
}

fn default_tiers() -> Vec<u8> {
    vec![1, 2]
}

fn default_project() -> String {
    "project".to_owned()
}

/// One step: exactly one kind, plus the modifiers that kind admits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    // Kinds.
    pub commit: Option<CommitStep>,
    pub branch: Option<BranchStep>,
    #[serde(rename = "squash-merge")]
    pub squash_merge: Option<SquashMergeStep>,
    #[serde(rename = "delete-branch")]
    pub delete_branch: Option<String>,
    pub clock: Option<ClockStep>,
    pub nyl: Option<Vec<String>>,
    pub driver: Option<DriverStep>,
    pub fault: Option<FaultStep>,
    pub resume: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<RefStep>,
    #[serde(rename = "ci-admit")]
    pub ci_admit: Option<CiAdmitStep>,
    #[serde(rename = "ci-worker")]
    pub ci_worker: Option<CiWorkerStep>,
    #[serde(rename = "ci-export")]
    pub ci_export: Option<CiExportStep>,

    // Modifiers.
    pub exit: Option<i32>,
    pub expect: Option<Expect>,
    pub id: Option<String>,
    pub pause: Option<PauseAt>,
    pub killed: Option<bool>,
    pub at: Option<String>,
    pub tier: Option<u8>,
}

/// The kind of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Commit,
    Branch,
    SquashMerge,
    DeleteBranch,
    Clock,
    Nyl,
    Driver,
    Fault,
    Resume,
    Ref,
    CiAdmit,
    CiWorker,
    CiExport,
}

impl StepKind {
    /// The first milestone whose harness executes this kind.
    pub fn executes_from(self) -> Milestone {
        match self {
            Self::CiAdmit | Self::CiWorker | Self::CiExport => Milestone(8),
            _ => Milestone(3),
        }
    }

    /// Steps that only tier 1 can run: they script fakes or the shell.
    pub fn tier_one_only(self) -> bool {
        matches!(self, Self::Driver | Self::Fault)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitStep {
    pub branch: String,
    pub message: String,
    /// A directory whose files are copied over the working clone.
    pub apply: Option<String>,
    /// Paths removed from the working clone.
    #[serde(default)]
    pub delete: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchStep {
    pub name: String,
    pub from: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SquashMergeStep {
    pub branch: String,
    pub into: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockStep {
    pub advance: Option<String>,
    pub set: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverStep {
    pub unit: String,
    pub operation: DriverOperation,
    pub outcome: DriverOutcome,
    pub category: Option<String>,
    pub retryable: Option<bool>,
    pub outputs: Option<Value>,
    pub times: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverOperation {
    Plan,
    Reconcile,
    Verify,
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverOutcome {
    Succeeded,
    Failed,
    Uncertain,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultStep {
    pub point: PointName,
    /// The unit the point concerns: required for `after-effect` and
    /// `before-checkpoint`; for `before-lease-update`, the unit whose phase
    /// the fault precedes, or none for the first phase; none for
    /// `before-push`.
    pub unit: Option<String>,
}

/// The points a `fault` step or a `pause` names. A `fault` step's
/// `executing` and `waiting-lease` parse so that validation can say they
/// only pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PointName {
    AfterEffect,
    BeforeCheckpoint,
    BeforeLeaseUpdate,
    BeforePush,
    Executing,
    WaitingLease,
}

impl PointName {
    /// The shell's fault point of this name.
    pub fn fault_point(self) -> nyl_orchestration::fault::FaultPoint {
        use nyl_orchestration::fault::FaultPoint;
        match self {
            Self::AfterEffect => FaultPoint::AfterEffect,
            Self::BeforeCheckpoint => FaultPoint::BeforeCheckpoint,
            Self::BeforeLeaseUpdate => FaultPoint::BeforeLeaseUpdate,
            Self::BeforePush => FaultPoint::BeforePush,
            Self::Executing => FaultPoint::Executing,
            Self::WaitingLease => FaultPoint::WaitingLease,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseAt {
    pub point: PointName,
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefStep {
    pub delete: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiAdmitStep {
    pub id: String,
    #[serde(default)]
    pub rollback: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiWorkerStep {
    pub admission: String,
    pub unit: Option<String>,
    #[serde(default)]
    pub leaving: bool,
    #[serde(rename = "allow-teardown", default)]
    pub allow_teardown: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiExportStep {
    pub operation: String,
    #[serde(rename = "fail-on-leaving", default)]
    pub fail_on_leaving: bool,
}

/// User-visible results a step checks. A key that is not given is not checked.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub executed: Option<Vec<String>>,
    pub current: Option<Vec<String>>,
    pub blocked: Option<Vec<String>>,
    pub failed: Option<Vec<String>>,
    pub uncertain: Option<Vec<String>>,
    #[serde(rename = "awaiting-approval")]
    pub awaiting_approval: Option<Vec<String>>,
    #[serde(rename = "pending-teardown")]
    pub pending_teardown: Option<Vec<String>>,
    #[serde(rename = "tearing-down")]
    pub tearing_down: Option<Vec<String>>,
    pub retained: Option<Vec<String>>,
    pub removed: Option<Vec<String>>,
    pub recovered: Option<Vec<String>>,
    pub imported: Option<Vec<String>>,
    pub commits: Option<CommitCounts>,
    pub units: Option<BTreeMap<String, UnitExpectation>>,
    pub codes: Option<Vec<String>>,
    pub get: Option<GetExpectation>,
    pub deploy: Option<DeployExpectation>,
    pub promotion: Option<PromotionExpectation>,
    pub ci: Option<CiExpectation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitCounts {
    pub desired: usize,
    pub observed: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitExpectation {
    /// A lifecycle state name, or the persisted lifecycle block.
    pub lifecycle: Option<Value>,
    /// The persisted condition, compared on the fields given, or `null`.
    #[serde(default, deserialize_with = "present")]
    pub condition: Option<Value>,
    pub receipt: Option<ReceiptExpectation>,
    pub uid: Option<UidExpectation>,
}

/// Distinguishes `condition: null` (checked to be absent) from a missing key.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptExpectation {
    Current,
    Stale,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UidExpectation {
    Same,
    New,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetExpectation {
    pub args: Vec<String>,
    pub value: Option<Value>,
    pub json: Option<BTreeMap<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployExpectation {
    pub branch: String,
    #[serde(default)]
    pub contains: Vec<String>,
    #[serde(default)]
    pub unchanged: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionExpectation {
    pub path: String,
    #[serde(rename = "sourceCommit")]
    pub source_commit: String,
    pub from: PromotionFrom,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionFrom {
    pub run: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiExpectation {
    pub admission: Option<String>,
    pub units: Option<Vec<String>>,
    #[serde(rename = "lifecycleLeaving")]
    pub lifecycle_leaving: Option<Vec<String>>,
    pub superseded: Option<Vec<String>>,
}

impl Step {
    /// The step's single kind, or why it has none or several.
    pub fn kind(&self) -> Result<StepKind, String> {
        let kinds = [
            (self.commit.is_some(), StepKind::Commit),
            (self.branch.is_some(), StepKind::Branch),
            (self.squash_merge.is_some(), StepKind::SquashMerge),
            (self.delete_branch.is_some(), StepKind::DeleteBranch),
            (self.clock.is_some(), StepKind::Clock),
            (self.nyl.is_some(), StepKind::Nyl),
            (self.driver.is_some(), StepKind::Driver),
            (self.fault.is_some(), StepKind::Fault),
            (self.resume.is_some(), StepKind::Resume),
            (self.reference.is_some(), StepKind::Ref),
            (self.ci_admit.is_some(), StepKind::CiAdmit),
            (self.ci_worker.is_some(), StepKind::CiWorker),
            (self.ci_export.is_some(), StepKind::CiExport),
        ];
        let set = kinds
            .iter()
            .filter(|(set, _)| *set)
            .map(|(_, kind)| *kind)
            .collect::<Vec<_>>();
        match set.as_slice() {
            [kind] => Ok(*kind),
            [] => Err("a step needs exactly one kind, such as nyl or commit; it has none".to_owned()),
            several => Err(format!("a step needs exactly one kind; it has {several:?}")),
        }
    }
}

/// Validate a parsed scenario against the format and its directory.
pub fn validate(scenario: &Scenario, directory: &Path, repository_root: &Path) -> Result<(), String> {
    validate_contract(&scenario.contract, repository_root)?;
    if scenario.tiers.is_empty() || scenario.tiers.iter().any(|tier| !matches!(tier, 1 | 2)) {
        return Err(format!("tiers {:?} must name tiers 1 and/or 2", scenario.tiers));
    }
    let project = directory.join(&scenario.project);
    if !project.join("nyl").is_dir() && !project.join("nyl.toml").is_file() {
        return Err(format!("project {} has no nyl.toml", project.display()));
    }
    let mut ids = BTreeMap::<String, bool>::new();
    let mut scripted = false;
    for (index, step) in scenario.steps.iter().enumerate() {
        validate_step(step, directory, &mut ids).map_err(|error| format!("step {}: {error}", index + 1))?;
        // driver, fault, and pause steps are tier 1 by kind, and a step that
        // depends on one carries `tier: 1`. A scenario that also runs in tier 2
        // marks every checked step after the first such step, since the
        // harness cannot tell which ones depend on it.
        let kind = step.kind()?;
        if scenario.tiers.contains(&2)
            && scripted
            && step.tier.is_none()
            && matches!(kind, StepKind::Nyl | StepKind::Resume)
        {
            return Err(format!(
                "step {}: follows a tier-1-only step in a scenario that runs in tier 2; mark it tier: 1 or set tiers: [1]",
                index + 1
            ));
        }
        scripted |= kind.tier_one_only() || step.pause.is_some();
    }
    // `ids` maps each id to whether its paused invocation still awaits a resume.
    if let Some((id, _)) = ids.iter().find(|(_, awaiting)| **awaiting) {
        return Err(format!("{id} is paused and never resumed"));
    }
    Ok(())
}

fn validate_contract(contract: &str, repository_root: &Path) -> Result<(), String> {
    let (file, anchor) = contract
        .split_once('#')
        .ok_or_else(|| format!("contract {contract:?} must name a file and a heading anchor"))?;
    let text = std::fs::read_to_string(repository_root.join(file))
        .map_err(|error| format!("contract file {file}: {error}"))?;
    let anchors = text
        .lines()
        .filter_map(|line| line.strip_prefix('#'))
        .map(|heading| github_anchor(heading.trim_start_matches('#').trim()))
        .collect::<Vec<_>>();
    if anchors.iter().any(|candidate| candidate == anchor) {
        Ok(())
    } else {
        Err(format!(
            "contract {contract}: {file} has no heading with anchor #{anchor}"
        ))
    }
}

/// The anchor GitHub generates for a Markdown heading.
fn github_anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric() || matches!(character, ' ' | '-' | '_'))
        .map(|character| if character == ' ' { '-' } else { character })
        .collect()
}

fn validate_step(step: &Step, directory: &Path, ids: &mut BTreeMap<String, bool>) -> Result<(), String> {
    let kind = step.kind()?;
    let invocation = matches!(kind, StepKind::Nyl | StepKind::CiAdmit | StepKind::CiExport);
    let checked = invocation || matches!(kind, StepKind::Resume | StepKind::CiWorker);
    if !checked && (step.exit.is_some() || step.expect.is_some() || step.killed.is_some()) {
        return Err(format!("{kind:?} steps take no exit, expect, or killed"));
    }
    if step.at.is_some() && !invocation {
        return Err(format!("{kind:?} steps take no at"));
    }
    if (step.id.is_some() || step.pause.is_some()) && kind != StepKind::Nyl {
        return Err("only nyl steps take id and pause".to_owned());
    }
    if step.pause.is_some() && step.id.is_none() {
        return Err("a paused nyl step needs an id to resume it".to_owned());
    }
    if step.pause.is_some() && (step.exit.is_some() || step.expect.is_some()) {
        return Err("a paused nyl step is checked by its resume step".to_owned());
    }
    if checked && step.pause.is_none() && step.exit.is_none() && step.killed != Some(true) {
        return Err(format!("{kind:?} steps need an exit, or killed: true after a fault"));
    }
    if step.killed.is_some() && step.exit.is_some() {
        return Err("a killed step has no exit".to_owned());
    }
    if let Some(tier) = step.tier {
        if tier != 1 {
            return Err(format!("tier {tier}: only tier 1 can be required of a single step"));
        }
    }
    match kind {
        StepKind::Commit => {
            let commit = step.commit.as_ref().expect("kind");
            if let Some(apply) = &commit.apply {
                let path: PathBuf = directory.join(apply);
                if !path.is_dir() {
                    return Err(format!("commit apply {} is not a directory", path.display()));
                }
            }
            if commit.apply.is_none() && commit.delete.is_empty() {
                return Err("a commit step applies or deletes something".to_owned());
            }
        }
        StepKind::Clock => {
            let clock = step.clock.as_ref().expect("kind");
            if clock.advance.is_some() == clock.set.is_some() {
                return Err("clock takes exactly one of advance and set".to_owned());
            }
        }
        StepKind::Nyl => {
            if step.nyl.as_ref().expect("kind").is_empty() {
                return Err("nyl needs arguments".to_owned());
            }
            if let Some(id) = &step.id {
                if ids.insert(id.clone(), step.pause.is_some()).is_some() {
                    return Err(format!("id {id} is used twice"));
                }
            }
        }
        StepKind::Resume => {
            let id = step.resume.as_ref().expect("kind");
            match ids.get_mut(id) {
                Some(awaiting @ true) => *awaiting = false,
                Some(false) => return Err(format!("resume {id}: that step was not paused or is already resumed")),
                None => return Err(format!("resume {id}: no earlier nyl step has that id")),
            }
        }
        StepKind::Driver => {
            let driver = step.driver.as_ref().expect("kind");
            if (driver.outcome == DriverOutcome::Failed) != (driver.category.is_some() && driver.retryable.is_some()) {
                return Err("a failed driver outcome, and only that, names its category and retryable".to_owned());
            }
            if driver.times == Some(0) {
                return Err("times must be at least 1".to_owned());
            }
        }
        StepKind::Fault => {
            let fault = step.fault.as_ref().expect("kind");
            match (fault.point, &fault.unit) {
                (PointName::Executing | PointName::WaitingLease, _) => {
                    return Err(format!(
                        "fault {:?}: executing and waiting-lease only pause; use a nyl step's pause",
                        fault.point
                    ));
                }
                (PointName::AfterEffect | PointName::BeforeCheckpoint, None) => {
                    return Err(format!("fault {:?} needs the unit it concerns", fault.point));
                }
                (PointName::BeforePush, Some(_)) => {
                    return Err("fault before-push concerns the whole run; it takes no unit".to_owned());
                }
                _ => {}
            }
        }
        StepKind::CiWorker => {
            let worker = step.ci_worker.as_ref().expect("kind");
            if worker.unit.is_some() == worker.leaving {
                return Err("ci-worker takes exactly one of unit and leaving".to_owned());
            }
            if worker.allow_teardown && !worker.leaving {
                return Err("allow-teardown applies to the leaving job".to_owned());
            }
        }
        _ => {}
    }
    Ok(())
}
