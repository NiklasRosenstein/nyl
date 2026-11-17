//! The evidence rule: what a recorded state proves for a path.
//!
//! Contract: [Promotion paths](../../../../../design/promotion.md#promotion-paths)
//! (evidence levels, failing attestations, `maxAttestationAge`),
//! [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source)
//! ("Evidence covers the whole source"), and [Attestations](../../../../../design/promotion.md#attestations).
//!
//! [`judge`] decides, for one recorded state, which [`Gap`]s keep it below an
//! evidence level and which attestations the promotion relied on:
//!
//! - **Receipts.** Every covered unit has a current receipt in the state
//!   ([`receipt_is_current`]). A covered unit is one the target also selects,
//!   or one a value selector reads.
//! - **Required attestations.** At `attested`, every attestation a covered
//!   unit declares, and every one the source environment declares, passes
//!   for the state; a path's `attestations` replaces a scope's names, and at
//!   `published` adds them. An attestation counts only for the receipt or
//!   state it names ([`evidence::recorded_for`]; an environment-wide one
//!   names the state's desired commit).
//! - **Age.** An attestation older than `maxAttestationAge` does not count:
//!   a required pass is [`GapKind::Aged`]. Age never clears a failure.
//! - **Failing.** A failing attestation recorded for the state blocks it
//!   whether or not the path requires it ([`GapKind::Failing`]), until an
//!   `--ignore-attestation` entry sets it aside; only a later passing
//!   attestation of the same name or that entry clears a failure.
//!
//! - **What runs now (D8).** An attestation its driver declares
//!   running-scoped ([`Driver::running_attestations`]: read from the system
//!   that runs the unit's result, such as a publication's `accepted` and
//!   `healthy`) counts only for the unit's current receipt, the one of the
//!   newest recorded state's observed receipt ([`Context::current`]), current
//!   there or left from a run that did not finish the unit. In an older
//!   state whose receipt of that unit is another one, or when the newest
//!   state has no receipt of it (removed or torn down), it neither passes nor
//!   fails: a required one is [`GapKind::NotRunning`], so the newest state decides,
//!   and a failing newest one blocks without a fallback to an older state
//!   that once passed. An explicitly chosen state (`--state-revision`,
//!   `--revision`) is judged on the attestations recorded for it
//!   (`current` is `None`).
//! - **Per deployment (D6, coverage).** For a value consumed by one
//!   deployment of a unit ([`Subject::Value`]), the unit's running-scoped
//!   attestations are judged for that deployment alone, from the driver's
//!   [`DeploymentVerdict`]s of the current receipt's attestation: the state
//!   whose receipt produced what the deployment runs qualifies when the
//!   deployment meets the attestation there. A path's `coverage` narrows the
//!   deployments a whole-source judgement counts (`applicationGroups`,
//!   `excludeApplications`) and adds deployments that must meet the required
//!   attestations at whatever they run (`requireApplications`). Without
//!   verdicts the attestation is judged as a whole. Which state a consumer
//!   runs is the value-only selection rule, not an evidence requirement, so
//!   it applies at every level. A deployment's publication matches a
//!   state whose receipt published either the matched commit or the
//!   revision it runs.
//! - **Coverage never fails open.** A named group no Application of the
//!   attestation belongs to, a coverage that leaves no Application to judge
//!   while the attestation fails, and a `requireApplications` name no
//!   running-scoped attestation lists are each a [`GapKind::Coverage`] gap.
//!
//! The core never reads the driver's `details`: the shell asks the driver
//! for the typed verdicts ([`super::running`]).
//!
//! [`Driver::running_attestations`]: nyl_core::orchestration::driver::Driver::running_attestations
//! [`DeploymentVerdict`]: nyl_core::orchestration::driver::DeploymentVerdict

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::{DeploymentRuns, DeploymentVerdict};
use nyl_core::orchestration::ids::{CommitId, UnitName};
use nyl_core::orchestration::promotion::EvidenceLevel;
use nyl_core::orchestration::state::{Attestation, AttestationResult, DesiredUnit, Receipt};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::promotion::{PathEvidence, PromotionPathSpec};

use super::history::RecordedState;
use super::IgnoreRequest;
use crate::rules::evidence;
use crate::rules::freshness::receipt_is_current;

