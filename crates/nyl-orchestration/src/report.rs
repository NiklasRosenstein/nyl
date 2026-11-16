//! Typed reports of state operations (`state init`, `state move`,
//! `state forget`, and `lease break`), of `nyl promote`, and of the
//! promotion inspections (`get states`, `get promotion-candidates`, and
//! `get promotions`).
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection)
//! for the exit categories. Every output is rendered from the report: the
//! table, `--output json`, and the exit category, so they cannot disagree.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::promotion::{EvidenceLevel, PromotionFrom, PromotionRecord};
use nyl_core::orchestration::state::{Attestation, AttestationResult, SourceChoice};
use nyl_core::orchestration::time::Timestamp;
use serde::Serialize;
use serde_json::Value;

use crate::rules::promotion::{Gap, GapKind, RecordedState, StateEvaluation, ENVIRONMENT_SCOPE};

/// How a state operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The operation wrote what it set out to.
    Done,
    /// Nothing needed to change; nothing was written.
    Unchanged,
    /// The operation refused; its findings say why and how to proceed.
    Refused,
}

/// The report of one state operation on one environment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StateOperationReport {
    #[serde(rename = "apiVersion")]
    pub api_version: &'static str,
    pub kind: &'static str,
    pub environment: EnvironmentName,
    /// The operation's name, such as `state-init` or `lease-break`.
    pub operation: &'static str,
    pub outcome: Outcome,
    pub exit: u8,
    /// The transition commits written, by state ref side, or under `source`
    /// the commit of a `record: source` block on the entry worktree's branch.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub commits: BTreeMap<&'static str, CommitId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Finding>,
    /// The expiry an instance's record holds after the operation, when it has one.
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<nyl_core::orchestration::time::Timestamp>,
    /// Operation specifics, such as the location moved to.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

impl StateOperationReport {
    pub fn new(environment: EnvironmentName, operation: &'static str) -> Self {
        Self {
            api_version: crate::run_report::REPORT_API_VERSION,
            kind: "StateOperationReport",
            environment,
            operation,
            outcome: Outcome::Unchanged,
            exit: 0,
            commits: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            expires_at: None,
            details: BTreeMap::new(),
        }
    }

    /// Refuse with `finding`: exit 2 while another run holds the lease, a
    /// broken lease waits for its import (`NYL-LEASE-BROKEN`), or an
    /// instance limit or renewal holds the operation back, so a pipeline can
    /// wait and retry, and exit 1 for every other refusal.
    #[must_use]
    pub fn refused(mut self, finding: Finding) -> Self {
        self.exit = if matches!(
            finding.code,
            Code::LeaseHeld
                | Code::LeaseBroken
                | Code::InstanceLimit
                | Code::InstanceRenewed
                | Code::InstanceRemovalPending
                | Code::PromotionMissing
                | Code::PromoteNoEvidence
                | Code::PromoteSourceRejected
                | Code::PromoteSupersedes
        ) {
            2
        } else {
            1
        };
        self.outcome = Outcome::Refused;
        self.errors.push(finding);
        self
    }

    #[must_use]
    pub fn done(mut self) -> Self {
        self.outcome = Outcome::Done;
        self.exit = 0;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_held_lease_waits_and_other_refusals_fail() {
        let report = || StateOperationReport::new(EnvironmentName::new("dev").unwrap(), "state-init");
        assert_eq!(report().refused(Finding::new(Code::LeaseHeld, "held", "wait")).exit, 2);
        assert_eq!(
            report()
                .refused(Finding::new(Code::StateMissing, "missing", "init"))
                .exit,
            1
        );
        assert_eq!(report().done().exit, 0);
    }
}

/// The report of one `nyl promote`: the table, `--output json`, the exit
/// category, and the transition commit's summary are all rendered from it.
///
/// Contract: [Exit categories](../../../../design/promotion.md#exit-categories).
/// `record` is the PromotionRecord that was written, that already stood
/// (`outcome: unchanged`), or, with `--dry-run`, that would be written (then
/// `outcome: done` with `dryRun: true`, and no commits).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromoteReport {
    #[serde(rename = "apiVersion")]
    pub api_version: &'static str,
    pub kind: &'static str,
    /// The PromotionPath.
    pub path: String,
    /// The source environment, which is only read.
    pub source: EnvironmentName,
    /// The target environment, whose desired state holds the record.
    pub environment: EnvironmentName,
    pub outcome: Outcome,
    #[serde(rename = "dryRun")]
    pub dry_run: bool,
    pub exit: u8,
    /// The transition commits written, by state ref side, or under `source`
    /// the commit of a `record: source` block on the entry worktree's branch.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub commits: BTreeMap<&'static str, CommitId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<nyl_core::orchestration::promotion::PromotionRecord>,
    /// What keeps the newest candidate below the path's evidence level, for a refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<PromoteGap>,
    /// The commits of other paths' records the promotion would replace,
    /// for a `NYL-PROMOTE-SUPERSEDES` refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<PromoteMissing>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Finding>,
}

/// One reason a state is below the evidence level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromoteGap {
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
}

