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
    /// Operation specifics, such as the location moved to.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

impl StateOperationReport {
    pub fn new(environment: EnvironmentName, operation: &'static str) -> Self {
        Self {
            api_version: "gitops.nyl/v1",
            kind: "StateOperationReport",
            environment,
            operation,
            outcome: Outcome::Unchanged,
            exit: 0,
            commits: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            details: BTreeMap::new(),
        }
    }

    /// Refuse with `finding`: exit 2 while another run holds the lease, so a
    /// pipeline can wait and retry, and exit 1 for every other refusal.
    #[must_use]
    pub fn refused(mut self, finding: Finding) -> Self {
        self.exit = if finding.code == Code::LeaseHeld { 2 } else { 1 };
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
