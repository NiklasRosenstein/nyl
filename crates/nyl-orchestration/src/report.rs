//! Typed reports of state operations: `state init`, `state move`,
//! `state forget`, and `lease break`.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection)
//! for the exit categories. Every output is rendered from the report: the
//! table, `--output json`, and the exit category, so they cannot disagree.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName};
use serde::Serialize;
use serde_json::Value;

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
    /// The transition commits written, by state ref side.
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
/// (`outcome: unchanged`), or, with `--dry-run`, that would be written.
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
    /// The transition commits written, by state ref side.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub commits: BTreeMap<&'static str, CommitId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<nyl_core::orchestration::promotion::PromotionRecord>,
    /// What keeps the newest candidate below the path's evidence level, for a refusal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<PromoteGap>,
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
        use std::fmt::Write as _;
        let mut text = String::new();
        let status = match (self.outcome, self.dry_run) {
            (Outcome::Refused, _) => "refused",
            (_, true) => "dry run, nothing written",
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
        for (side, commit) in &self.commits {
            let _ = writeln!(text, "  {side}: {commit}");
        }
        text
    }
}
