//! The decision core of `nyl promote`: which recorded state a promotion takes,
//! what evidence it rests on, and the PromotionRecord it would write.
//!
//! Contract: [Promotion paths](../../../../../design/promotion.md#promotion-paths),
//! [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source),
//! [Health evidence](../../../../../design/promotion.md#health-evidence), and the
//! decisions D2, D11, and D13 (D5, D6, D7, and D8 are not here: see below) of the
//! [M6 plan](../../../../../design/m6-plan.md#open-decisions).
//!
//! [`decide`] is a pure function of the source environment's recorded states
//! ([`RecordedState`], which the shell reads from Git history: PR 6), the
//! path, and the invocation's [`Request`]. It performs no effect and reads no
//! clock: `now` and the next `sequence` are passed in. Its owners:
//!
//! - [`history`]: recorded states from the desired and observed history by
//!   `Nyl-Run-Id`.
//! - [`standing`]: the evidence rule for one state, at `published` and
//!   `attested`: receipts, required attestations, `maxAttestationAge`, and
//!   failing attestations.
//! - [`values`]: reading each promoted value through the shared reference
//!   resolver.
//! - [`lineage`]: what `status` shows of the promotion an environment runs.
//! - this module: state selection and the record.
//!
//! **State selection.** Without flags, the newest state whose evidence
//! reaches the path's level. `--revision <source commit>` takes the newest
//! qualifying state with that source commit and `--state-revision <desired
//! commit>` exactly one ([`Selection`]); an explicitly chosen state reaches
//! `attested` through the attestations recorded for it, so it needs
//! `--evidence published` only when those do not reach the level (D13), and
//! then the record carries an override ([`Code::PromoteEvidenceOverride`]).
//! With `--evidence published` and no explicit state, the newest state that
//! reaches `published` is chosen, and the record carries an override only when
//! that state is below the path's level. `--evidence published` never clears a
//! failing attestation: only
//! `--ignore-attestation <scope>/<name>=<reason>` does, and the record keeps
//! the reason ([`IgnoreRequest`]); an entry for an attestation that is pending
//! or aged, not failing, sets nothing aside. A state that ran an archived worktree
//! snapshot is never a candidate.
//!
//! **Refusal.** When no state reaches the level, nothing is written:
//! [`Refusal`] carries [`Code::PromoteNoEvidence`] (exit 2) naming what the
//! newest candidate lacks, with the fix. A selector that cannot be read is
//! [`Code::PromoteSelectorInvalid`] (exit 1).
//!
//! **Unchanged (D2).** When the chosen state and values are what the path's
//! record already names and that record is the newest on the target (no other
//! path wrote since), the decision is [`Decision::Unchanged`]. "The record
//! supplies every value of the target" is approximated by "it is the newest
//! record on the target": the binding suppliers of path-less bindings (PR 9)
//! can refine it when the shell passes them in.
//!
//! **Which paths.** A path whose target takes its source from the promotion
//! (`promotes_source`) takes the source commit and every value from one state
//! under the whole-source rule. Any other path promotes values: each value
//! comes from the newest state in which its consumer ([`values::consumer`])
//! reaches the level, `from` is the newest of those states, and a value from
//! another state names it (D11).
//!
//! Not here: the next `sequence` from history (PR 6), the supersede check
//! (PR 9), artifact verification and every other effect (PR 6; the decision
//! lists what to verify), a `--value` subset (PR 6), and the value-only
//! specifics of PR 11: judging a publication's `accepted` and `healthy` per
//! covered Application (D6) and tracing an input selector to the Application
//! of its Release (D7). Until then a value-only path judges the selected unit
//! like any other unit, and a failing attestation of any unit of the chosen
//! state blocks it. Also not here: D8 (a publication's newest attestation
//! decides what runs, and mixed revisions block). It needs a typed,
//! driver-decided "what runs now" result in state, which the core cannot read
//! from the driver's private `details`; see the M6 plan.