/// The commits of one record on another path that the promoted source
/// commit or value lacks (`NYL-PROMOTE-SUPERSEDES`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromoteMissing {
    /// The other path.
    pub path: String,
    /// The sequence of its record.
    pub sequence: u64,
    /// The repository, by the name a `--supersede` entry qualifies it with.
    pub repository: String,
    /// What must contain the commits: `source` (the target's source commit) or value names.
    pub items: Vec<String>,
    /// The promoted commit compared.
    pub commit: CommitId,
    /// The other record's commit.
    pub record_commit: CommitId,
    /// A range that covers the missing commits, for `--supersede <range>=<reason>`.
    pub range: String,
    /// The missing commits, oldest first.
    pub commits: Vec<PromoteMissingCommit>,
}

/// One missing commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromoteMissingCommit {
    pub commit: CommitId,
    /// The first line of its message.
    pub subject: String,
}

impl PromoteReport {
    pub fn new(path: &str, source: EnvironmentName, environment: EnvironmentName, dry_run: bool) -> Self {
        Self {
            api_version: crate::run_report::REPORT_API_VERSION,
            kind: "PromoteReport",
            path: path.to_owned(),
            source,
            environment,
            outcome: Outcome::Unchanged,
            dry_run,
            exit: 0,
            commits: BTreeMap::new(),
            record: None,
            gaps: Vec::new(),
            missing: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Refuse with `finding`: exit 2 when nothing failed but evidence, a
    /// containing commit, or the lease is missing, exit 1 otherwise.
    #[must_use]
    pub fn refused(mut self, finding: Finding) -> Self {
        self.exit = crate::run_report::ExitCategory::of_finding(&finding).code();
        self.outcome = Outcome::Refused;
        self.errors.push(finding);
        self
    }

    /// The table `nyl promote` prints.
    pub fn table(&self) -> String {
        let mut text = String::new();
        let status = match (self.outcome, self.dry_run) {
            (Outcome::Refused, _) => "refused",
            (Outcome::Done, true) => "dry run, nothing written",
            (Outcome::Unchanged, true) => "dry run, unchanged",
            (Outcome::Done, false) => "promoted",
            (Outcome::Unchanged, false) => "unchanged, nothing written",
        };
        let _ = writeln!(
            text,
            "promote {}: {} -> {}: {status}",
            self.path, self.source, self.environment
        );
        if let Some(record) = &self.record {
            let _ = writeln!(text, "  sequence: {}", record.sequence);
            if let nyl_core::orchestration::promotion::PromotionFrom::Environment(from) = &record.from {
                let _ = writeln!(
                    text,
                    "  from: {} run {} (desired {})",
                    from.environment, from.run, from.desired_commit
                );
            }
            if let Some(commit) = &record.source_commit {
                let _ = writeln!(text, "  source commit: {commit}");
            }
            for (name, value) in &record.values {
                let shown = value
                    .value
                    .as_str()
                    .map_or_else(|| value.value.to_string(), str::to_owned);
                let _ = writeln!(text, "  value {name}: {shown}");
            }
            let level = match record.evidence.level {
                nyl_core::orchestration::promotion::EvidenceLevel::Published => "published",
                nyl_core::orchestration::promotion::EvidenceLevel::Attested => "attested",
            };
            let _ = writeln!(
                text,
                "  evidence: {level}{}",
                if record.evidence.override_.is_some() {
                    " (override)"
                } else {
                    ""
                }
            );
            for attestation in &record.evidence.attestations {
                let _ = writeln!(text, "    {}/{}", attestation.scope, attestation.name);
            }
            let _ = writeln!(text, "  artifacts verified: {}", record.artifacts_verified);
        }
        for gap in &self.gaps {
            let _ = writeln!(text, "  missing: {}", gap.message);
        }
        for missing in &self.missing {
            let _ = writeln!(
                text,
                "  missing from {} #{} ({}), --supersede {}:",
                missing.path,
                missing.sequence,
                missing.items.join(", "),
                missing.range
            );
            for commit in &missing.commits {
                let _ = writeln!(text, "    {} {}", &commit.commit.as_str()[..12], commit.subject);
            }
        }
        if let Some(record) = self.record.as_ref().filter(|record| !record.superseded.is_empty()) {
            for superseded in &record.superseded {
                let _ = writeln!(
                    text,
                    "  superseded {}: {}",
                    &superseded.commit.as_str()[..12],
                    superseded.reason
                );
            }
        }
        for (side, commit) in &self.commits {
            let _ = writeln!(text, "  {side}: {commit}");
        }
        text
    }
}

/// The `apiVersion` the promotion inspection reports share with every other report.
fn api_version() -> &'static str {
    crate::run_report::REPORT_API_VERSION
}

fn level_name(level: Option<EvidenceLevel>) -> &'static str {
    match level {
        Some(EvidenceLevel::Attested) => "attested",
        Some(EvidenceLevel::Published) => "published",
        None => "none",
    }
}

fn shown(value: &Value) -> String {
    value.as_str().map_or_else(|| value.to_string(), str::to_owned)
}

fn at(timestamp: &Timestamp) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The report of `nyl get states -e <env>`: the environment's recorded
/// states, newest first, with the evidence recorded for each.
///
/// Contract: [Promoting an environment's source](../../../../design/promotion.md#promoting-an-environments-source),
/// "Inspection". A state is one transition commit pair of a `reconcile` or
/// `recover` run ([`crate::rules::promotion::recorded_states`]); its
/// attestations are those recorded for it however much later, as `nyl
/// promote` reads them. The text and `-o json` are both rendered from it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatesReport {
    #[serde(rename = "apiVersion")]
    pub api_version: &'static str,
    pub kind: &'static str,
    /// The environment whose states are listed.
    pub environment: EnvironmentName,
    /// Its recorded states, newest first.
    pub states: Vec<StateReport>,
    /// 0 when the state was read, 1 when it could not be (see `errors`).
    pub exit: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Finding>,
}

