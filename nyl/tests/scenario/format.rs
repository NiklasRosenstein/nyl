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
    /// The publication drivers tier 1 runs it with; default both.
    #[serde(default)]
    pub publication: PublicationDrivers,
    pub steps: Vec<Step>,
}

/// Which `KubernetesPublication` drivers tier 1 runs a scenario with: the
/// test kind that stands in for it and the built-in driver, or the built-in
/// driver only, for a scenario that depends on what that driver derives from
/// its render, which the test kind states in its declaration instead, or the
/// test kind only, for a scenario that scripts the publication's reconcile
/// outcome, which the built-in driver, running for real, cannot be made to give.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicationDrivers {
    #[default]
    Both,
    Real,
    Fake,
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
    pub merge: Option<MergeStep>,
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
    pub file: Option<FileStep>,
    pub argocd: Option<ArgocdStep>,

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
    Merge,
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
    File,
    Argocd,
}

impl StepKind {
    /// The first milestone whose harness executes this kind.
    pub fn executes_from(self) -> Milestone {
        match self {
            Self::CiAdmit | Self::CiWorker | Self::CiExport => Milestone(8),
            Self::Merge | Self::Argocd => Milestone(6),
            _ => Milestone(3),
        }
    }

    /// Steps that only tier 1 can run: they script fakes or the shell.
    pub fn tier_one_only(self) -> bool {
        matches!(self, Self::Driver | Self::Fault | Self::Argocd)
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

/// A change made outside Nyl to a file in the scenario's temporary
/// directory, `%SCENARIO_TMP%`, such as the resources a Command unit's
/// `detectDrift` step inspects: `content` writes the file, `delete: true` removes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileStep {
    /// Relative to `%SCENARIO_TMP%`, and never leaving it.
    pub path: String,
    pub content: Option<String>,
    #[serde(default)]
    pub delete: bool,
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

/// A merge commit of `branch` into `into` (`git merge --no-ff`), keeping the
/// branch, so every commit of `branch` becomes reachable from `into`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeStep {
    pub branch: String,
    pub into: String,
}

/// Tier 1: what the fake Argo CD reports for a target's generated
/// Applications from now on, read by the fake publication's `attest`
/// through the shared matching of a reading to a publication.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgocdStep {
    /// The DeploymentTarget whose Applications are reported.
    pub target: String,
    /// What every Application of the target, the catalog Application
    /// included, reports unless `releases` names it.
    pub all: ApplicationReading,
    /// Per Release (`<group>/<release>`), what its Application reports.
    #[serde(default)]
    pub releases: BTreeMap<String, ApplicationReading>,
}

