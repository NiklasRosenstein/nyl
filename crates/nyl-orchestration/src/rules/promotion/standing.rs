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
//! What a publication unit's newest attestation says is running (decision D8)
//! needs a typed, driver-decided result that state does not record yet; see
//! the M6 plan's D8. The core never reads the driver's `details`.

use std::collections::BTreeSet;

use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::promotion::EvidenceLevel;
use nyl_core::orchestration::state::{Attestation, AttestationResult};
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
    /// An attestation recorded for the state fails.
    Failing,
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
    /// The environment and exactly these units (a value-only path's
    /// consumers) must be proven; a failing attestation of any other unit of
    /// the state still blocks it, until the per-Application judgement of
    /// the value-only specifics (PR 11) replaces that.
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
        Subject::Source => ctx
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
    // A failing attestation of any unit blocks the state.
    let judged: BTreeSet<&UnitName> = desired.keys().collect();
    for unit in judged {
        judge_unit(ctx, state, level, unit, covered.contains(unit), &mut verdict);
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