/// The scope name of environment-wide attestations, as `--ignore-attestation`
/// and the record write it.
pub const ENVIRONMENT_SCOPE: &str = "environment";

/// What keeps a state below an evidence level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    /// A covered unit has no current receipt in the state.
    NoReceipt,
    /// A required attestation is not recorded for the state.
    Pending,
    /// A required attestation is older than `maxAttestationAge`.
    Aged,
    /// An attestation recorded for the state fails, or, judged per
    /// deployment, a covered deployment falls short of it.
    Failing,
    /// A running-scoped attestation does not describe this state: the unit
    /// has a newer receipt, whose attestations say what runs (D8), or the
    /// deployment that consumes a value runs another state's result.
    NotRunning,
    /// The path's `coverage` cannot be judged as written: it names an
    /// Application or ApplicationGroup no running-scoped attestation of the
    /// current receipts lists, or leaves no Application to judge while the
    /// attestation fails.
    Coverage,
    /// The target runs the state's source commit, and its definitions there
    /// cannot be read (the commit cannot be checked out) or rendered: which
    /// units it would select is unknown ([`TargetUnits::AtSource`]).
    TargetSource,
}

/// The units the target environment selects, which the whole-source rule
/// covers. Contract: [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source),
/// "Evidence covers the whole source".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetUnits {
    /// The target does not run the promoted commit (its source is not
    /// promoted along the path): the units its entry worktree selects.
    Fixed(BTreeSet<UnitName>),
    /// The target runs the promoted source commit, so its definitions at
    /// that commit decide which units it selects: per source commit, those
    /// units, or why the target's definitions there do not render. The
    /// shell renders each candidate's commit once ([`crate::shell::promote::target_units`]).
    AtSource(BTreeMap<CommitId, Result<BTreeSet<UnitName>, String>>),
}

impl TargetUnits {
    /// The units the target selects when it runs `state`, or why they are unknown.
    pub fn of(&self, state: &RecordedState) -> Result<&BTreeSet<UnitName>, String> {
        match self {
            Self::Fixed(units) => Ok(units),
            Self::AtSource(by_commit) => {
                let commit = state
                    .source_commit()
                    .ok_or_else(|| "the state records no source commit".to_owned())?;
                match by_commit.get(commit) {
                    Some(Ok(units)) => Ok(units),
                    Some(Err(reason)) => Err(reason.clone()),
                    None => Err(format!("its definitions at source commit {commit} were not read")),
                }
            }
        }
    }
}

/// One reason a state does not reach a level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// The unit, or [`ENVIRONMENT_SCOPE`].
    pub scope: String,
    /// The attestation, for a gap about one.
    pub name: Option<String>,
    pub kind: GapKind,
    pub message: String,
}

/// An attestation a promotion relied on.
#[derive(Debug, Clone, PartialEq)]
pub struct Used {
    pub scope: String,
    pub name: String,
    pub attestation: Attestation,
}

/// The verdict on one state at one level.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Judgement {
    pub gaps: Vec<Gap>,
    /// The required attestations that passed, or were set aside.
    pub used: Vec<Used>,
    /// The `--ignore-attestation` entries that set a failing attestation aside.
    pub ignored: Vec<IgnoreRequest>,
}

/// What a state is judged for.
pub(super) enum Subject<'a> {
    /// The whole source: every unit the target also selects and every unit a
    /// value reads.
    Source,
    /// The environment and exactly these units must be proven; a failing
    /// attestation of any other unit of the state still blocks it. An
    /// explicitly chosen state of a value-only path is judged so.
    Units(&'a BTreeSet<UnitName>),
    /// One value of a value-only path: the environment and the value's unit
    /// must be proven, and when `deployment` consumes the value, the unit's
    /// running-scoped attestations are judged for that deployment alone. The
    /// running-scoped attestations of other units cover other deployments,
    /// so they neither prove nor block the value (D6).
    Value {
        unit: &'a UnitName,
        deployment: Option<&'a str>,
    },
}