mod history;
mod lineage;
mod standing;
mod values;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, PromotionPathName, UnitName};
use nyl_core::orchestration::promotion::{
    EnvironmentFrom, EvidenceAttestation, EvidenceLevel, EvidenceOverride, IgnoredAttestation, PromotedState,
    PromotedValue, PromotionEvidence, PromotionFrom, PromotionRecord, PromotionRequest,
};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::promotion::PromotionPath;

pub use history::{recorded_states, DesiredEntry, RecordedState, StateAttestations, StateRef};
pub use lineage::lineage;
pub use standing::{Gap, GapKind, Judgement, Used, ENVIRONMENT_SCOPE};
pub use values::ArtifactCheck;

use standing::{judge, Context, Subject};

use crate::run_report::ExitCategory;

/// `--ignore-attestation <scope>/<name>=<reason>`: set a failing attestation
/// aside, with the reason the record keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreRequest {
    /// A unit name, or [`ENVIRONMENT_SCOPE`].
    pub scope: String,
    pub name: String,
    pub reason: String,
}

impl IgnoreRequest {
    /// Parse `scope/name=reason`; the reason may itself contain `=` and `/`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let malformed =
            || format!("--ignore-attestation {text:?} is not <unit>/<name>=<reason> or environment/<name>=<reason>");
        let (target, reason) = text.split_once('=').ok_or_else(malformed)?;
        let (scope, name) = target.split_once('/').ok_or_else(malformed)?;
        if scope.is_empty() || name.is_empty() || name.contains('/') || reason.trim().is_empty() {
            return Err(malformed());
        }
        Ok(Self {
            scope: scope.to_owned(),
            name: name.to_owned(),
            reason: reason.trim().to_owned(),
        })
    }
}

/// Which recorded state the invocation asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The newest state that reaches the level.
    Newest,
    /// `--revision`: the newest qualifying state that ran this source commit.
    SourceRevision(CommitId),
    /// `--state-revision`: exactly this state, by its desired commit.
    StateRevision(CommitId),
}

impl Selection {
    fn explicit(&self) -> bool {
        *self != Self::Newest
    }

    fn admits(&self, state: &RecordedState) -> bool {
        match self {
            Self::Newest => true,
            Self::SourceRevision(commit) => state.source_commit() == Some(commit),
            Self::StateRevision(commit) => state.desired_commit == *commit,
        }
    }
}

/// What the invocation asked.
#[derive(Debug, Clone)]
pub struct Request {
    pub select: Selection,
    /// `--evidence published`: accept a state below the path's level.
    pub evidence_published: bool,
    pub ignore: Vec<IgnoreRequest>,
    /// Who asks, when, and why, recorded like an approval.
    pub requested: PromotionRequest,
}

/// Everything [`decide`] reads.
pub struct PromotionInput<'a> {
    /// A validated path ([`crate::rules::promotion_paths`]) from `source` to the target.
    pub path: &'a PromotionPath,
    pub source: &'a EnvironmentName,
    /// The source's recorded states, newest first, with the documents at
    /// each and the attestations recorded for it.
    pub history: &'a [RecordedState],
    /// The units the target environment also selects: the whole-source rule
    /// covers them.
    pub target_units: &'a BTreeSet<UnitName>,
    /// Whether the target's source is `fromPromotion` naming this path.
    pub promotes_source: bool,
    /// The records in the target environment's desired state, by path.
    pub target_records: &'a BTreeMap<PromotionPathName, PromotionRecord>,
    /// The record's sequence, from the target's history (PR 6).
    pub sequence: u64,
    pub now: Timestamp,
    pub request: &'a Request,
}

/// What `nyl promote` would do.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Write this record.
    Promote(Box<Promotion>),
    /// The chosen state and values are what the path's record already names
    /// (`outcome: unchanged`); nothing is written.
    Unchanged(Box<PromotionRecord>),
}

/// A promotion to record.
#[derive(Debug, Clone, PartialEq)]
pub struct Promotion {
    /// The record to write. `artifactsVerified` is `false`; the shell sets it
    /// once it has verified [`Self::artifacts`].
    pub record: PromotionRecord,
    /// [`Code::PromoteEvidenceOverride`] when the evidence is below the path's level.
    pub warnings: Vec<Finding>,
    /// The artifacts the promoted values read, for `verifyArtifacts`.
    pub artifacts: Vec<ArtifactCheck>,
    /// `--ignore-attestation` entries that set nothing aside; the record
    /// omits them.
    pub unused_ignores: Vec<IgnoreRequest>,
}

