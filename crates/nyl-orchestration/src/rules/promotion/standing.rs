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
//!   a required one is [`GapKind::Aged`], and an aged failing one no longer
//!   blocks.
//! - **Failing.** A failing attestation recorded for the state blocks it
//!   whether or not the path requires it ([`GapKind::Failing`]), until an
//!   `--ignore-attestation` entry sets it aside.
//! - **Publications (D8).** A state judged without an explicit choice, at
//!   `attested`, must also be what a publication unit's newest attestation
//!   shows running: when that attestation names another receipt the state is
//!   [`GapKind::NotRunning`], so a mix of revisions blocks without falling
//!   back to an older state whose attestations once passed. An explicitly
//!   chosen state uses the attestations recorded for it.
//!
//! A *publication unit*, for this rule, is a unit whose recorded attestations
//! carry the neutral deployment reading of the contract's
//! [Reading Argo CD](../../../../../design/promotion.md#reading-argo-cd)
//! (`details.deployments`, each with `state`, `health`, and the `publication`
//! it matched). No unit kind is named: any driver that records that reading
//! gets the rule, and the details are read only to explain a failure.

use std::collections::BTreeSet;

use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::promotion::EvidenceLevel;
use nyl_core::orchestration::state::{Attestation, AttestationResult};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::promotion::{PathEvidence, PromotionPathSpec};
use serde_json::Value;

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
    /// An attestation recorded for the state fails.
    Failing,
    /// A publication unit's newest attestation names another receipt, so the
    /// state is not what runs now.
    NotRunning,
    /// A publication's newest attestation shows Applications at different revisions.
    Mixed,
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
    /// value reads; `explicit` when the state was chosen by `--revision` or
    /// `--state-revision`.
    Source { explicit: bool },
    /// The environment and exactly these units (a value-only path's
    /// consumers).
    Units(&'a BTreeSet<UnitName>),
}

/// What the rule reads besides the state.
pub(super) struct Context<'a> {
    pub spec: &'a PromotionPathSpec,
    pub now: Timestamp,
    /// The units the target environment also selects.
    pub target_units: &'a BTreeSet<UnitName>,
    /// The units the path's value selectors read.
    pub value_units: &'a BTreeSet<UnitName>,
    /// Every recorded state of the source, newest first.
    pub history: &'a [RecordedState],
    pub ignores: &'a [IgnoreRequest],
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

    /// The newest attestation recorded for a publication unit in any state,
    /// when the unit is one ([`is_publication`]).
    fn newest_publication(&self, unit: &UnitName) -> Option<&Attestation> {
        let all = || {
            self.history
                .iter()
                .filter_map(|state| state.attestations.units.get(unit))
                .flat_map(|set| set.values())
        };
        if !all().any(is_publication) {
            return None;
        }
        all().max_by_key(|attestation| attestation.at)
    }
}

/// Whether an attestation carries the neutral deployment reading.
fn is_publication(attestation: &Attestation) -> bool {
    attestation.details.get("deployments").is_some_and(Value::is_array)
}

/// The Applications of a deployment reading that do not pass, and whether
/// they run different publications.
fn explain(attestation: &Attestation) -> (Vec<String>, bool) {
    let Some(deployments) = attestation.details.get("deployments").and_then(Value::as_array) else {
        return (Vec::new(), false);
    };
    let mut failing = Vec::new();
    let mut publications = BTreeSet::new();
    let mut mixed = false;
    for deployment in deployments {
        let text = |field: &str| deployment.get(field).and_then(Value::as_str);
        let id = text("id").unwrap_or("?");
        let state = text("state").unwrap_or("?");
        let health = text("health");
        if let Some(publication) = text("publication") {
            publications.insert(publication.to_owned());
        }
        if matches!(state, "other-publication" | "mixed") {
            mixed = true;
        }
        if state != "accepted" || health.is_some_and(|health| health != "Healthy") {
            failing.push(match health {
                Some(health) => format!("{id} {state}, {health}"),
                None => format!("{id} {state}"),
            });
        }
    }
    (failing, mixed || publications.len() > 1)
}