/// One recorded state.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateReport {
    /// The `Nyl-Run-Id` of the run that recorded it.
    pub run: RunId,
    /// The state commit on the desired ref: what `--state-revision` names.
    pub desired_commit: CommitId,
    /// The run's commit on the observed ref, when it wrote one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_commit: Option<CommitId>,
    /// The source commit the run rendered its units at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    /// How that source commit was chosen; a worktree snapshot is never a
    /// promotion candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_chosen_by: Option<SourceChoice>,
    /// Each unit of the state and whether it has a current receipt there.
    pub units: Vec<StateUnitReport>,
    /// The attestations recorded for the state: environment-wide ones
    /// first, then each unit's, by name.
    pub attestations: Vec<StateAttestationReport>,
}

/// One unit of a recorded state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateUnitReport {
    /// The unit.
    pub name: UnitName,
    /// Its kind, as the desired document names it.
    pub kind: String,
    /// Whether the unit has a current receipt in the state.
    pub receipt: bool,
    /// The artifacts the receipt lists, by name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

/// One attestation recorded for a state.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateAttestationReport {
    /// The unit, or `environment` for an environment-wide attestation.
    pub scope: String,
    /// The attestation's name.
    pub name: String,
    /// `pass` or `fail`.
    pub result: AttestationResult,
    /// When it was recorded.
    pub at: Timestamp,
    /// The reason it was recorded with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl StateReport {
    /// The report of one recorded state.
    pub fn of(state: &RecordedState) -> Self {
        let snapshot = &state.snapshot;
        let units = snapshot
            .desired
            .iter()
            .map(|(name, desired)| {
                let receipt = crate::rules::freshness::receipt_is_current(desired, snapshot.observed.get(name));
                StateUnitReport {
                    name: name.clone(),
                    kind: desired.unit.kind.clone(),
                    receipt,
                    artifacts: if receipt {
                        snapshot
                            .artifacts
                            .get(name)
                            .map(|set| set.keys().cloned().collect())
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect();
        let attestation = |scope: &str, name: &str, attestation: &Attestation| StateAttestationReport {
            scope: scope.to_owned(),
            name: name.to_owned(),
            result: attestation.result,
            at: attestation.at,
            reason: attestation.reason.clone(),
        };
        let attestations = state
            .attestations
            .environment
            .iter()
            .map(|(name, recorded)| attestation(ENVIRONMENT_SCOPE, name, recorded))
            .chain(state.attestations.units.iter().flat_map(|(unit, set)| {
                set.iter()
                    .map(move |(name, recorded)| attestation(unit.as_str(), name, recorded))
            }))
            .collect();
        Self {
            run: state.run.clone(),
            desired_commit: state.desired_commit.clone(),
            observed_commit: state.observed_commit.clone(),
            source_commit: state.source_commit().cloned(),
            source_chosen_by: snapshot
                .environment
                .as_ref()
                .map(|record| record.source_chosen_by.clone()),
            units,
            attestations,
        }
    }
}

impl StatesReport {
    pub fn new(environment: EnvironmentName, states: &[RecordedState]) -> Self {
        Self {
            api_version: api_version(),
            kind: "StatesReport",
            environment,
            states: states.iter().map(StateReport::of).collect(),
            exit: 0,
            errors: Vec::new(),
        }
    }

    /// The report of a read that failed: exit 1 with the finding.
    #[must_use]
    pub fn refused(environment: EnvironmentName, finding: Finding) -> Self {
        Self {
            exit: 1,
            errors: vec![finding],
            ..Self::new(environment, &[])
        }
    }

    /// The text `nyl get states` prints: one block per state, newest first.
    pub fn table(&self) -> String {
        let mut text = String::new();
        if self.states.is_empty() && self.errors.is_empty() {
            let _ = writeln!(text, "{}: no recorded states", self.environment);
        }
        for state in &self.states {
            let _ = writeln!(text, "{}", state.desired_commit);
            let _ = writeln!(text, "  run:          {}", state.run);
            if let Some(commit) = &state.observed_commit {
                let _ = writeln!(text, "  observed:     {commit}");
            }
            if let Some(commit) = &state.source_commit {
                let _ = writeln!(text, "  source:       {commit}");
            }
            let current: Vec<&str> = state
                .units
                .iter()
                .filter(|unit| unit.receipt)
                .map(|unit| unit.name.as_str())
                .collect();
            let missing: Vec<&str> = state
                .units
                .iter()
                .filter(|unit| !unit.receipt)
                .map(|unit| unit.name.as_str())
                .collect();
            let _ = write!(text, "  receipts:     {}", list(&current));
            if !missing.is_empty() {
                let _ = write!(text, "; none for {}", missing.join(", "));
            }
            text.push('\n');
            if state.attestations.is_empty() {
                let _ = writeln!(text, "  attestations: none");
            } else {
                let _ = writeln!(text, "  attestations:");
                for attestation in &state.attestations {
                    let _ = write!(
                        text,
                        "    {}/{} {} at {}",
                        attestation.scope,
                        attestation.name,
                        result_name(attestation.result),
                        at(&attestation.at)
                    );
                    if let Some(reason) = &attestation.reason {
                        let _ = write!(text, ": {reason}");
                    }
                    text.push('\n');
                }
            }
        }
        text
    }
}

fn list(items: &[&str]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

fn result_name(result: AttestationResult) -> &'static str {
    match result {
        AttestationResult::Pass => "pass",
        AttestationResult::Fail => "fail",
    }
}

/// The report of `nyl get promotion-candidates <path>`: the source's recorded
/// states evaluated against the path, newest first, the level each reaches or
/// what it lacks, the values it would carry, and what `nyl promote` would do
/// now.
///
/// Contract: [Promoting an environment's source](../../../../design/promotion.md#promoting-an-environments-source),
/// "Inspection". Every judgement is `nyl promote`'s own: the levels come
/// from [`crate::rules::promotion::evaluate`] and the choice from
/// [`crate::rules::promotion::decide`] over the same reading of state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromotionCandidatesReport {
    #[serde(rename = "apiVersion")]
    pub api_version: &'static str,
    pub kind: &'static str,
    /// The PromotionPath.
    pub path: String,
    /// The source environment whose states are listed.
    pub source: EnvironmentName,
    /// The target environment.
    pub environment: EnvironmentName,
    /// The evidence level the path requires.
    pub evidence: EvidenceLevel,
    pub candidates: Vec<CandidateReport>,
    /// Why `nyl promote` without flags would refuse, when it would.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Finding>,
    pub exit: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Finding>,
}

/// One candidate state.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
// Independent facts about one state, each a JSON key scripts read.
#[allow(clippy::struct_excessive_bools)]
pub struct CandidateReport {
    /// The `Nyl-Run-Id` of the run that recorded the state.
    pub run: RunId,
    /// The state commit on the source's desired ref.
    pub desired_commit: CommitId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_commit: Option<CommitId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    /// The highest level the state reaches, up to the path's: for a
    /// value-only path, the highest any value reaches.
    pub level: CandidateLevel,
    /// Whether `nyl promote` could take the state: it reaches the path's
    /// level, or, on a value-only path, some value's consumer does
    /// (`byValue`). A `selected` state is always eligible.
    pub eligible: bool,
    /// Whether `nyl promote` without flags would take this state now.
    pub selected: bool,
    /// Whether the path's record in the target names this state: what the
    /// target was last promoted to on this path.
    pub promoted: bool,
    /// Whether the target runs this state now: its last run applied the
    /// path's record that names it, and no newer record waits.
    pub running: bool,
    /// For a value-only path, each value judged for its consumer.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_value: BTreeMap<String, ValueCandidate>,
    /// What keeps the state below the path's level.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<MissingEvidence>,
    /// The values the state would carry, by name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, Value>,
}

/// The level a candidate reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateLevel {
    Attested,
    Published,
    /// Below `published`.
    None,
}

impl CandidateLevel {
    fn of(level: Option<EvidenceLevel>) -> Self {
        match level {
            Some(EvidenceLevel::Attested) => Self::Attested,
            Some(EvidenceLevel::Published) => Self::Published,
            None => Self::None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Attested => "attested",
            Self::Published => "published",
            Self::None => "none",
        }
    }
}

/// One value of a value-only path in one candidate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ValueCandidate {
    /// The level the value's consumer reaches, up to the path's.
    pub level: CandidateLevel,
    /// Whether it reaches the path's level, so the state can supply the value.
    pub eligible: bool,
}

