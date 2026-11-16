//! The PromotionRecord: what `nyl promote` records about one promotion.
//!
//! Contract: [PromotionRecord](../../../../design/promotion.md#promotionrecord).
//!
//! One record shape is used wherever a record is kept (decision D4 of the M6
//! plan): `desired/promotions/<path>.yaml` in an environment's state, where
//! the document carries `apiVersion: gitops.nyl/v1` and `kind:
//! PromotionRecord`, and the `source.fromPromotion.promoted` block of an
//! Environment with `record: source`, where both are left out because the
//! block's place in the resource gives them. `desired/promotions/` is a file
//! the state layout of format `v1` always listed; adding the file changed no
//! version (decision F1 of the M6 plan).
//!
//! Lists the contract says a record always carries (`ignoredAttestations`,
//! `excludedApplications`, `superseded`, `evidence.attestations`) are written
//! even when empty, so a record states that nothing was ignored or superseded
//! instead of leaving it out.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{CommitId, EnvironmentName, ExecutionKey, PromotionPathName, RunId, Uid, UnitName};
use super::state::{constant_marker, document_kind, ApprovalIdentity, AttestationResult, StateV1};
use super::time::Timestamp;

document_kind!(PromotionRecordKind, "PromotionRecord");

/// Which field of the source a promoted value reads: the same selectors, with
/// the same JSON Pointer semantics and resolver, as `fromUnit` references and
/// `nyl get`.
///
/// Set exactly one of the four forms:
/// `{unit, input}` reads the source unit's resolved spec;
/// `{unit, output, pointer?}` a declared, non-sensitive output of its receipt;
/// `{unit, artifact, kind?, pointer}` a field of an artifact its receipt lists,
/// relative to the artifact's `spec`; and `{input}` (a target source, M11)
/// a Release input of the target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionSelect {
    /// The source unit's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<UnitName>,
    /// JSON Pointer into the source unit's resolved spec, such as `/source` or `/releases/platform~1web/image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// A declared, non-sensitive output of the unit's receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// An artifact the unit's receipt lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// The artifact kind the artifact must have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// JSON Pointer inside the output, or inside the artifact's `spec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// The source state a record promotes: one transition commit pair of the
/// source environment, pushed together with one `Nyl-Run-Id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionFrom {
    pub environment: EnvironmentName,
    pub run: RunId,
    #[serde(rename = "desiredCommit")]
    pub desired_commit: CommitId,
    /// Absent when the state has no observed commit of its own.
    #[serde(rename = "observedCommit", default, skip_serializing_if = "Option::is_none")]
    pub observed_commit: Option<CommitId>,
}

/// The state a value-only path took one value from, when that is not the
/// record's `from`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotedState {
    pub run: RunId,
    #[serde(rename = "desiredCommit")]
    pub desired_commit: CommitId,
    #[serde(rename = "observedCommit", default, skip_serializing_if = "Option::is_none")]
    pub observed_commit: Option<CommitId>,
}

/// The identity of the source unit a value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotedUnit {
    pub name: UnitName,
    /// The incarnation; absent when the unit had none in the recorded state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Uid>,
}

/// One promoted value with its lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotedValue {
    pub value: Value,
    pub select: PromotionSelect,
    pub unit: PromotedUnit,
    /// The receipt the value came from, for an output or artifact selector.
    #[serde(rename = "executionKey", default, skip_serializing_if = "Option::is_none")]
    pub execution_key: Option<ExecutionKey>,
    /// The digest of the unit's resolved input, for an `input` selector.
    #[serde(rename = "inputDigest", default, skip_serializing_if = "Option::is_none")]
    pub input_digest: Option<String>,
    /// Present when a value-only path took the value from another state than `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<PromotedState>,
}

/// The level a promotion's evidence reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceLevel {
    Published,
    Attested,
}

/// An attestation a promotion relied on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceAttestation {
    /// The unit name, or `environment`.
    pub scope: String,
    pub name: String,
    pub result: AttestationResult,
    pub at: Timestamp,
    /// The driver's details, such as each Application's running publication and health.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

/// Why a promotion went ahead below the path's evidence level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceOverride {
    pub reason: String,
}