/// What the rule reads besides the state.
pub(super) struct Context<'a> {
    pub spec: &'a PromotionPathSpec,
    pub now: Timestamp,
    /// The units the target environment also selects.
    pub target_units: &'a TargetUnits,
    /// The units the path's value selectors read.
    pub value_units: &'a BTreeSet<UnitName>,
    pub ignores: &'a [IgnoreRequest],
    /// The newest recorded state, whose receipts are the current ones that
    /// running-scoped attestations count for (D8); `None` for an explicitly
    /// chosen state, which is judged on what was recorded for it.
    pub current: Option<&'a RecordedState>,
    /// Every recorded state read, newest first: where a deployment's
    /// publication is looked for.
    pub history: &'a [RecordedState],
}

impl Context<'_> {
    /// The level the path requires.
    pub fn level(&self) -> EvidenceLevel {
        match self.spec.evidence {
            PathEvidence::Published => EvidenceLevel::Published,
            PathEvidence::Attested => EvidenceLevel::Attested,
        }
    }

    /// The level the path requires, as the contract and messages spell it.
    pub fn level_name(&self) -> &'static str {
        match self.level() {
            EvidenceLevel::Published => "published",
            EvidenceLevel::Attested => "attested",
        }
    }

    fn aged(&self, attestation: &Attestation) -> bool {
        self.spec
            .max_attestation_age
            .is_some_and(|age| age.after(attestation.at) < self.now)
    }

    fn ignore(&self, scope: &str, name: &str) -> Option<&IgnoreRequest> {
        self.ignores
            .iter()
            .find(|ignore| ignore.scope == scope && ignore.name == name)
    }

    /// Whether `name` of `unit` is running-scoped, in `state` or in the
    /// current state.
    fn running(&self, state: &RecordedState, unit: &UnitName, name: &str) -> bool {
        let declares = |state: &RecordedState| {
            state
                .running
                .get(unit)
                .is_some_and(|running| running.names.contains(name))
        };
        declares(state) || self.current.is_some_and(declares)
    }

    /// The state whose recorded running-scoped attestations of `unit` say
    /// what runs: the current one, or `state` itself when it was chosen
    /// explicitly.
    fn running_source<'s>(&'s self, state: &'s RecordedState) -> &'s RecordedState {
        self.current.unwrap_or(state)
    }

    /// Whether `state`'s receipt of `unit` is the current one (always, for
    /// an explicitly chosen state): the newest state's observed receipt.
    /// When the newest state has no receipt of the unit (removed or torn
    /// down), no older receipt is current: nothing of it runs as recorded.
    fn is_current(&self, state: &RecordedState, unit: &UnitName) -> bool {
        let Some(current) = self.current else {
            return true;
        };
        match (receipt_key(current, unit), receipt_key(state, unit)) {
            (Some(newest), Some(own)) => newest == own,
            _ => false,
        }
    }

    /// Why `state`'s running-scoped attestation `name` of `unit` no longer
    /// counts.
    fn not_current(&self, unit: &UnitName, name: &str) -> String {
        match self.current.and_then(|current| receipt_key(current, unit)) {
            Some(_) => format!(
                "attestation {unit}/{name} of this state no longer says what runs: unit {unit} has a newer receipt, whose {name} decides"
            ),
            None => format!(
                "attestation {unit}/{name} of this state no longer says what runs: the newest state has no receipt of unit {unit} (it was removed or torn down)"
            ),
        }
    }

    /// The running-scoped attestation `name` of `unit`'s current receipt (the
    /// newest state's observed receipt, current there or not), as recorded,
    /// for judging `state`.
    fn current_attestation<'s>(
        &'s self,
        state: &'s RecordedState,
        unit: &UnitName,
        name: &str,
    ) -> Option<&'s Attestation> {
        let source = self.running_source(state);
        let key = receipt_key(source, unit)?;
        source
            .attestations
            .units
            .get(unit)
            .or_else(|| source.attestations.stale_receipts.get(unit))
            .and_then(|recorded| recorded.get(name))
            .filter(|attestation| attestation.execution_key == key)
    }

    /// The newest recorded state whose receipt of `unit` published `commit`.
    fn published(&self, unit: &UnitName, commits: &[&str]) -> Option<&RecordedState> {
        self.history.iter().find(|state| {
            state
                .running
                .get(unit)
                .and_then(|running| running.publication.as_deref())
                .is_some_and(|publication| commits.contains(&publication))
        })
    }

    fn coverage_narrows(&self) -> bool {
        !self.spec.coverage.application_groups.is_empty() || !self.spec.coverage.exclude_applications.is_empty()
    }

    fn excluded(&self, deployment: &str) -> bool {
        self.spec
            .coverage
            .exclude_applications
            .iter()
            .any(|name| name == deployment)
    }

    /// Whether a whole-source judgement counts `verdict`: in a named group,
    /// when the path names groups, and not excluded.
    fn counts(&self, verdict: &DeploymentVerdict) -> bool {
        let groups = &self.spec.coverage.application_groups;
        (groups.is_empty() || verdict.group.as_ref().is_some_and(|group| groups.contains(group)))
            && !self.excluded(&verdict.name)
    }
}