/// Why a candidate falls short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissingReason {
    /// A covered unit has no current receipt in the state.
    NoReceipt,
    /// A required attestation is not recorded for the state.
    Pending,
    /// A required attestation is older than `maxAttestationAge`.
    Aged,
    /// An attestation recorded for the state fails.
    Failing,
}

/// One reason a candidate falls short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingEvidence {
    /// The unit; absent for an environment-wide attestation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The attestation, for a gap about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attestation: Option<String>,
    /// Why it falls short.
    pub reason: MissingReason,
    pub message: String,
}

impl MissingEvidence {
    fn of(gap: &Gap) -> Self {
        Self {
            unit: (gap.scope != ENVIRONMENT_SCOPE).then(|| gap.scope.clone()),
            attestation: gap.name.clone(),
            reason: match gap.kind {
                GapKind::NoReceipt => MissingReason::NoReceipt,
                GapKind::Pending => MissingReason::Pending,
                GapKind::Aged => MissingReason::Aged,
                GapKind::Failing => MissingReason::Failing,
            },
            message: gap.message.clone(),
        }
    }
}

impl PromotionCandidatesReport {
    pub fn new(path: &str, source: EnvironmentName, environment: EnvironmentName, evidence: EvidenceLevel) -> Self {
        Self {
            api_version: api_version(),
            kind: "PromotionCandidatesReport",
            path: path.to_owned(),
            source,
            environment,
            evidence,
            candidates: Vec::new(),
            refusal: None,
            exit: 0,
            errors: Vec::new(),
        }
    }

    /// The report of a read that failed: exit 1 with the finding.
    #[must_use]
    pub fn refused(mut self, finding: Finding) -> Self {
        self.exit = 1;
        self.errors.push(finding);
        self
    }