/// Why nothing is promoted.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub finding: Finding,
    /// 2 for missing, stale, mixed, or failing evidence, 1 for a selector
    /// that cannot be read.
    pub exit: ExitCategory,
    /// What the newest candidate lacks, for evidence refusals.
    pub gaps: Vec<Gap>,
}

impl Refusal {
    fn no_evidence(finding: Finding, gaps: Vec<Gap>) -> Self {
        Self {
            finding,
            exit: ExitCategory::Incomplete,
            gaps,
        }
    }

    fn invalid(finding: Finding) -> Self {
        Self {
            finding,
            exit: ExitCategory::Error,
            gaps: Vec::new(),
        }
    }
}

/// One source state evaluated against the path, for `nyl get
/// promotion-candidates`.
#[derive(Debug, Clone, PartialEq)]
pub struct StateEvaluation {
    pub run: nyl_core::orchestration::ids::RunId,
    pub desired_commit: CommitId,
    pub observed_commit: Option<CommitId>,
    pub source_commit: Option<CommitId>,
    /// The highest level the state reaches, up to the path's, as `nyl
    /// promote` judges it: for a path that promotes the source, by the
    /// whole-source rule; for a value-only path, the highest any value
    /// reaches ([`Self::by_value`]). `None` below `published`.
    pub level: Option<EvidenceLevel>,
    /// Whether `nyl promote` without flags could take the state: for the
    /// whole source, it reaches the path's level; for a value-only path,
    /// some value's consumer does, so the state can supply that value.
    pub eligible: bool,
    /// Why the state, or a value of a value-only path, does not reach the
    /// path's level; empty when nothing falls short.
    pub gaps: Vec<Gap>,
    /// The values the state would carry, by name; a value the state cannot
    /// supply (its producer has no current receipt) is left out.
    pub values: BTreeMap<String, serde_json::Value>,
    /// For a value-only path, each value judged for its consumer, as
    /// `nyl promote` selects a state per value; empty otherwise.
    pub by_value: BTreeMap<String, ValueEvaluation>,
}

/// One value of a value-only path judged in one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueEvaluation {
    /// The highest level its consumer reaches, up to the path's.
    pub level: Option<EvidenceLevel>,
    /// Whether it reaches the path's level.
    pub eligible: bool,
}

/// Whether `level` satisfies `required`.
fn reaches(level: Option<EvidenceLevel>, required: EvidenceLevel) -> bool {
    matches!(
        (level, required),
        (Some(EvidenceLevel::Attested), _) | (Some(EvidenceLevel::Published), EvidenceLevel::Published)
    )
}

fn context<'a>(input: &'a PromotionInput<'_>, value_units: &'a BTreeSet<UnitName>) -> Context<'a> {
    Context {
        spec: &input.path.spec,
        now: input.now,
        target_units: input.target_units,
        value_units,
        ignores: &input.request.ignore,
    }
}

fn value_units(path: &PromotionPath) -> BTreeSet<UnitName> {
    values::units(path.spec.values.values().map(|value| &value.select))
}