/// The execution key of `unit`'s receipt in `state`.
fn receipt_key<'s>(state: &'s RecordedState, unit: &UnitName) -> Option<&'s str> {
    state
        .snapshot
        .observed
        .get(unit)
        .and_then(|observed| observed.receipt.as_ref())
        .map(|receipt| receipt.execution_key.as_str())
}

/// The verdicts the driver gave for `unit`'s attestation `name` recorded
/// for `state`'s receipt of it.
fn verdicts<'s>(state: &'s RecordedState, unit: &UnitName, name: &str) -> Option<&'s [DeploymentVerdict]> {
    state
        .running
        .get(unit)
        .and_then(|running| running.verdicts.get(name))
        .map(Vec::as_slice)
}

fn failing_gap(scope: &str, attestation: &Attestation) -> Gap {
    let name = &attestation.name;
    let mut message = format!("attestation {scope}/{name} fails");
    if let Some(reason) = attestation.reason.as_deref().filter(|reason| !reason.is_empty()) {
        message.push_str(": ");
        message.push_str(reason);
    }
    Gap {
        scope: scope.to_owned(),
        name: Some(name.clone()),
        kind: GapKind::Failing,
        message,
    }
}

impl Judgement {
    fn gap(&mut self, scope: &str, name: Option<&str>, kind: GapKind, message: String) {
        self.gaps.push(Gap {
            scope: scope.to_owned(),
            name: name.map(str::to_owned),
            kind,
            message,
        });
    }

    /// Classify one attestation name of a scope: the recorded one that
    /// names the receipt or state (`None` when none does), and whether the
    /// level requires it.
    fn classify(&mut self, ctx: &Context<'_>, scope: &str, name: &str, recorded: Option<&Attestation>, required: bool) {
        let Some(attestation) = recorded else {
            if required {
                self.gap(
                    scope,
                    Some(name),
                    GapKind::Pending,
                    format!("attestation {scope}/{name} is not recorded for this state"),
                );
            }
            return;
        };
        // Age retires a pass only: a failure stays until a later passing
        // attestation of the same name replaces it, or it is ignored.
        if ctx.aged(attestation) && attestation.result == AttestationResult::Pass {
            if required {
                let age = ctx.spec.max_attestation_age.expect("only an age ages");
                self.gap(
                    scope,
                    Some(name),
                    GapKind::Aged,
                    format!(
                        "attestation {scope}/{name} was recorded at {}, older than maxAttestationAge {age}",
                        attestation.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    ),
                );
            }
            return;
        }
        match attestation.result {
            AttestationResult::Pass => {
                if required {
                    self.used.push(used(scope, name, attestation));
                }
            }
            AttestationResult::Fail => {
                if let Some(ignore) = ctx.ignore(scope, name) {
                    self.ignored.push(ignore.clone());
                    if required {
                        self.used.push(used(scope, name, attestation));
                    }
                } else {
                    self.gaps.push(failing_gap(scope, attestation));
                }
            }
        }
    }
}

fn used(scope: &str, name: &str, attestation: &Attestation) -> Used {
    Used {
        scope: scope.to_owned(),
        name: name.to_owned(),
        attestation: attestation.clone(),
    }
}

/// How a unit's running-scoped attestations are judged.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Running<'a> {
    /// As a whole, for the current receipt only (D8).
    Whole,
    /// Per deployment, counting those the path's `coverage` covers.
    Coverage,
    /// For the one deployment that consumes a value.
    Consumer(&'a str),
    /// Not at all: they cover deployments the subject does not.
    Ignored,
}