/// One Application in an `argocd` step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationReading {
    /// The `id` of the earlier `nyl` step whose run published what the
    /// Application runs (the publication commit that run's receipt for the
    /// target records), or `unmatched` for a revision no publication matches.
    pub runs: String,
    /// Argo CD's live health; default `Healthy`.
    #[serde(default)]
    pub health: ApplicationHealth,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ApplicationHealth {
    #[default]
    Healthy,
    Progressing,
    Degraded,
    Unknown,
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
#[serde(rename_all = "kebab-case")]
pub enum DriverOperation {
    Plan,
    Reconcile,
    DetectDrift,
    Teardown,
    /// Reads the unit's observed values; `outputs` holds the values it
    /// reports. Executes from M5.
    Observe,
    /// Checks that an artifact of the unit's receipt still exists, as
    /// `nyl promote` asks the producing kind's driver before recording:
    /// `succeeded` (it exists) or `failed` (it is gone). Executes from M6.
    VerifyArtifact,
}

impl DriverOperation {
    /// The first milestone whose harness executes this operation.
    pub fn executes_from(self) -> Milestone {
        match self {
            Self::Observe => Milestone(5),
            Self::VerifyArtifact => Milestone(6),
            _ => Milestone(3),
        }
    }

    /// Operations whose `failed` outcome is a finding, not a failed
    /// execution, so it names no failure category.
    fn uncategorized(self) -> bool {
        matches!(self, Self::Observe | Self::VerifyArtifact)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverOutcome {
    Succeeded,
    Failed,
    Uncertain,
    /// `detect-drift` only: the fake reports drift.
    Drifted,
    /// `observe` only: the observations are not available yet.
    Pending,
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
    /// Delete this ref in the state repository.
    pub delete: Option<String>,
    /// Point this ref of `project.git` at the tip of the branch `to`, as a host
    /// that keeps a pull request's head under `refs/pull/<n>/head` does.
    pub set: Option<String>,
    pub to: Option<String>,
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
    /// A plan or status report's `ready` units: work for the next reconcile.
    pub ready: Option<Vec<String>>,
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
    pub released: Option<Vec<String>>,
    pub held: Option<Vec<String>>,
    pub drifted: Option<Vec<String>>,
    pub recovered: Option<Vec<String>>,
    pub imported: Option<Vec<String>>,
    /// The summary's `driftRepaired`, and the report's list of that name.
    #[serde(rename = "driftRepaired")]
    pub drift_repaired: Option<Vec<String>>,
    /// The units of the summary's `observationsChanged`, and the report's.
    #[serde(rename = "observationsChanged")]
    pub observations_changed: Option<Vec<String>>,
    /// The units of the summary's `reapplied`, and the report's list.
    pub reapplied: Option<Vec<String>>,
    /// What `reconcile --detect-drift` found, per unit (`clean`, `drifted`,
    /// `unknown`, or `unsupported`): exactly these units carry a `driftCheck`
    /// in the report and in the transition commit summary.
    #[serde(rename = "drift-checks")]
    pub drift_checks: Option<BTreeMap<String, String>>,
    /// Whether the transition commit records a lease break (`leaseBroken`).
    #[serde(rename = "lease-broken")]
    pub lease_broken: Option<bool>,
    /// How a state operation, such as `hold` or `resume`, ended.
    pub outcome: Option<OperationOutcome>,
    pub commits: Option<CommitCounts>,
    pub units: Option<BTreeMap<String, UnitExpectation>>,
    pub codes: Option<Vec<String>>,
    /// Text the invocation printed, on stdout or stderr: each entry is a substring.
    pub messages: Option<Vec<String>>,
    pub get: Option<GetExpectation>,
    pub deploy: Option<DeployExpectation>,
    /// The commit a reconcile ran its first environment at, as a Git revision
    /// of `project.git`, such as `pr-124`.
    pub source: Option<String>,
    /// How the run's environment record says its source was chosen
    /// (`sourceChosenBy`), compared on the fields given, such as `{by: recorded}`.
    #[serde(rename = "source-chosen-by")]
    pub source_chosen_by: Option<Value>,
    /// The keep refs of preview instances' source commits in `project.git`.
    #[serde(rename = "keep-refs")]
    pub keep_refs: Option<KeepRefsExpectation>,
    pub promotion: Option<PromotionExpectation>,
    pub ci: Option<CiExpectation>,
}

/// A StateOperationReport's `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationOutcome {
    Done,
    Unchanged,
    Refused,
}

impl OperationOutcome {
    pub fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Unchanged => "unchanged",
            Self::Refused => "refused",
        }
    }
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
    /// The condition `nyl status` shows next to the unit's state, such as a
    /// held unit that is still `uncertain`; `null` checks that it shows none.
    #[serde(rename = "condition-state", default, deserialize_with = "present")]
    pub condition_state: Option<Value>,
    /// The latest drift observation `nyl status` shows for the unit.
    pub drift: Option<DriftExpectation>,
}

/// A unit's drift attribute in `nyl status`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftExpectation {
    pub result: DriftResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriftResult {
    Clean,
    Drifted,
    Unknown,
}

impl DriftResult {
    pub fn name(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Drifted => "drifted",
            Self::Unknown => "unknown",
        }
    }
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
    /// No uid: a held marker, or a unit held without an incarnation.
    None,
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
    /// Patterns that match no file on the branch after the step.
    #[serde(default)]
    pub absent: Vec<String>,
    /// Per file pattern, what the files it matches mention: a literal text,
    /// or the arguments of a `nyl get` invocation whose value they carry. A
    /// value must appear in at least one matching file.
    #[serde(default)]
    pub mentions: BTreeMap<String, Vec<Mention>>,
}