    /// The candidates of `evaluations`, marking those `selected` (the
    /// states the promotion would take, by desired commit), `promoted`
    /// (those the path's current record names), and `running` (those the
    /// record the target's last run applied names).
    #[must_use]
    pub fn with_candidates(
        mut self,
        evaluations: Vec<StateEvaluation>,
        selected: &[CommitId],
        promoted: &[CommitId],
        running: &[CommitId],
    ) -> Self {
        self.candidates = evaluations
            .into_iter()
            .map(|evaluation| CandidateReport {
                selected: selected.contains(&evaluation.desired_commit),
                promoted: promoted.contains(&evaluation.desired_commit),
                running: running.contains(&evaluation.desired_commit),
                level: CandidateLevel::of(evaluation.level),
                eligible: evaluation.eligible,
                by_value: evaluation
                    .by_value
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.clone(),
                            ValueCandidate {
                                level: CandidateLevel::of(value.level),
                                eligible: value.eligible,
                            },
                        )
                    })
                    .collect(),
                missing: evaluation.gaps.iter().map(MissingEvidence::of).collect(),
                run: evaluation.run,
                desired_commit: evaluation.desired_commit,
                observed_commit: evaluation.observed_commit,
                source_commit: evaluation.source_commit,
                values: evaluation.values,
            })
            .collect();
        self
    }

    /// The text `nyl get promotion-candidates` prints.
    pub fn table(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(
            text,
            "promotion-candidates {}: {} -> {}, evidence {}",
            self.path,
            self.source,
            self.environment,
            level_name(Some(self.evidence))
        );
        if let Some(refusal) = &self.refusal {
            let _ = writeln!(
                text,
                "  nyl promote would refuse: {}: {}",
                refusal.code, refusal.message
            );
        } else if let Some(selected) = self.candidates.iter().find(|candidate| candidate.selected) {
            let _ = writeln!(text, "  nyl promote would take {}", selected.desired_commit);
        }
        if self.candidates.is_empty() && self.errors.is_empty() {
            let _ = writeln!(text, "  {} has no recorded states", self.source);
        }
        for candidate in &self.candidates {
            let mut marks = Vec::new();
            if candidate.selected {
                marks.push("selected");
            }
            if candidate.promoted {
                marks.push("promoted");
            }
            if candidate.running {
                marks.push("running");
            }
            let marks = if marks.is_empty() {
                String::new()
            } else {
                format!(" ({})", marks.join(", "))
            };
            let _ = writeln!(text, "{} {}{marks}", candidate.desired_commit, candidate.level.as_str());
            let _ = writeln!(text, "  run:     {}", candidate.run);
            if let Some(commit) = &candidate.source_commit {
                let _ = writeln!(text, "  source:  {commit}");
            }
            for (name, value) in &candidate.values {
                let _ = write!(text, "  value {name}: {}", shown(value));
                if let Some(judged) = candidate.by_value.get(name) {
                    let _ = write!(
                        text,
                        " ({}{})",
                        judged.level.as_str(),
                        if judged.eligible { "" } else { ", not eligible" }
                    );
                }
                text.push('\n');
            }
            for missing in &candidate.missing {
                let _ = writeln!(text, "  missing: {}", missing.message);
            }
        }
        text
    }
}

/// The report of `nyl get promotions -e <env>`: the PromotionRecords in the
/// environment's desired state, newest first, which one the last run
/// applied, and every promotion its desired history records.
///
/// Contract: [PromotionRecord](../../../../design/promotion.md#promotionrecord)
/// ("Each promotion replaces the path's record; Git history keeps earlier
/// ones") and [Several paths into one environment](../../../../design/promotion.md#several-paths-into-one-environment).
/// The history is read from the summaries of the `promote` transition
/// commits, after one sync of the state refs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PromotionsReport {
    #[serde(rename = "apiVersion")]
    pub api_version: &'static str,
    pub kind: &'static str,
    pub environment: EnvironmentName,
    /// The record the last run applied to the environment's source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<AppliedPromotion>,
    /// The record the next run takes its source commit from, when the
    /// environment's source is promoted: the newest on its source paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<AppliedPromotion>,
    /// Each `fromPromotion` binding of the environment's units and the record
    /// that supplies it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<BindingSupplier>,
    /// The current record of each path, highest sequence first.
    pub promotions: Vec<PromotionRecord>,
    /// Every promotion into the environment, newest first.
    pub history: Vec<PromotionHistoryEntry>,
    pub exit: u8,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<Finding>,
}

/// A `fromPromotion` binding and the record it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingSupplier {
    /// The unit whose spec holds the binding; a DeploymentTarget's
    /// `releaseInputs` are in its publication unit's spec.
    pub unit: UnitName,
    /// Where in the unit's spec.
    pub pointer: String,
    /// The promoted value it reads.
    pub value: String,
    /// The path the binding names; absent for a path-less binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The record that supplies it: path and sequence. Absent when no record
    /// supplies it yet, and the unit waits (`NYL-PROMOTION-MISSING`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplier: Option<AppliedPromotion>,
}

/// The record whose source commit the environment's last run applied, as
/// its `sourceChosenBy` names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedPromotion {
    /// The PromotionPath that supplied the source commit.
    pub path: String,
    /// The sequence of the record the run applied.
    pub sequence: u64,
}