/// Judge `state` at `level` for `subject`.
pub(super) fn judge(
    ctx: &Context<'_>,
    state: &RecordedState,
    subject: &Subject<'_>,
    level: EvidenceLevel,
) -> Judgement {
    let mut verdict = Judgement::default();
    let desired = &state.snapshot.desired;
    // The units whose receipts and required attestations count.
    let covered: BTreeSet<UnitName> = match subject {
        Subject::Source => match ctx.target_units.of(state) {
            Ok(units) => units
                .iter()
                .filter(|unit| desired.contains_key(*unit))
                .chain(ctx.value_units)
                .cloned()
                .collect(),
            Err(reason) => {
                verdict.gap(
                    ENVIRONMENT_SCOPE,
                    None,
                    GapKind::TargetSource,
                    format!(
                        "the target runs this state's source commit, but cannot tell which units it selects there: {reason}"
                    ),
                );
                ctx.value_units.clone()
            }
        },
        Subject::Units(units) => (*units).clone(),
        Subject::Value { unit, .. } => BTreeSet::from([(*unit).clone()]),
    };
    for unit in covered.iter().filter(|unit| !desired.contains_key(*unit)) {
        verdict.gap(
            unit.as_str(),
            None,
            GapKind::NoReceipt,
            format!("unit {unit} is not selected in this state, so it has no receipt"),
        );
    }
    // A failing attestation of any unit blocks the state.
    for unit in desired.keys() {
        let running = match subject {
            Subject::Source | Subject::Units(_) if ctx.coverage_narrows() => Running::Coverage,
            Subject::Source | Subject::Units(_) => Running::Whole,
            Subject::Value {
                unit: consumer,
                deployment,
            } if *consumer == unit => deployment.map_or(Running::Whole, Running::Consumer),
            Subject::Value { .. } => Running::Ignored,
        };
        judge_unit(ctx, state, level, (unit, covered.contains(unit)), running, &mut verdict);
    }
    judge_environment(ctx, state, level, &mut verdict);
    require_applications(ctx, state, level, &mut verdict);
    verdict
        .gaps
        .sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
    verdict
        .used
        .sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
    verdict
}

/// The names a scope requires at `level`: the path's list when it names the
/// scope, else every declared name at `attested`.
fn required_names(named: Option<&Vec<String>>, declared: BTreeSet<String>, level: EvidenceLevel) -> BTreeSet<String> {
    match (named, level) {
        (Some(named), _) => named.iter().cloned().collect(),
        (None, EvidenceLevel::Attested) => declared,
        (None, EvidenceLevel::Published) => BTreeSet::new(),
    }
}

/// The names `unit` must pass at `level` when covered.
fn unit_required(ctx: &Context<'_>, desired: &DesiredUnit, unit: &UnitName, level: EvidenceLevel) -> BTreeSet<String> {
    required_names(
        ctx.spec.attestations.units.get(unit.as_str()),
        evidence::declared(desired),
        level,
    )
}

/// The current receipt of `unit` in `state`, with its desired document.
fn current_receipt<'s>(state: &'s RecordedState, unit: &UnitName) -> Option<(&'s DesiredUnit, &'s Receipt)> {
    let desired = state.snapshot.desired.get(unit)?;
    let observed = state.snapshot.observed.get(unit);
    if !receipt_is_current(desired, observed) {
        return None;
    }
    Some((desired, observed?.receipt.as_ref()?))
}