/// Every candidate state of the source judged as `nyl promote` judges it,
/// newest first, without applying the invocation's selection or ignores: a
/// path that promotes the target's source (or no values) by the whole-source
/// rule, a value-only path per value, for the environment and that value's
/// consumer, as `select_values` picks a state per value. A state `decide`
/// takes for some value is therefore always `eligible`. The level is the
/// highest the state reaches up to the path's own: a `published` path
/// reports `published` or nothing.
pub fn evaluate(input: &PromotionInput<'_>) -> Vec<StateEvaluation> {
    let value_units = value_units(input.path);
    let ctx = Context {
        ignores: &[],
        ..context(input, &value_units)
    };
    let whole = input.promotes_source || input.path.spec.values.is_empty();
    // A value-only path selects a state per value, for that value's consumer.
    let consumers: Vec<(&String, BTreeSet<UnitName>)> = input
        .path
        .spec
        .values
        .iter()
        .filter_map(|(name, value)| values::consumer(&value.select).map(|unit| (name, BTreeSet::from([unit]))))
        .collect();
    let levels: &[EvidenceLevel] = match ctx.level() {
        EvidenceLevel::Attested => &[EvidenceLevel::Attested, EvidenceLevel::Published],
        EvidenceLevel::Published => &[EvidenceLevel::Published],
    };
    let required = ctx.level();
    input
        .history
        .iter()
        .filter(|state| state.is_candidate())
        .map(|state| {
            let reach = |subject: &Subject<'_>| {
                levels
                    .iter()
                    .copied()
                    .find(|level| judge(&ctx, state, subject, *level).gaps.is_empty())
            };
            let mut by_value = BTreeMap::new();
            let (level, eligible, gaps) = if whole {
                let level = reach(&Subject::Source);
                let gaps = judge(&ctx, state, &Subject::Source, required).gaps;
                (level, reaches(level, required), gaps)
            } else {
                let mut gaps: Vec<Gap> = Vec::new();
                for (name, units) in &consumers {
                    let subject = Subject::Units(units);
                    let level = reach(&subject);
                    let eligible = reaches(level, required);
                    if !eligible {
                        for gap in judge(&ctx, state, &subject, required).gaps {
                            if !gaps.contains(&gap) {
                                gaps.push(gap);
                            }
                        }
                    }
                    by_value.insert((*name).clone(), ValueEvaluation { level, eligible });
                }
                gaps.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
                let level = if by_value
                    .values()
                    .any(|value| value.level == Some(EvidenceLevel::Attested))
                {
                    Some(EvidenceLevel::Attested)
                } else if by_value.values().any(|value| value.level.is_some()) {
                    Some(EvidenceLevel::Published)
                } else {
                    None
                };
                (level, by_value.values().any(|value| value.eligible), gaps)
            };
            StateEvaluation {
                run: state.run.clone(),
                desired_commit: state.desired_commit.clone(),
                observed_commit: state.observed_commit.clone(),
                source_commit: state.source_commit().cloned(),
                level,
                eligible,
                gaps,
                values: input
                    .path
                    .spec
                    .values
                    .iter()
                    .filter_map(|(name, value)| {
                        values::read(input.source, state, name, &value.select, input.now)
                            .ok()
                            .map(|read| (name.clone(), read.value.value))
                    })
                    .collect(),
                by_value,
            }
        })
        .collect()
}

/// A state chosen, with the judgement it was chosen on.
struct Chosen<'a> {
    state: &'a RecordedState,
    judgement: Judgement,
    /// The gaps at the path's level, when the state was taken below it.
    overridden: Option<Vec<Gap>>,
}

/// Decide the promotion: the record it would write, that it changes nothing,
/// or why it is refused.
pub fn decide(input: &PromotionInput<'_>) -> Result<Decision, Refusal> {
    let value_units = value_units(input.path);
    let ctx = context(input, &value_units);
    let whole = input.promotes_source || input.path.spec.values.is_empty();
    let (chosen, per_value) = if whole {
        (vec![select_whole(input, &ctx)?], BTreeMap::new())
    } else {
        select_values(input, &ctx)?
    };
    build(input, &ctx, &chosen, &per_value, whole)
}

fn candidates<'a>(input: &'a PromotionInput<'_>) -> Vec<&'a RecordedState> {
    input
        .history
        .iter()
        .filter(|state| state.is_candidate() && input.request.select.admits(state))
        .collect()
}