/// Something deployed files mention.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Mention {
    /// A literal text.
    Text(String),
    /// The arguments of a `nyl get` invocation, such as `[get, output, database/connection, -e, dev, --pointer, /host]`.
    Get(Vec<String>),
}

/// The keep refs (`refs/nyl-keep/<id>/<commit>`) in `project.git` after a step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepRefsExpectation {
    /// How many keep refs exist.
    pub count: usize,
    /// Revisions of `project.git`, such as `pr-124`, whose commit has a keep ref.
    #[serde(default)]
    pub kept: Vec<String>,
}

/// The PromotionRecord a `promote` step wrote, read where the path keeps it
/// (`record`), or, for `--dry-run`, the record its report shows it would
/// write. A key that is not given is not checked.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionExpectation {
    pub path: String,
    /// Where the record was written: `state` (default), the target
    /// environment's `desired/promotions/<path>.yaml`, or `source`, the
    /// Environment's `source.fromPromotion.promoted` block on the branch the
    /// step ran at.
    #[serde(default)]
    pub record: RecordLocation,
    pub sequence: Option<u64>,
    /// The promoted source commit as a Git revision of `project.git`, or
    /// `null` for a path that promotes only values.
    #[serde(rename = "sourceCommit", default, deserialize_with = "present")]
    pub source_commit: Option<Value>,
    pub from: PromotionFrom,
    /// Exactly these values, each equal to `equals`; `state` names the step
    /// whose run wrote the state a value-only path took the value from, when
    /// it differs from `from`.
    pub values: Option<BTreeMap<String, PromotedValueExpectation>>,
    pub evidence: Option<EvidenceExpectation>,
    /// Exactly these superseded commits, as revisions of `project.git`.
    pub superseded: Option<Vec<SupersededExpectation>>,
    #[serde(rename = "artifactsVerified")]
    pub artifacts_verified: Option<bool>,
    pub requested: Option<RequestedExpectation>,
    /// Exactly these entries of `ignoredAttestations`.
    #[serde(rename = "ignoredAttestations")]
    pub ignored_attestations: Option<Vec<IgnoredAttestationExpectation>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordLocation {
    #[default]
    State,
    Source,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionFrom {
    /// The source environment; default the path's `from`.
    pub environment: Option<String>,
    /// The `id` of the `nyl` step whose run recorded the promoted state.
    pub run: String,
    /// The record's `from.desiredCommit`, usually `%STATE(<id>)%`.
    #[serde(rename = "desiredCommit")]
    pub desired_commit: Option<String>,
    /// The record's `from.observedCommit`, usually `%OBSERVED(<id>)%`.
    #[serde(rename = "observedCommit")]
    pub observed_commit: Option<String>,
}

/// The record's `requested` block, compared on the fields given.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedExpectation {
    pub reason: String,
}