fn judge_unit(
    ctx: &Context<'_>,
    state: &RecordedState,
    level: EvidenceLevel,
    (unit, covered): (&UnitName, bool),
    running: Running<'_>,
    verdict: &mut Judgement,
) {
    let Some((desired, receipt)) = current_receipt(state, unit) else {
        if covered {
            verdict.gap(
                unit.as_str(),
                None,
                GapKind::NoReceipt,
                format!("unit {unit} has no current receipt in this state"),
            );
        }
        return;
    };
    let recorded = state.attestations.units.get(unit);
    let required = if covered {
        unit_required(ctx, desired, unit, level)
    } else {
        BTreeSet::new()
    };
    let mut names: BTreeSet<&str> = recorded
        .into_iter()
        .flat_map(|set| set.keys())
        .map(String::as_str)
        .chain(required.iter().map(String::as_str))
        .collect();
    if let Running::Consumer(deployment) = running {
        // Which state the consumer runs is read from the current receipt's
        // running-scoped attestations, whatever this state recorded and at
        // every evidence level (D6, D7).
        let declared: BTreeSet<&str> = [Some(state), ctx.current]
            .into_iter()
            .flatten()
            .filter_map(|state| state.running.get(unit))
            .flat_map(|running| running.names.iter().map(String::as_str))
            .collect();
        for name in declared {
            judge_consumer(ctx, state, (unit, name, deployment), required.contains(name), verdict);
            names.remove(name);
        }
    }
    for name in names {
        let required = required.contains(name);
        if ctx.running(state, unit, name) {
            match running {
                Running::Ignored | Running::Consumer(_) => continue,
                Running::Whole | Running::Coverage if !ctx.is_current(state, unit) => {
                    if required {
                        verdict.gap(
                            unit.as_str(),
                            Some(name),
                            GapKind::NotRunning,
                            ctx.not_current(unit, name),
                        );
                    }
                    continue;
                }
                Running::Coverage => {
                    if let Some(verdicts) = verdicts(state, unit, name) {
                        let attestation = evidence::recorded_for(name, desired, receipt, recorded);
                        judge_coverage(ctx, (unit, name), attestation, verdicts, required, verdict);
                        continue;
                    }
                }
                Running::Whole => {}
            }
        }
        let attestation = evidence::recorded_for(name, desired, receipt, recorded);
        verdict.classify(ctx, unit.as_str(), name, attestation, required);
    }
}

/// A running-scoped attestation judged over the deployments the path's
/// `coverage` counts: each must meet it at this receipt.
fn judge_coverage(
    ctx: &Context<'_>,
    (unit, name): (&UnitName, &str),
    attestation: Option<&Attestation>,
    verdicts: &[DeploymentVerdict],
    required: bool,
    verdict: &mut Judgement,
) {
    let Some(attestation) = attestation else {
        verdict.classify(ctx, unit.as_str(), name, None, required);
        return;
    };
    // Every group the path names must be one of these Applications', so a
    // misspelt group never leaves the attestation unjudged.
    for group in &ctx.spec.coverage.application_groups {
        if !verdicts
            .iter()
            .any(|deployment| deployment.group.as_ref() == Some(group))
        {
            verdict.gap(
                unit.as_str(),
                Some(name),
                GapKind::Coverage,
                format!(
                    "the path's coverage.applicationGroups names {group}, but no Application that attestation {unit}/{name} lists belongs to it"
                ),
            );
        }
    }
    let counted: Vec<&DeploymentVerdict> = verdicts.iter().filter(|deployment| ctx.counts(deployment)).collect();
    if counted.is_empty() && attestation.result == AttestationResult::Fail {
        verdict.gap(
            unit.as_str(),
            Some(name),
            GapKind::Coverage,
            format!(
                "the path's coverage (applicationGroups, excludeApplications) leaves no Application of attestation {unit}/{name} to judge, and it fails{}",
                attestation.reason.as_deref().map(|reason| format!(": {reason}")).unwrap_or_default()
            ),
        );
        return;
    }
    let short: Vec<String> = counted
        .into_iter()
        .filter(|deployment| !(deployment.meets && deployment.runs == DeploymentRuns::Receipt))
        .map(|deployment| {
            deployment
                .reason
                .clone()
                .unwrap_or_else(|| format!("{} does not run this receipt", deployment.name))
        })
        .collect();
    if short.is_empty() {
        // Passing for what the path covers, whatever the aggregate says.
        let passing = Attestation {
            result: AttestationResult::Pass,
            ..attestation.clone()
        };
        verdict.classify(ctx, unit.as_str(), name, Some(&passing), required);
        if let Some(last) = verdict.used.last_mut() {
            if last.scope == unit.as_str() && last.name == name {
                last.attestation = attestation.clone();
            }
        }
        return;
    }
    let failing = Attestation {
        result: AttestationResult::Fail,
        reason: Some(format!("for the Applications the path covers: {}", short.join("; "))),
        ..attestation.clone()
    };
    verdict.classify(ctx, unit.as_str(), name, Some(&failing), required);
}