/// Whether `state` qualifies for `subject`: it reaches the path's level, or,
/// with `--evidence published`, it reaches `published`. The second element
/// holds the gaps at the path's level when the state was taken below it, so
/// the record carries an override only then.
fn qualify<'a>(
    input: &PromotionInput<'_>,
    ctx: &Context<'_>,
    state: &'a RecordedState,
    subject: &Subject<'_>,
) -> Option<Chosen<'a>> {
    let judgement = judge(ctx, state, subject, ctx.level());
    if judgement.gaps.is_empty() {
        return Some(Chosen {
            state,
            judgement,
            overridden: None,
        });
    }
    if input.request.evidence_published && ctx.level() == EvidenceLevel::Attested {
        let relaxed = judge(ctx, state, subject, EvidenceLevel::Published);
        if relaxed.gaps.is_empty() {
            return Some(Chosen {
                state,
                judgement: relaxed,
                overridden: Some(judgement.gaps),
            });
        }
    }
    None
}

fn select_whole<'a>(input: &'a PromotionInput<'_>, ctx: &Context<'_>) -> Result<Chosen<'a>, Refusal> {
    let pool = candidates(input);
    let subject = Subject::Source;
    if let Some(chosen) = pool.iter().find_map(|state| qualify(input, ctx, state, &subject)) {
        return Ok(chosen);
    }
    let gaps = pool
        .first()
        .map(|state| judge(ctx, state, &subject, ctx.level()).gaps)
        .unwrap_or_default();
    Err(refuse(input, &pool, gaps, None))
}

/// The value-only selection: each value from the newest state in which its
/// consumer reaches the level; an explicit choice is one state for all.
fn select_values<'a>(
    input: &'a PromotionInput<'_>,
    ctx: &Context<'_>,
) -> Result<(Vec<Chosen<'a>>, BTreeMap<String, usize>), Refusal> {
    let pool = candidates(input);
    let mut consumers = BTreeMap::new();
    for (name, value) in &input.path.spec.values {
        let unit = values::preflight(name, &value.select).map_err(Refusal::invalid)?;
        consumers.insert(name.clone(), unit);
    }
    let all: BTreeSet<UnitName> = consumers.values().cloned().collect();
    let mut chosen: Vec<Chosen<'a>> = Vec::new();
    let mut per_value = BTreeMap::new();
    let find = |units: &BTreeSet<UnitName>| {
        let subject = Subject::Units(units);
        pool.iter().find_map(|state| qualify(input, ctx, state, &subject))
    };
    if input.request.select.explicit() {
        let Some(found) = find(&all) else {
            let gaps = pool
                .first()
                .map(|state| judge(ctx, state, &Subject::Units(&all), ctx.level()).gaps)
                .unwrap_or_default();
            return Err(refuse(input, &pool, gaps, None));
        };
        for name in consumers.keys() {
            per_value.insert(name.clone(), 0);
        }
        chosen.push(found);
        return Ok((chosen, per_value));
    }
    for (name, unit) in &consumers {
        let units = BTreeSet::from([unit.clone()]);
        let Some(found) = find(&units) else {
            let gaps = pool
                .first()
                .map(|state| judge(ctx, state, &Subject::Units(&units), ctx.level()).gaps)
                .unwrap_or_default();
            return Err(refuse(input, &pool, gaps, Some(name)));
        };
        let index = if let Some(index) = chosen.iter().position(|known| known.state.run == found.state.run) {
            index
        } else {
            chosen.push(found);
            chosen.len() - 1
        };
        per_value.insert(name.clone(), index);
    }
    Ok((chosen, per_value))
}