/// One entry of the record's `ignoredAttestations`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoredAttestationExpectation {
    /// `<unit>/<name>` or `environment/<name>`.
    pub attestation: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotedValueExpectation {
    /// The value: a literal, or the arguments of a `nyl get` invocation
    /// whose value it must equal.
    pub equals: Mention,
    /// The `id` of the step whose run wrote the value's own state.
    pub state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceExpectation {
    pub level: EvidenceLevel,
    /// Whether the level was reached through an override (`--evidence published`).
    #[serde(rename = "override")]
    pub overridden: Option<bool>,
    /// Exactly the attestations used, as `<unit>/<name>` or `environment/<name>`.
    pub attestations: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceLevel {
    Published,
    Attested,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupersededExpectation {
    /// The repository the commit is in, when it is checked.
    pub repository: Option<String>,
    /// A revision of `project.git`, such as `release/prod~1`.
    pub commit: String,
    pub reason: String,
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
            (self.merge.is_some(), StepKind::Merge),
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
            (self.file.is_some(), StepKind::File),
            (self.argocd.is_some(), StepKind::Argocd),
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
    // A `plan` script waiting for the next `nyl` step would be consumed by the
    // `plan` that resolves a `%PLAN-DIGEST%`, not by the step it is meant for.
    let mut plan_scripted = false;
    for (index, step) in scenario.steps.iter().enumerate() {
        if plan_scripted
            && substituted_arguments(step).into_iter().any(|argument| {
                placeholders(argument)
                    .is_ok_and(|found| found.iter().any(|p| matches!(p, Placeholder::PlanDigest { .. })))
            })
        {
            return Err(format!(
                "step {}: uses %PLAN-DIGEST%, whose `plan` would consume the `driver: {{operation: plan}}` script before it",
                index + 1
            ));
        }
        if step.kind()? == StepKind::Nyl {
            plan_scripted = false;
        }
        if step
            .driver
            .as_ref()
            .is_some_and(|driver| driver.operation == DriverOperation::Plan)
        {
            plan_scripted = true;
        }
        validate_placeholders(step, scenario.milestone, &ids)
            .map_err(|error| format!("step {}: {error}", index + 1))?;
        validate_step(step, directory, &mut ids).map_err(|error| format!("step {}: {error}", index + 1))?;
        let kind = step.kind()?;
        if kind.executes_from() > scenario.milestone {
            return Err(format!(
                "step {}: {kind:?} runs from {}, after the scenario's milestone",
                index + 1,
                kind.executes_from()
            ));
        }
        if let Some(driver) = &step.driver {
            if driver.operation.executes_from() > scenario.milestone {
                return Err(format!(
                    "step {}: {:?} runs from {}, after the scenario's milestone",
                    index + 1,
                    driver.operation,
                    driver.operation.executes_from()
                ));
            }
        }
        // driver, fault, argocd, and pause steps are tier 1 by kind, and a
        // step that depends on one carries `tier: 1`. A scenario that also
        // runs in tier 2 marks every checked step after the first such step,
        // since the harness cannot tell which ones depend on it.
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
    // Only the test kind reads the scripted Argo CD in tier 1.
    if scenario.publication != PublicationDrivers::Fake && scenario.steps.iter().any(|step| step.argocd.is_some()) {
        return Err(
            "a scenario with argocd steps runs with publication: fake, the only driver that reads them".to_owned(),
        );
    }
    // `ids` maps each id to whether its paused invocation still awaits a resume.
    if let Some((id, _)) = ids.iter().find(|(_, awaiting)| **awaiting) {
        return Err(format!("{id} is paused and never resumed"));
    }
    Ok(())
}

/// Whether a `nyl` step's arguments parse with the real command line, as
/// the runner invokes it (with the clone's path appended), so a misspelt or
/// misused option, such as `--reapply` given a unit instead of `--unit`,
/// fails before the scenario runs. Only scenarios that run can be checked:
/// a pending one may use options its milestone has not added yet.
pub fn validate_arguments(args: &[String]) -> Result<(), String> {
    use clap::Parser;
    let argv = std::iter::once("nyl")
        .chain(args.iter().map(String::as_str))
        .chain(std::iter::once("."));
    nyl::cli::Cli::try_parse_from(argv)
        .map(drop)
        .map_err(|error| format!("nyl {}: {error}", args.join(" ")))
}

/// An argument placeholder, which the runner replaces before the step runs
/// (`design/reference-scenarios.md#argument-placeholders`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placeholder {
    /// `%STATE(<id>)%`: the desired-state commit the `nyl` step with that id
    /// wrote (its observed-state commit when it wrote no desired one).
    State(String),
    /// `%OBSERVED(<id>)%`: the observed-state commit the `nyl` step with that
    /// id wrote.
    Observed(String),
    /// `%COMMIT(<revision>)%`: the full commit ID of a revision of `project.git`.
    Commit(String),
    /// `%PLAN-DIGEST(<environment>/<unit>)%`: the `changeDigest` that
    /// `nyl plan -e <environment> --unit <unit>` reports at the step's checkout.
    PlanDigest { environment: String, unit: String },
}

impl Placeholder {
    /// The text a scenario writes for it, which the runner replaces.
    pub fn token(&self) -> String {
        match self {
            Self::State(id) => format!("%STATE({id})%"),
            Self::Observed(id) => format!("%OBSERVED({id})%"),
            Self::Commit(revision) => format!("%COMMIT({revision})%"),
            Self::PlanDigest { environment, unit } => format!("%PLAN-DIGEST({environment}/{unit})%"),
        }
    }
}

/// The first milestone whose harness replaces argument placeholders.
pub const PLACEHOLDERS_FROM: Milestone = Milestone(6);

/// Every placeholder in `text`: `%NAME(argument)%` with an upper-case name.
pub fn placeholders(text: &str) -> Result<Vec<Placeholder>, String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        let after = &rest[start + 1..];
        let name_len = after
            .find(|character: char| !(character.is_ascii_uppercase() || character == '-'))
            .unwrap_or(after.len());
        if name_len == 0 || !after[name_len..].starts_with('(') {
            rest = after;
            continue;
        }
        let name = &after[..name_len];
        let close = after[name_len..]
            .find(")%")
            .ok_or_else(|| format!("placeholder %{name}( in {text:?} is not closed with )%"))?;
        let argument = &after[name_len + 1..name_len + close];
        if argument.is_empty() {
            return Err(format!("placeholder %{name}()% in {text:?} needs an argument"));
        }
        found.push(match name {
            "STATE" => Placeholder::State(argument.to_owned()),
            "OBSERVED" => Placeholder::Observed(argument.to_owned()),
            "COMMIT" => Placeholder::Commit(argument.to_owned()),
            "PLAN-DIGEST" => {
                let (environment, unit) = argument
                    .split_once('/')
                    .ok_or_else(|| format!("%PLAN-DIGEST({argument})% must name <environment>/<unit>"))?;
                Placeholder::PlanDigest {
                    environment: environment.to_owned(),
                    unit: unit.to_owned(),
                }
            }
            other => return Err(format!("unknown placeholder %{other}(…)% in {text:?}")),
        });
        rest = &after[name_len + close + 2..];
    }
    Ok(found)
}

/// The arguments of a step and its expectations that the runner substitutes
/// placeholders in: the `nyl` arguments and every `nyl get` invocation of its
/// `expect` (and, in [`validate_placeholders`], the string values `get.json`
/// compares).
fn substituted_arguments(step: &Step) -> Vec<&String> {
    let mut arguments: Vec<&String> = step.nyl.iter().flatten().collect();
    if let Some(expect) = &step.expect {
        arguments.extend(expect.get.iter().flat_map(|get| &get.args));
        for mentions in expect.deploy.iter().flat_map(|deploy| deploy.mentions.values()) {
            for mention in mentions {
                if let Mention::Get(args) = mention {
                    arguments.extend(args);
                }
            }
        }
        for value in expect
            .promotion
            .iter()
            .flat_map(|promotion| promotion.values.iter().flatten())
        {
            if let Mention::Get(args) = &value.1.equals {
                arguments.extend(args);
            }
        }
    }
    arguments
}

/// Placeholders are replaced from M6 on, and `%STATE(<id>)%` names a `nyl`
/// step before this one. Ids named by a step's `promotion` expectation
/// (`from.run`, a value's `state`) and `argocd` readings must be earlier
/// steps' too, or the step itself for `from.run`.
fn validate_placeholders(step: &Step, milestone: Milestone, ids: &BTreeMap<String, bool>) -> Result<(), String> {
    // The string values a `get` expectation compares, and the commits a
    // `promotion` expectation names, are substituted too.
    let mut compared = Vec::new();
    for get in step.expect.iter().flat_map(|expect| expect.get.iter()) {
        compared.extend(get.value.iter().filter_map(Value::as_str).map(str::to_owned));
        compared.extend(
            get.json
                .iter()
                .flatten()
                .filter_map(|(_, value)| value.as_str().map(str::to_owned)),
        );
    }
    if let Some(promotion) = step.expect.as_ref().and_then(|expect| expect.promotion.as_ref()) {
        compared.extend(promotion.from.desired_commit.iter().cloned());
        compared.extend(promotion.from.observed_commit.iter().cloned());
    }
    for argument in substituted_arguments(step).into_iter().chain(&compared) {
        let found = placeholders(argument)?;
        if !found.is_empty() && milestone < PLACEHOLDERS_FROM {
            return Err(format!(
                "placeholders run from {PLACEHOLDERS_FROM}, after the scenario's milestone: {argument:?}"
            ));
        }
        for placeholder in found {
            if let Placeholder::State(id) | Placeholder::Observed(id) = &placeholder {
                if !ids.contains_key(id) {
                    return Err(format!("{placeholder:?}: no earlier nyl step has that id"));
                }
            }
        }
    }
    if let Some(promotion) = step.expect.as_ref().and_then(|expect| expect.promotion.as_ref()) {
        let own = step.id.as_ref() == Some(&promotion.from.run);
        if !own && !ids.contains_key(&promotion.from.run) {
            return Err(format!(
                "promotion from.run {:?}: no nyl step has that id",
                promotion.from.run
            ));
        }
        for ignored in promotion.ignored_attestations.iter().flatten() {
            if ignored.attestation.split('/').count() != 2 {
                return Err(format!(
                    "ignoredAttestations {:?} must be <unit>/<name> or environment/<name>",
                    ignored.attestation
                ));
            }
        }
        for (name, value) in promotion.values.iter().flatten() {
            if let Some(state) = &value.state {
                if !ids.contains_key(state) {
                    return Err(format!(
                        "promotion value {name}: state {state:?} names no earlier nyl step"
                    ));
                }
            }
        }
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
            let observe = driver.operation == DriverOperation::Observe;
            // An observation that could not be read, or an artifact that is
            // gone, has a reason, never a failure category: it does not fail
            // the unit.
            let categorized = driver.outcome == DriverOutcome::Failed && !driver.operation.uncategorized();
            if categorized != (driver.category.is_some() && driver.retryable.is_some()) {
                return Err(
                    "a failed driver outcome other than observe's and verify-artifact's, and only that, names its category and retryable"
                        .to_owned(),
                );
            }
            if observe && matches!(driver.outcome, DriverOutcome::Uncertain | DriverOutcome::Drifted) {
                return Err("an observe outcome is succeeded, pending, or failed".to_owned());
            }
            if driver.operation == DriverOperation::VerifyArtifact
                && !matches!(driver.outcome, DriverOutcome::Succeeded | DriverOutcome::Failed)
            {
                return Err("a verify-artifact outcome is succeeded or failed".to_owned());
            }
            if (driver.outcome == DriverOutcome::Pending) && !observe {
                return Err("only an observe outcome can be pending".to_owned());
            }
            if driver.times == Some(0) {
                return Err("times must be at least 1".to_owned());
            }
            if (driver.outcome == DriverOutcome::Drifted) && driver.operation != DriverOperation::DetectDrift {
                return Err("only a detect-drift outcome can be drifted".to_owned());
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
        StepKind::Ref => {
            let reference = step.reference.as_ref().expect("kind");
            let set = match (&reference.set, &reference.to) {
                (Some(_), Some(_)) => true,
                (None, None) => false,
                _ => return Err("ref takes set and to together".to_owned()),
            };
            if reference.delete.is_some() == set {
                return Err("ref takes exactly one of delete and set with to".to_owned());
            }
        }
        StepKind::File => {
            let file = step.file.as_ref().expect("kind");
            if file.content.is_some() == file.delete {
                return Err("file takes exactly one of content and delete: true".to_owned());
            }
            let path = Path::new(&file.path);
            let inside = !file.path.is_empty()
                && path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)));
            if !inside {
                return Err(format!(
                    "file path {:?} must be relative to %SCENARIO_TMP% and stay inside it",
                    file.path
                ));
            }
        }
        StepKind::Argocd => {
            let argocd = step.argocd.as_ref().expect("kind");
            for (release, reading) in std::iter::once((&"*".to_owned(), &argocd.all)).chain(&argocd.releases) {
                if release != "*" && release.split('/').count() != 2 {
                    return Err(format!("argocd release {release:?} must be <group>/<release>"));
                }
                if reading.runs != "unmatched" && !ids.contains_key(&reading.runs) {
                    return Err(format!(
                        "argocd runs {:?}: no earlier nyl step has that id, and it is not unmatched",
                        reading.runs
                    ));
                }
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