/// A running-scoped attestation judged for the one deployment that consumes
/// a value: from the current receipt's attestation, the deployment must
/// meet it, and what it runs must be what `state`'s receipt produced. That
/// last part is the value-only selection rule (D6, D7), not an evidence
/// requirement, so it holds at every level and under `--evidence published`.
fn judge_consumer(
    ctx: &Context<'_>,
    state: &RecordedState,
    (unit, name, deployment): (&UnitName, &str, &str),
    required: bool,
    verdict: &mut Judgement,
) {
    let Some(attestation) = ctx.current_attestation(state, unit, name) else {
        if required {
            verdict.gap(
                unit.as_str(),
                Some(name),
                GapKind::Pending,
                format!("attestation {unit}/{name} is not recorded for unit {unit}'s current receipt"),
            );
        }
        return;
    };
    let source = ctx.running_source(state);
    let Some(verdicts) = verdicts(source, unit, name) else {
        // The driver cannot tell deployments apart: the attestation as a
        // whole, for the current receipt only.
        if ctx.is_current(state, unit) {
            verdict.classify(ctx, unit.as_str(), name, Some(attestation), required);
        } else if required {
            verdict.gap(
                unit.as_str(),
                Some(name),
                GapKind::NotRunning,
                ctx.not_current(unit, name),
            );
        }
        return;
    };
    let Some(found) = verdicts.iter().find(|verdict| verdict.name == deployment) else {
        if required {
            verdict.gap(
                unit.as_str(),
                Some(name),
                GapKind::Pending,
                format!("attestation {unit}/{name} lists no deployment {deployment}, which consumes the value"),
            );
        }
        return;
    };
    let excused = ctx.excluded(deployment) && found.runs != DeploymentRuns::Unproven;
    if !found.meets && !excused {
        let reason = found
            .reason
            .clone()
            .unwrap_or_else(|| format!("{deployment} falls short"));
        let failing = Attestation {
            result: AttestationResult::Fail,
            reason: Some(reason),
            ..attestation.clone()
        };
        verdict.classify(ctx, unit.as_str(), name, Some(&failing), required);
        if !verdict
            .ignored
            .iter()
            .any(|ignore| ignore.scope == unit.as_str() && ignore.name == name)
        {
            return;
        }
    }
    if !consumer_runs_state(ctx, (state, source), (unit, name, deployment), &found.runs, verdict) {
        return;
    }
    if found.meets || excused {
        let passing = Attestation {
            result: AttestationResult::Pass,
            ..attestation.clone()
        };
        verdict.classify(ctx, unit.as_str(), name, Some(&passing), required);
        if let Some(last) = verdict.used.last_mut() {
            if last.scope == unit.as_str() && last.name == name {
                last.attestation = attestation.clone();
            }
        }
    }
}

/// The value-only selection rule (D6, D7): whether `state`'s receipt of
/// `unit` published what the consuming `deployment` runs, as the current
/// receipt's (`source`'s) attestation `name` shows it. When it did not, the
/// gap says which state the consumer runs instead, or that no recorded
/// state published it.
fn consumer_runs_state(
    ctx: &Context<'_>,
    (state, source): (&RecordedState, &RecordedState),
    (unit, name, deployment): (&UnitName, &str, &str),
    runs: &DeploymentRuns,
    verdict: &mut Judgement,
) -> bool {
    let rule = "a path that promotes only values takes each value from the state whose receipt published what its consumer runs";
    let here = match runs {
        DeploymentRuns::Receipt => receipt_key(state, unit) == receipt_key(source, unit),
        DeploymentRuns::Publication {
            publication,
            running,
            truncated,
        } => {
            let commits: Vec<&str> = std::iter::once(publication.as_str())
                .chain(running.as_deref())
                .collect();
            let own = state
                .running
                .get(unit)
                .and_then(|running| running.publication.as_deref());
            if own.is_some_and(|own| commits.contains(&own)) {
                true
            } else {
                if ctx.published(unit, &commits).is_none() {
                    let truncated = if *truncated {
                        "; the driver's matching stopped at its limit, so an older publication may have introduced it"
                    } else {
                        ""
                    };
                    verdict.gap(
                        unit.as_str(),
                        Some(name),
                        GapKind::NotRunning,
                        format!(
                            "no recorded state published what {deployment} runs ({publication}){truncated}, and {rule}: promote a state explicitly with `--state-revision`, or have the source publish again so a recorded state holds it"
                        ),
                    );
                    return false;
                }
                false
            }
        }
        DeploymentRuns::Unproven => false,
    };
    if !here {
        let runs = match runs {
            DeploymentRuns::Receipt => format!("what unit {unit}'s current receipt published"),
            DeploymentRuns::Publication { publication, .. } => format!("publication {publication}"),
            DeploymentRuns::Unproven => "nothing a receipt of the unit explains".to_owned(),
        };
        verdict.gap(
            unit.as_str(),
            Some(name),
            GapKind::NotRunning,
            format!("{deployment}, which consumes the value, runs {runs}, not what this state's {unit} published, and {rule}"),
        );
        return false;
    }
    true
}