/// The refusal for a pool with no qualifying state.
fn refuse(input: &PromotionInput<'_>, pool: &[&RecordedState], gaps: Vec<Gap>, value: Option<&str>) -> Refusal {
    let source = input.source;
    let level = match input.path.spec.evidence {
        nyl_core::resources::promotion::PathEvidence::Published => "published",
        nyl_core::resources::promotion::PathEvidence::Attested => "attested",
    };
    let path = &input.path.metadata.name;
    let for_value = value.map(|name| format!(" for value {name:?}")).unwrap_or_default();
    if pool.is_empty() {
        let message = match &input.request.select {
            Selection::Newest => {
                format!(
                    "path {path} has no recorded state of {source} to promote{for_value}: nothing was reconciled there"
                )
            }
            Selection::SourceRevision(commit) => {
                format!("no recorded state of {source} ran source commit {commit}")
            }
            Selection::StateRevision(commit)
                if input
                    .history
                    .iter()
                    .any(|state| state.desired_commit == *commit && !state.is_candidate()) =>
            {
                format!(
                    "{commit} is a state of {source} that ran an archived worktree snapshot, which cannot establish promotion evidence"
                )
            }
            Selection::StateRevision(commit) => format!("{commit} is not a recorded state of {source}"),
        };
        return Refusal::no_evidence(
            Finding::new(
                Code::PromoteNoEvidence,
                message,
                format!("run `nyl reconcile -e {source}` first, or list its recorded states with `nyl get states -e {source}`"),
            ),
            gaps,
        );
    }
    let which = match &input.request.select {
        Selection::Newest => "the newest recorded state".to_owned(),
        Selection::SourceRevision(commit) => format!("the newest recorded state at source commit {commit}"),
        Selection::StateRevision(commit) => format!("the state {commit}"),
    };
    let detail = gaps
        .iter()
        .map(|gap| gap.message.clone())
        .collect::<Vec<_>>()
        .join("; ");
    Refusal::no_evidence(
        Finding::new(
            Code::PromoteNoEvidence,
            format!(
                "no recorded state of {source} reaches evidence level {level} for path {path}{for_value}; {which} falls short: {detail}"
            ),
            fix_for(source, &gaps, level),
        ),
        gaps,
    )
}

fn fix_for(source: &EnvironmentName, gaps: &[Gap], level: &str) -> String {
    let kinds: BTreeSet<_> = gaps.iter().map(|gap| gap.kind as u8).collect();
    let has = |kind: GapKind| kinds.contains(&(kind as u8));
    let mut fixes = Vec::new();
    if has(GapKind::NoReceipt) {
        fixes.push(format!(
            "run `nyl reconcile -e {source}` until every unit the target selects has a current receipt"
        ));
    }
    if has(GapKind::Pending) {
        fixes.push(format!(
            "record the missing attestations (`nyl attest -e {source}` for what drivers attest, `nyl attest -e {source} [--unit <unit>] --name <name> --result pass` for external ones)"
        ));
    }
    if has(GapKind::Aged) {
        fixes.push(format!(
            "record them again with `nyl attest -e {source}`: attestations older than maxAttestationAge do not count"
        ));
    }
    if has(GapKind::Failing) {
        fixes.push(
            "fix what failed and record a newer passing attestation, or set it aside with `--ignore-attestation <unit>/<name>=<reason>` (or `environment/<name>=<reason>`)"
                .to_owned(),
        );
    }
    if level == "attested" {
        fixes.push("or promote below the path's level with `--evidence published`".to_owned());
    }
    fixes.join("; ")
}