fn failing_gap(scope: &str, attestation: &Attestation) -> Gap {
    let name = &attestation.name;
    let (failing, mixed) = explain(attestation);
    let mut message = format!("attestation {scope}/{name} fails");
    if mixed {
        message.push_str(": the Applications run mixed revisions");
    }
    if !failing.is_empty() {
        message.push_str(" (");
        message.push_str(&failing.join("; "));
        message.push(')');
    }
    if let Some(reason) = attestation.reason.as_deref().filter(|reason| !reason.is_empty()) {
        message.push_str(": ");
        message.push_str(reason);
    }
    Gap {
        scope: scope.to_owned(),
        name: Some(name.clone()),
        kind: if mixed { GapKind::Mixed } else { GapKind::Failing },
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
        if ctx.aged(attestation) {
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
        Subject::Source { .. } => ctx
            .target_units
            .iter()
            .filter(|unit| desired.contains_key(*unit))
            .chain(ctx.value_units)
            .cloned()
            .collect(),
        Subject::Units(units) => (*units).clone(),
    };
    for unit in covered.iter().filter(|unit| !desired.contains_key(*unit)) {
        verdict.gap(
            unit.as_str(),
            None,
            GapKind::NoReceipt,
            format!("unit {unit} is not selected in this state, so it has no receipt"),
        );
    }
    let judged: BTreeSet<&UnitName> = match subject {
        // A failing attestation of any unit blocks the state.
        Subject::Source { .. } => desired.keys().collect(),
        Subject::Units(units) => units.iter().filter(|unit| desired.contains_key(*unit)).collect(),
    };
    for unit in judged {
        judge_unit(ctx, state, subject, level, unit, covered.contains(unit), &mut verdict);
    }
    judge_environment(ctx, state, level, &mut verdict);
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

fn judge_unit(
    ctx: &Context<'_>,
    state: &RecordedState,
    subject: &Subject<'_>,
    level: EvidenceLevel,
    unit: &UnitName,
    covered: bool,
    verdict: &mut Judgement,
) {
    let desired = &state.snapshot.desired[unit];
    let observed = state.snapshot.observed.get(unit);
    if !receipt_is_current(desired, observed) {
        if covered {
            verdict.gap(
                unit.as_str(),
                None,
                GapKind::NoReceipt,
                format!("unit {unit} has no current receipt in this state"),
            );
        }
        return;
    }
    let receipt = observed
        .and_then(|observed| observed.receipt.as_ref())
        .expect("a current receipt exists");
    let recorded = state.attestations.units.get(unit);
    let required = if covered {
        required_names(
            ctx.spec.attestations.units.get(unit.as_str()),
            evidence::declared(desired),
            level,
        )
    } else {
        BTreeSet::new()
    };
    let names: BTreeSet<&str> = recorded
        .into_iter()
        .flat_map(|set| set.keys())
        .map(String::as_str)
        .chain(required.iter().map(String::as_str))
        .collect();
    for name in names {
        let attestation = evidence::recorded_for(name, desired, receipt, recorded);
        verdict.classify(ctx, unit.as_str(), name, attestation, required.contains(name));
    }
    if let (Subject::Source { explicit: false }, EvidenceLevel::Attested, true) = (subject, level, covered) {
        if let Some(newest) = ctx.newest_publication(unit) {
            if newest.execution_key != receipt.execution_key.as_str() {
                verdict.gap(
                    unit.as_str(),
                    None,
                    GapKind::NotRunning,
                    format!(
                        "the newest attestation of {unit} ({}, recorded at {}) names another receipt, so this state is not what {unit} runs now",
                        newest.name,
                        newest.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    ),
                );
            }
        }
    }
}

fn judge_environment(ctx: &Context<'_>, state: &RecordedState, level: EvidenceLevel, verdict: &mut Judgement) {
    let declared: BTreeSet<String> = state
        .snapshot
        .environment
        .as_ref()
        .and_then(|record| record.spec.get("attestations"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
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