/// The source state a `promote` commit's summary names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryFrom {
    /// The source environment.
    pub environment: EnvironmentName,
    /// The `Nyl-Run-Id` of the run that recorded the state.
    pub run: RunId,
    /// The state's commit on the source's desired ref.
    pub desired_commit: CommitId,
}

/// One promotion as its transition commit's summary records it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionHistoryEntry {
    /// The `promote` transition commit on the desired ref.
    pub commit: CommitId,
    /// The PromotionPath promoted along.
    pub path: String,
    /// The record's sequence.
    pub sequence: u64,
    /// The source commit the record promoted, for a path that promotes the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    /// The source state, as the summary names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<HistoryFrom>,
    /// Who asked, from the commit's `Nyl-Requested-By`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    /// Why, from the commit's `Nyl-Reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PromotionsReport {
    pub fn new(environment: EnvironmentName) -> Self {
        Self {
            api_version: api_version(),
            kind: "PromotionsReport",
            environment,
            applied: None,
            source: None,
            bindings: Vec::new(),
            promotions: Vec::new(),
            history: Vec::new(),
            exit: 0,
            errors: Vec::new(),
        }
    }

    /// The report of a read that failed: exit 1 with the finding.
    #[must_use]
    pub fn refused(mut self, finding: Finding) -> Self {
        self.exit = 1;
        self.errors.push(finding);
        self
    }

    /// The text `nyl get promotions` prints.
    pub fn table(&self) -> String {
        let mut text = String::new();
        if self.promotions.is_empty() && self.errors.is_empty() {
            let _ = writeln!(text, "{}: no promotion records", self.environment);
        }
        for record in &self.promotions {
            let applied = self
                .applied
                .as_ref()
                .is_some_and(|applied| applied.path == record.path.as_str() && applied.sequence == record.sequence);
            let _ = writeln!(
                text,
                "{} #{}{}",
                record.path,
                record.sequence,
                if applied { " (applied)" } else { "" }
            );
            if let PromotionFrom::Environment(from) = &record.from {
                let _ = writeln!(
                    text,
                    "  from:     {} run {} (desired {})",
                    from.environment, from.run, from.desired_commit
                );
            }
            if let Some(commit) = &record.source_commit {
                let _ = writeln!(text, "  source:   {commit}");
            }
            for (name, value) in &record.values {
                let _ = writeln!(text, "  value {name}: {}", shown(&value.value));
            }
            let _ = writeln!(
                text,
                "  evidence: {}{}",
                level_name(Some(record.evidence.level)),
                if record.evidence.override_.is_some() {
                    " (override)"
                } else {
                    ""
                }
            );
            let _ = write!(
                text,
                "  requested by {} at {}",
                record.requested.by,
                at(&record.requested.at)
            );
            if let Some(reason) = &record.requested.reason {
                let _ = write!(text, ": {reason}");
            }
            text.push('\n');
        }
        if let Some(source) = &self.source {
            let _ = writeln!(text, "source from {} #{}", source.path, source.sequence);
        }
        if !self.bindings.is_empty() {
            let _ = writeln!(text, "bindings:");
            for binding in &self.bindings {
                let supplier = binding.supplier.as_ref().map_or_else(
                    || "no record yet".to_owned(),
                    |supplier| format!("{} #{}", supplier.path, supplier.sequence),
                );
                let _ = writeln!(
                    text,
                    "  {} {} {}: {supplier}",
                    binding.unit, binding.pointer, binding.value
                );
            }
        }
        if !self.history.is_empty() {
            let _ = writeln!(text, "history, newest first:");
            for entry in &self.history {
                let _ = write!(text, "  #{} {} {}", entry.sequence, entry.path, entry.commit);
                if let Some(reason) = &entry.reason {
                    let _ = write!(text, ": {reason}");
                }
                text.push('\n');
            }
        }
        text
    }
}

/// Golden output of the promotion inspection reports: the text each `nyl
/// get` form prints and the JSON `-o json` prints, from fixed reports, so a
/// change to either is a deliberate one. Catches a renamed JSON key that
/// scripts and the scenarios read, and text that drops what a reader needs
/// (the full desired commit `--state-revision` takes, what a state lacks).
#[cfg(test)]
mod promotion_report_tests {
    use std::collections::BTreeMap;

    use nyl_core::orchestration::ids::PromotionPathName;
    use nyl_core::orchestration::promotion::{
        EnvironmentFrom, EvidenceAttestation, PromotedUnit, PromotedValue, PromotionEvidence, PromotionRequest,
        PromotionSelect,
    };
    use nyl_core::orchestration::state::{
        ApprovalIdentity, AttestationKind, EnvironmentRecord, EnvironmentRecordKind, StateV1,
    };
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{artifact, commit, desired, listed, name, observed, receipt, run, COMMIT, RUN_1};
    use crate::rules::promotion::StateAttestations;

    const DESIRED: &str = "d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0";
    const OBSERVED: &str = "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b";
    const AT: &str = "2026-09-25T12:00:00Z";

    fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    fn prod() -> EnvironmentName {
        EnvironmentName::new("prod").unwrap()
    }