/// `coverage.requireApplications`: each named deployment must appear in a
/// running-scoped attestation of the current receipts, and meet every
/// required one at whatever it runs. A name no attestation lists, such as
/// a misspelt one or one of a publication that does not observe, is a
/// [`GapKind::Coverage`] gap, whatever the path requires.
fn require_applications(ctx: &Context<'_>, state: &RecordedState, level: EvidenceLevel, verdict: &mut Judgement) {
    let names = &ctx.spec.coverage.require_applications;
    if names.is_empty() {
        return;
    }
    let source = ctx.running_source(state);
    for application in names {
        let mut listed = false;
        for (unit, running) in &source.running {
            let Some(desired) = source.snapshot.desired.get(unit) else {
                continue;
            };
            let required = unit_required(ctx, desired, unit, level);
            for name in &running.names {
                let Some(found) = running
                    .verdicts
                    .get(name)
                    .and_then(|verdicts| verdicts.iter().find(|verdict| &verdict.name == application))
                else {
                    continue;
                };
                let Some(attestation) = ctx.current_attestation(state, unit, name) else {
                    continue;
                };
                listed = true;
                if !required.contains(name) {
                    continue;
                }
                let duplicate = verdict.used.iter().any(|known| {
                    known.scope == unit.as_str()
                        && known.name == *name
                        && known.attestation.execution_key == attestation.execution_key
                });
                if found.meets {
                    if !duplicate {
                        verdict.used.push(used(unit.as_str(), name, attestation));
                    }
                } else if let Some(ignore) = ctx.ignore(unit.as_str(), name) {
                    if !verdict.ignored.contains(ignore) {
                        verdict.ignored.push(ignore.clone());
                    }
                } else {
                    verdict.gap(
                        unit.as_str(),
                        Some(name),
                        GapKind::Failing,
                        format!(
                            "attestation {unit}/{name} fails for {application}, which the path's coverage.requireApplications requires: {}",
                            found.reason.as_deref().unwrap_or("it falls short")
                        ),
                    );
                }
            }
        }
        if !listed {
            verdict.gap(
                ENVIRONMENT_SCOPE,
                None,
                GapKind::Coverage,
                format!(
                    "the path's coverage.requireApplications names {application}, which no running-scoped attestation of the source's current receipts lists"
                ),
            );
        }
    }
}

fn judge_environment(ctx: &Context<'_>, state: &RecordedState, level: EvidenceLevel, verdict: &mut Judgement) {
    let declared: BTreeSet<String> = state
        .snapshot
        .environment
        .as_ref()
        .map(|record| evidence::declared_environment(&record.spec))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let required = required_names(ctx.spec.attestations.environment.as_ref(), declared, level);
    let key = state.desired_commit.as_str();
    let recorded = |name: &str| {
        state
            .attestations
            .environment
            .get(name)
            .filter(|attestation| attestation.execution_key == key)
    };
    let names: BTreeSet<&str> = state
        .attestations
        .environment
        .keys()
        .map(String::as_str)
        .chain(required.iter().map(String::as_str))
        .collect();
    for name in names {
        verdict.classify(ctx, ENVIRONMENT_SCOPE, name, recorded(name), required.contains(name));
    }
}