/// Read the values and assemble the record.
// One linear assembly of the record's fields; splitting it would only scatter them.
#[allow(clippy::too_many_lines)]
fn build(
    input: &PromotionInput<'_>,
    ctx: &Context<'_>,
    chosen: &[Chosen<'_>],
    per_value: &BTreeMap<String, usize>,
    whole: bool,
) -> Result<Decision, Refusal> {
    // `from`: the newest state any value came from.
    let newest = chosen
        .iter()
        .min_by_key(|chosen| {
            input
                .history
                .iter()
                .position(|state| state.run == chosen.state.run)
                .unwrap_or(usize::MAX)
        })
        .expect("a promotion has a state");
    let state = newest.state;
    let mut promoted: BTreeMap<String, PromotedValue> = BTreeMap::new();
    let mut artifacts = Vec::new();
    for (name, value) in &input.path.spec.values {
        let from = if whole { state } else { chosen[per_value[name]].state };
        let read = values::read(input.source, from, name, &value.select, input.now).map_err(|error| match error {
            values::Unreadable::Invalid(finding) => Refusal::invalid(finding),
            values::Unreadable::Blocked { unit, message } => Refusal::no_evidence(
                Finding::new(
                    Code::PromoteNoEvidence,
                    message,
                    format!("run `nyl reconcile -e {} --unit {unit}` first", input.source),
                ),
                Vec::new(),
            ),
        })?;
        let mut read_value = read.value;
        if from.run != state.run {
            read_value.state = Some(PromotedState {
                run: from.run.clone(),
                desired_commit: from.desired_commit.clone(),
                observed_commit: from.observed_commit.clone(),
            });
        }
        artifacts.extend(read.artifact);
        promoted.insert(name.clone(), read_value);
    }

    let mut used: Vec<Used> = Vec::new();
    let mut ignored: Vec<IgnoreRequest> = Vec::new();
    for chosen in chosen {
        for item in &chosen.judgement.used {
            let duplicate = used.iter().any(|known| {
                known.scope == item.scope
                    && known.name == item.name
                    && known.attestation.execution_key == item.attestation.execution_key
            });
            if !duplicate {
                used.push(item.clone());
            }
        }
        for item in &chosen.judgement.ignored {
            if !ignored.contains(item) {
                ignored.push(item.clone());
            }
        }
    }
    used.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));
    ignored.sort_by(|a, b| (&a.scope, &a.name).cmp(&(&b.scope, &b.name)));

    let overridden: Vec<&Gap> = chosen
        .iter()
        .filter_map(|chosen| chosen.overridden.as_ref())
        .flatten()
        .collect();
    let below = chosen.iter().any(|chosen| chosen.overridden.is_some());
    let requested = &input.request.requested;
    let mut warnings = Vec::new();
    if below {
        warnings.push(Finding::new(
            Code::PromoteEvidenceOverride,
            format!(
                "path {} promotes below its evidence level: --evidence published took a state without the evidence {} requires: {}",
                input.path.metadata.name,
                ctx.level_name(),
                overridden
                    .iter()
                    .map(|gap| gap.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            "record the missing evidence and promote again without --evidence published, or keep the override: the record names it",
        ));
    }
    let record = PromotionRecord {
        path: input.path.name().map_err(|error| {
            Refusal::invalid(Finding::new(
                Code::PromoteSelectorInvalid,
                error.to_string(),
                "fix the PromotionPath's name",
            ))
        })?,
        sequence: input.sequence,
        from: PromotionFrom::Environment(EnvironmentFrom {
            environment: input.source.clone(),
            run: state.run.clone(),
            desired_commit: state.desired_commit.clone(),
            observed_commit: state.observed_commit.clone(),
        }),
        source_commit: if input.promotes_source {
            state.source_commit().cloned()
        } else {
            None
        },
        values: promoted,
        evidence: PromotionEvidence {
            level: if below { EvidenceLevel::Published } else { ctx.level() },
            attestations: used
                .iter()
                .map(|item| EvidenceAttestation {
                    scope: item.scope.clone(),
                    name: item.name.clone(),
                    result: item.attestation.result,
                    at: item.attestation.at,
                    details: item.attestation.details.clone(),
                })
                .collect(),
            override_: below.then(|| EvidenceOverride {
                reason: requested.reason.clone().unwrap_or_else(|| {
                    format!(
                        "--evidence published, given without --reason, took a state below the path's {} level",
                        ctx.level_name()
                    )
                }),
            }),
        },
        ignored_attestations: ignored
            .iter()
            .map(|item| IgnoredAttestation {
                scope: item.scope.clone(),
                name: item.name.clone(),
                reason: item.reason.clone(),
                by: requested.by.clone(),
            })
            .collect(),
        excluded_applications: input.path.spec.coverage.exclude_applications.clone(),
        superseded: Vec::new(),
        artifacts_verified: false,
        requested: requested.clone(),
    };
    let unused_ignores = input
        .request
        .ignore
        .iter()
        .filter(|ignore| !ignored.contains(ignore))
        .cloned()
        .collect();

    if let Some(existing) = input.target_records.get(&record.path) {
        let newest_sequence = input.target_records.values().map(|known| known.sequence).max();
        if existing.from == record.from
            && existing.source_commit == record.source_commit
            && existing.values == record.values
            && Some(existing.sequence) == newest_sequence
        {
            return Ok(Decision::Unchanged(Box::new(existing.clone())));
        }
    }
    Ok(Decision::Promote(Box::new(Promotion {
        record,
        warnings,
        artifacts,
        unused_ignores,
    })))
}