    /// One state of dev: web-image built, kubernetes without a receipt,
    /// and the environment's `qa` recorded for it.
    fn state() -> RecordedState {
        let mut snapshot = nyl_core::orchestration::ports::EnvironmentSnapshot::default();
        let image = artifact(
            "web-image",
            "image",
            "ContainerImage",
            "web",
            json!({"reference": "registry.example.com/web@sha256:4f0c"}),
        );
        snapshot
            .desired
            .insert(name("web-image"), desired("web-image", "OciImage", json!({}), "web"));
        snapshot.observed.insert(
            name("web-image"),
            observed(
                "web-image",
                "OciImage",
                Some(receipt("web", json!({}), vec![listed(&image)])),
            ),
        );
        snapshot
            .artifacts
            .insert(name("web-image"), BTreeMap::from([("image".to_owned(), image)]));
        snapshot.desired.insert(
            name("kubernetes"),
            desired("kubernetes", "KubernetesPublication", json!({}), "kube"),
        );
        snapshot.environment = Some(EnvironmentRecord {
            api_version: StateV1,
            kind: EnvironmentRecordKind,
            environment: dev(),
            spec: json!({"attestations": ["qa"]}),
            source_commit: commit(COMMIT),
            source_chosen_by: SourceChoice::Entry,
            applied_promotion_sequence: None,
            ownership: BTreeMap::new(),
        });
        let mut attestations = StateAttestations::default();
        attestations.environment.insert(
            "qa".to_owned(),
            Attestation {
                api_version: StateV1,
                kind: AttestationKind,
                name: "qa".to_owned(),
                unit: None,
                execution_key: DESIRED.to_owned(),
                result: AttestationResult::Pass,
                at: AT.parse().unwrap(),
                source: Value::Null,
                reason: Some("QA signed off".to_owned()),
                details: Value::Null,
            },
        );
        RecordedState {
            run: run(RUN_1),
            desired_commit: commit(DESIRED),
            observed_commit: Some(commit(OBSERVED)),
            snapshot,
            attestations,
        }
    }