/// The evidence a promotion rested on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionEvidence {
    pub level: EvidenceLevel,
    #[serde(default)]
    pub attestations: Vec<EvidenceAttestation>,
    /// Written as `null` when the promotion needed no override.
    #[serde(rename = "override", default)]
    pub override_: Option<EvidenceOverride>,
}

/// An attestation `--ignore-attestation` set aside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IgnoredAttestation {
    pub scope: String,
    pub name: String,
    pub reason: String,
    pub by: String,
}

/// A commit `--supersede` accepted as replaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SupersededCommit {
    /// The repository of the commit, such as a unit's source repository.
    pub repository: String,
    pub commit: CommitId,
    pub reason: String,
    pub by: String,
}

/// Who asked for the promotion, recorded like an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionRequest {
    pub by: String,
    pub identity: ApprovalIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What `nyl promote` records about one promotion on one path.
///
/// Each promotion replaces the path's record; Git history keeps earlier ones.
/// `apiVersion` and `kind` are written in `desired/promotions/<path>.yaml`
/// and left out of the `promoted` block of an Environment's source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionRecord {
    #[serde(rename = "apiVersion", default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<StateV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PromotionRecordKind>,
    /// The PromotionPath that wrote the record.
    pub path: PromotionPathName,
    /// Increases with every promotion into the target environment, on any path; the highest is the newest.
    pub sequence: u64,
    /// The source state; for a path that promotes only values, the newest state any value came from.
    pub from: PromotionFrom,
    /// The source commit of that state, present when the path promotes the target's source or a value's lineage includes one.
    #[serde(rename = "sourceCommit", default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    /// The promoted values by name.
    #[serde(default)]
    pub values: BTreeMap<String, PromotedValue>,
    pub evidence: PromotionEvidence,
    #[serde(rename = "ignoredAttestations", default)]
    pub ignored_attestations: Vec<IgnoredAttestation>,
    /// The path's `coverage.excludeApplications` at promotion time.
    #[serde(rename = "excludedApplications", default)]
    pub excluded_applications: Vec<String>,
    #[serde(default)]
    pub superseded: Vec<SupersededCommit>,
    /// Whether the artifact check ran.
    #[serde(rename = "artifactsVerified")]
    pub artifacts_verified: bool,
    pub requested: PromotionRequest,
}

impl PromotionRecord {
    /// The record as a state document: with `apiVersion` and `kind`.
    #[must_use]
    pub fn into_document(mut self) -> Self {
        self.api_version = Some(StateV1);
        self.kind = Some(PromotionRecordKind);
        self
    }

    /// Whether the record carries the markers a state document has.
    pub fn is_document(&self) -> bool {
        self.api_version.is_some() && self.kind.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        serde_json::json!({
            "path": "dev-to-prod", "sequence": 3,
            "from": {"environment": "dev", "run": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90", "desiredCommit": "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a"},
            "values": {}, "evidence": {"level": "published", "override": null},
            "artifactsVerified": true,
            "requested": {"by": "alice", "identity": "local", "at": "2026-09-25T16:41:00Z"}
        })
    }

    /// The block of an Environment's source and the state document share one
    /// shape: only the markers differ, and the empty lists are always written.
    #[test]
    fn test_one_shape_with_markers_only_in_the_document() {
        let block: PromotionRecord = serde_json::from_value(sample()).unwrap();
        assert!(!block.is_document());
        let written = serde_json::to_value(&block).unwrap();
        assert!(written.get("apiVersion").is_none());
        for list in ["ignoredAttestations", "excludedApplications", "superseded"] {
            assert_eq!(written[list], serde_json::json!([]), "{list}");
        }
        assert_eq!(written["evidence"]["override"], Value::Null);
        let document = serde_json::to_value(block.into_document()).unwrap();
        assert_eq!(document["apiVersion"], "gitops.nyl/v1");
        assert_eq!(document["kind"], "PromotionRecord");
    }

    #[test]
    fn test_record_rejects_other_formats_and_unknown_fields() {
        let mut other = sample();
        other["apiVersion"] = "gitops.nyl/v2".into();
        assert!(serde_json::from_value::<PromotionRecord>(other).is_err());
        let mut unknown = sample();
        unknown["surprise"] = true.into();
        assert!(serde_json::from_value::<PromotionRecord>(unknown).is_err());
    }
}
