//! Transition commit messages.
//!
//! Contract: [Transition commits](../../../../design/orchestration-core.md#transition-commits).
//! Each operation writes at most one commit per state ref. Its message is a
//! summary line, a summary machines parse, and Git trailers naming the
//! operation, run, environment, the state commits it started from, and the
//! time its decisions used. The summary is rendered from the operation's
//! typed report, so the commit and every other output agree.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::state::{Operation, Request};
use nyl_core::orchestration::time::Timestamp;
use serde_json::Value;

/// The machine-readable part of a transition commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionSummary {
    pub operation: Operation,
    pub environment: EnvironmentName,
    /// Counts or a short outcome for the summary line, such as `2 executed, 1 failed`.
    pub headline: String,
    /// Per unit: its result and details, as the operation's report holds them.
    pub units: BTreeMap<UnitName, Value>,
    pub recovered: Vec<UnitName>,
    pub imported: Vec<UnitName>,
    /// A `nyl lease break` this run's lease took over.
    pub lease_broken: Option<Request>,
    /// Operation specifics, such as a move's source and target.
    pub details: BTreeMap<String, Value>,
}

/// The trailers of a transition commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailers {
    pub run: RunId,
    pub source_commit: Option<CommitId>,
    pub read_desired: Option<CommitId>,
    pub read_observed: Option<CommitId>,
    pub runner: String,
    pub evaluated_at: Timestamp,
    pub version: String,
    pub requested_by: Option<String>,
    pub reason: Option<String>,
    /// A `--local` run's commit, never pushed.
    pub local: bool,
}

/// Render a transition commit message.
pub fn message(summary: &TransitionSummary, trailers: &Trailers) -> String {
    let mut text = format!("nyl: {} {}", summary.operation.as_str(), summary.environment);
    if !summary.headline.is_empty() {
        let _ = write!(text, " ({})", summary.headline);
    }
    text.push_str("\n\nunits:");
    if summary.units.is_empty() {
        text.push_str(" {}");
    }
    for (unit, value) in &summary.units {
        let _ = write!(text, "\n  {unit}: {}", flow(value));
    }
    let list = |units: &[UnitName]| {
        format!(
            "[{}]",
            units.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        )
    };
    let _ = write!(text, "\nrecovered: {}", list(&summary.recovered));
    let _ = write!(text, "\nimported: {}", list(&summary.imported));
    if let Some(broken) = &summary.lease_broken {
        let _ = write!(
            text,
            "\nleaseBroken: {}",
            flow(&serde_json::to_value(broken).expect("requests serialize"))
        );
    }
    for (key, value) in &summary.details {
        let _ = write!(text, "\n{key}: {}", flow(value));
    }
    text.push_str("\n\n");
    let mut trailer = |key: &str, value: &str| {
        let _ = writeln!(text, "{key}: {}", value.replace('\n', " "));
    };
    trailer("Nyl-Operation", summary.operation.as_str());
    trailer("Nyl-Run-Id", trailers.run.as_str());
    trailer("Nyl-Environment", summary.environment.as_str());
    if let Some(commit) = &trailers.source_commit {
        trailer("Nyl-Source-Commit", commit.as_str());
    }
    if let Some(commit) = &trailers.read_desired {
        trailer("Nyl-Read-Desired", commit.as_str());
    }
    if let Some(commit) = &trailers.read_observed {
        trailer("Nyl-Read-Observed", commit.as_str());
    }
    trailer("Nyl-Runner", &trailers.runner);
    trailer(
        "Nyl-Evaluated-At",
        &trailers.evaluated_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    trailer("Nyl-Version", &trailers.version);
    if let Some(by) = &trailers.requested_by {
        trailer("Nyl-Requested-By", by);
    }
    if let Some(reason) = &trailers.reason {
        trailer("Nyl-Reason", reason);
    }
    if trailers.local {
        trailer("Nyl-Local", "true");
    }
    text
}

/// A value in YAML flow style: compact JSON, which YAML reads as is.
fn flow(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON values serialize")
}

/// The value of a trailer in a transition commit message.
pub fn trailer<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    message
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(": ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_message_carries_a_parseable_summary_and_the_contract_trailers() {
        let summary = TransitionSummary {
            operation: Operation::Reconcile,
            environment: EnvironmentName::new("dev").unwrap(),
            headline: "2 executed, 1 failed, 1 blocked".to_owned(),
            units: BTreeMap::from([
                (UnitName::new("network").unwrap(), json!({"result": "current"})),
                (
                    UnitName::new("web-image").unwrap(),
                    json!({"result": "failed", "category": "tool-error", "retryable": true}),
                ),
            ]),
            recovered: Vec::new(),
            imported: vec![UnitName::new("network").unwrap()],
            lease_broken: None,
            details: BTreeMap::new(),
        };
        let trailers = Trailers {
            run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
            source_commit: Some(CommitId::new("a".repeat(40)).unwrap()),
            read_desired: None,
            read_observed: None,
            runner: "https://ci.example.com/runs/1234".to_owned(),
            evaluated_at: chrono::DateTime::parse_from_rfc3339("2026-09-24T10:07:30Z")
                .unwrap()
                .to_utc(),
            version: "0.7.0".to_owned(),
            requested_by: None,
            reason: None,
            local: false,
        };
        let text = message(&summary, &trailers);
        assert!(text.starts_with("nyl: reconcile dev (2 executed, 1 failed, 1 blocked)\n\nunits:\n"));
        // The summary between the headline and the trailers is YAML.
        let body = text.split("\n\n").nth(1).unwrap();
        let parsed: serde_json::Value = serde_saphyr::from_str(body).unwrap();
        assert_eq!(parsed["units"]["web-image"]["category"], "tool-error");
        assert_eq!(parsed["imported"], json!(["network"]));
        assert_eq!(trailer(&text, "Nyl-Operation"), Some("reconcile"));
        assert_eq!(trailer(&text, "Nyl-Evaluated-At"), Some("2026-09-24T10:07:30Z"));
        assert_eq!(trailer(&text, "Nyl-Source-Commit"), Some("a".repeat(40).as_str()));
        assert_eq!(trailer(&text, "Nyl-Local"), None);
    }
}