    #[test]
    fn test_states_report_golden() {
        let report = StatesReport::new(dev(), &[state()]);
        assert_eq!(
            report.table(),
            "d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0
  run:          0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90
  observed:     0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b
  source:       3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a
  receipts:     web-image; none for kubernetes
  attestations:
    environment/qa pass at 2026-09-25T12:00:00Z: QA signed off
"
        );
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({
                "apiVersion": crate::run_report::REPORT_API_VERSION,
                "kind": "StatesReport",
                "environment": "dev",
                "exit": 0,
                "states": [{
                    "run": RUN_1,
                    "desiredCommit": DESIRED,
                    "observedCommit": OBSERVED,
                    "sourceCommit": COMMIT,
                    "sourceChosenBy": {"by": "entry"},
                    "units": [
                        {"name": "kubernetes", "kind": "KubernetesPublication", "receipt": false},
                        {"name": "web-image", "kind": "OciImage", "receipt": true, "artifacts": ["image"]},
                    ],
                    "attestations": [
                        {"scope": "environment", "name": "qa", "result": "pass", "at": AT, "reason": "QA signed off"},
                    ],
                }],
            })
        );
        assert_eq!(StatesReport::new(dev(), &[]).table(), "dev: no recorded states\n");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One golden text and JSON, written out in full.
    fn test_promotion_candidates_report_golden() {
        let newer = StateEvaluation {
            run: run("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91"),
            desired_commit: commit("d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1"),
            observed_commit: None,
            source_commit: Some(commit("41f0000000000000000000000000000000000000")),
            level: None,
            eligible: false,
            gaps: vec![Gap {
                scope: "kubernetes".to_owned(),
                name: None,
                kind: GapKind::NoReceipt,
                message: "unit kubernetes has no current receipt in this state".to_owned(),
            }],
            values: BTreeMap::new(),
            by_value: BTreeMap::new(),
        };
        let older = StateEvaluation {
            run: run(RUN_1),
            desired_commit: commit(DESIRED),
            observed_commit: Some(commit(OBSERVED)),
            source_commit: Some(commit(COMMIT)),
            level: Some(EvidenceLevel::Published),
            eligible: true,
            gaps: Vec::new(),
            values: BTreeMap::from([("webImage".to_owned(), json!("registry.example.com/web@sha256:4f0c"))]),
            by_value: BTreeMap::from([(
                "webImage".to_owned(),
                crate::rules::promotion::ValueEvaluation {
                    level: Some(EvidenceLevel::Published),
                    eligible: true,
                },
            )]),
        };
        let report = PromotionCandidatesReport::new("dev-to-prod", dev(), prod(), EvidenceLevel::Published)
            .with_candidates(
                vec![newer, older],
                &[commit(DESIRED)],
                &[commit(DESIRED)],
                &[commit(DESIRED)],
            );
        assert_eq!(
            report.table(),
            "promotion-candidates dev-to-prod: dev -> prod, evidence published
  nyl promote would take d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0
d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1 none
  run:     0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91
  source:  41f0000000000000000000000000000000000000
  missing: unit kubernetes has no current receipt in this state
d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0 published (selected, promoted, running)
  run:     0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90
  source:  3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a
  value webImage: registry.example.com/web@sha256:4f0c (published)
"
        );
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({
                "apiVersion": crate::run_report::REPORT_API_VERSION,
                "kind": "PromotionCandidatesReport",
                "path": "dev-to-prod",
                "source": "dev",
                "environment": "prod",
                "evidence": "published",
                "exit": 0,
                "candidates": [
                    {
                        "run": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91",
                        "desiredCommit": "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1",
                        "sourceCommit": "41f0000000000000000000000000000000000000",
                        "level": "none",
                        "eligible": false,
                        "selected": false,
                        "promoted": false,
                        "running": false,
                        "missing": [{
                            "unit": "kubernetes",
                            "reason": "no-receipt",
                            "message": "unit kubernetes has no current receipt in this state",
                        }],
                    },
                    {
                        "run": RUN_1,
                        "desiredCommit": DESIRED,
                        "observedCommit": OBSERVED,
                        "sourceCommit": COMMIT,
                        "level": "published",
                        "eligible": true,
                        "selected": true,
                        "promoted": true,
                        "running": true,
                        "byValue": {"webImage": {"level": "published", "eligible": true}},
                        "values": {"webImage": "registry.example.com/web@sha256:4f0c"},
                    },
                ],
            })
        );
        // A refusal is what `nyl promote` would say, not an error of the read.
        let mut refused = PromotionCandidatesReport::new("dev-to-prod", dev(), prod(), EvidenceLevel::Attested);
        refused.refusal = Some(Finding::new(
            Code::PromoteNoEvidence,
            "path dev-to-prod has no recorded state of dev to promote",
            "run `nyl reconcile -e dev` first",
        ));
        assert_eq!(
            refused.table(),
            "promotion-candidates dev-to-prod: dev -> prod, evidence attested
  nyl promote would refuse: NYL-PROMOTE-NO-EVIDENCE: path dev-to-prod has no recorded state of dev to promote
  dev has no recorded states
"
        );
        assert_eq!(refused.exit, 0);
    }

    fn record(sequence: u64) -> PromotionRecord {
        PromotionRecord {
            path: PromotionPathName::new("dev-to-prod").unwrap(),
            sequence,
            from: PromotionFrom::Environment(EnvironmentFrom {
                environment: dev(),
                run: run(RUN_1),
                desired_commit: commit(DESIRED),
                observed_commit: Some(commit(OBSERVED)),
            }),
            source_commit: Some(commit(COMMIT)),
            values: BTreeMap::from([(
                "webImage".to_owned(),
                PromotedValue {
                    value: json!("registry.example.com/web@sha256:4f0c"),
                    select: PromotionSelect {
                        unit: Some(name("web-image")),
                        artifact: Some("image".to_owned()),
                        pointer: Some("/reference".to_owned()),
                        ..PromotionSelect::default()
                    },
                    unit: Some(PromotedUnit {
                        name: name("web-image"),
                        uid: None,
                    }),
                    execution_key: None,
                    input_digest: None,
                    state: None,
                },
            )]),
            evidence: PromotionEvidence {
                level: EvidenceLevel::Attested,
                attestations: vec![EvidenceAttestation {
                    scope: "environment".to_owned(),
                    name: "qa".to_owned(),
                    result: AttestationResult::Pass,
                    at: AT.parse().unwrap(),
                    details: Value::Null,
                }],
                override_: None,
            },
            ignored_attestations: Vec::new(),
            excluded_applications: Vec::new(),
            superseded: Vec::new(),
            artifacts_verified: true,
            requested: PromotionRequest {
                by: "Nyl Test <test@example.invalid>".to_owned(),
                identity: ApprovalIdentity::Local,
                source: None,
                at: AT.parse().unwrap(),
                reason: Some("Release 1".to_owned()),
            },
        }
    }

    #[test]
    fn test_promotions_report_golden() {
        let mut report = PromotionsReport::new(prod());
        report.applied = Some(AppliedPromotion {
            path: "dev-to-prod".to_owned(),
            sequence: 1,
        });
        report.promotions = vec![record(1)];
        report.history = vec![PromotionHistoryEntry {
            commit: commit("a6d7f81b83c462d9c82c9d009aea352d70f01425"),
            path: "dev-to-prod".to_owned(),
            sequence: 1,
            source_commit: Some(commit(COMMIT)),
            from: Some(HistoryFrom {
                environment: dev(),
                run: run(RUN_1),
                desired_commit: commit(DESIRED),
            }),
            requested_by: Some("Nyl Test <test@example.invalid>".to_owned()),
            reason: Some("Release 1".to_owned()),
        }];
        assert_eq!(
            report.table(),
            "dev-to-prod #1 (applied)
  from:     dev run 0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90 (desired d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0)
  source:   3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a
  value webImage: registry.example.com/web@sha256:4f0c
  evidence: attested
  requested by Nyl Test <test@example.invalid> at 2026-09-25T12:00:00Z: Release 1
history, newest first:
  #1 dev-to-prod a6d7f81b83c462d9c82c9d009aea352d70f01425: Release 1
"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["kind"], "PromotionsReport");
        assert_eq!(json["applied"], json!({"path": "dev-to-prod", "sequence": 1}));
        // Each promotion is the exact record state holds.
        assert_eq!(json["promotions"][0], serde_json::to_value(record(1)).unwrap());
        assert_eq!(
            json["history"],
            json!([{
                "commit": "a6d7f81b83c462d9c82c9d009aea352d70f01425",
                "path": "dev-to-prod",
                "sequence": 1,
                "sourceCommit": COMMIT,
                "from": {"environment": "dev", "run": RUN_1, "desiredCommit": DESIRED},
                "requestedBy": "Nyl Test <test@example.invalid>",
                "reason": "Release 1",
            }])
        );
        assert_eq!(PromotionsReport::new(prod()).table(), "prod: no promotion records\n");
    }
}
