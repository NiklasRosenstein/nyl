//! The PromotionRecord: what `nyl promote` records about one promotion.
//!
//! Contract: [PromotionRecord](../../../../design/promotion.md#promotionrecord).
//!
//! One record shape is used wherever a record is kept (decision D4 of the M6
//! plan): [`PromotionRecord`] is the shape. In `desired/promotions/<path>.yaml`
//! it is wrapped as a [`PromotionRecordDocument`] with `apiVersion:
//! gitops.nyl/v1` and `kind: PromotionRecord`, both required; in the
//! `source.fromPromotion.promoted` block of an Environment with `record:
//! source` it stands alone, and the markers are rejected there.
//!
//! The shape reserves M11's target source: [`PromotionFrom::Target`] and a value
//! without a source unit. M6 never writes them, and validation and `nyl promote`
//! refuse a target (`NYL-PROMOTE-UNSUPPORTED`), but a reader loads a record that
//! has them, so M11 changes no format. `desired/promotions/` is a file
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

/// The source state a record promotes: an environment's, or (M11, never
/// written by M6) a DeploymentTarget's publication commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PromotionFrom {
    Environment(EnvironmentFrom),
    /// Reserved for M11.
    Target(TargetFrom),
}

/// A target source: the publication commit the values came from. Reserved for M11.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetFrom {
    pub target: String,
    pub repository: String,
    pub branch: String,
    pub commit: CommitId,
}

/// One transition commit pair of the source environment, pushed together
/// with one `Nyl-Run-Id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentFrom {
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
    /// The source unit; absent for a target source's value (M11) and when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<PromotedUnit>,
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
    pub attestations: Vec<EvidenceAttestation>,
    /// Written as `null` when the promotion needed no override.
    #[serde(rename = "override")]
    #[schemars(required)]
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
/// In an Environment's `source.fromPromotion.promoted` block it stands alone;
/// in state it is a [`PromotionRecordDocument`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionRecord {
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
    pub values: BTreeMap<String, PromotedValue>,
    pub evidence: PromotionEvidence,
    #[serde(rename = "ignoredAttestations")]
    pub ignored_attestations: Vec<IgnoredAttestation>,
    /// The path's `coverage.excludeApplications` at promotion time.
    #[serde(rename = "excludedApplications")]
    pub excluded_applications: Vec<String>,
    pub superseded: Vec<SupersededCommit>,
    /// Whether the artifact check ran.
    #[serde(rename = "artifactsVerified")]
    pub artifacts_verified: bool,
    pub requested: PromotionRequest,
}

/// `desired/promotions/<path>.yaml`: a [`PromotionRecord`] with the markers
/// every state document carries, both required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromotionRecordDocument {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: PromotionRecordKind,
    #[serde(flatten)]
    pub record: PromotionRecord,
}

impl From<PromotionRecord> for PromotionRecordDocument {
    fn from(record: PromotionRecord) -> Self {
        Self {
            api_version: StateV1,
            kind: PromotionRecordKind,
            record,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        serde_json::json!({
            "path": "dev-to-prod", "sequence": 3,
            "from": {"environment": "dev", "run": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90", "desiredCommit": "3e7b9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a"},
            "values": {}, "evidence": {"level": "published", "attestations": [], "override": null},
            "ignoredAttestations": [], "excludedApplications": [], "superseded": [],
            "artifactsVerified": true,
            "requested": {"by": "alice", "identity": "local", "at": "2026-09-25T16:41:00Z"}
        })
    }

    /// The block of an Environment's source rejects the state markers, the
    /// state document requires them, and the lists are written when empty.
    #[test]
    fn test_block_and_document_differ_only_in_the_markers() {
        let block: PromotionRecord = serde_json::from_value(sample()).unwrap();
        let written = serde_json::to_value(&block).unwrap();
        assert_eq!(written, sample());
        let mut marked = sample();
        marked["apiVersion"] = "gitops.nyl/v1".into();
        marked["kind"] = "PromotionRecord".into();
        assert!(
            serde_json::from_value::<PromotionRecord>(marked.clone()).is_err(),
            "the block takes no markers"
        );
        let document: PromotionRecordDocument = serde_json::from_value(marked).unwrap();
        assert_eq!(document.record, block);
        assert_eq!(
            serde_json::to_value(PromotionRecordDocument::from(block)).unwrap()["kind"],
            "PromotionRecord"
        );
        assert!(
            serde_json::from_value::<PromotionRecordDocument>(sample()).is_err(),
            "a document needs its markers"
        );
    }

    #[test]
    fn test_record_rejects_other_formats_and_unknown_fields() {
        let mut document = sample();
        document["apiVersion"] = "gitops.nyl/v2".into();
        document["kind"] = "PromotionRecord".into();
        let error = serde_json::from_value::<PromotionRecordDocument>(document)
            .unwrap_err()
            .to_string();
        assert!(error.contains("upgrade Nyl"), "{error}");
        let mut unknown = sample();
        unknown["surprise"] = true.into();
        assert!(serde_json::from_value::<PromotionRecord>(unknown).is_err());
    }

    /// The shape reserves M11's target source: a reader loads it, and it round-trips.
    #[test]
    fn test_a_target_source_record_loads() {
        let mut target = sample();
        target["from"] = serde_json::json!({"target": "dev", "repository": "https://example.invalid/deploy.git", "branch": "deploy/dev", "commit": "9c1e9c0a1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a"});
        target["values"] = serde_json::json!({"webImage": {"value": "x", "select": {"input": "/releases/platform~1web/image"}, "inputDigest": "sha256:0"}});
        let record: PromotionRecord = serde_json::from_value(target.clone()).unwrap();
        assert!(matches!(record.from, PromotionFrom::Target(_)));
        assert!(record.values["webImage"].unit.is_none());
        assert_eq!(serde_json::to_value(&record).unwrap(), target);
    }

    /// The published schema requires what the writer always writes.
    #[test]
    fn test_schema_requires_the_always_written_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(PromotionRecord)).unwrap();
        let required = schema["required"].as_array().unwrap();
        for field in [
            "values",
            "ignoredAttestations",
            "excludedApplications",
            "superseded",
            "evidence",
            "requested",
        ] {
            assert!(required.contains(&field.into()), "{field}: {required:?}");
        }
        let evidence = &schema["$defs"]["PromotionEvidence"]["required"];
        assert!(evidence.as_array().unwrap().contains(&"override".into()), "{evidence}");
        assert!(
            evidence.as_array().unwrap().contains(&"attestations".into()),
            "{evidence}"
        );
        let document = serde_json::to_value(schemars::schema_for!(PromotionRecordDocument)).unwrap();
        let text = document.to_string();
        assert!(
            text.contains("apiVersion") && text.contains("PromotionRecord"),
            "{text}"
        );
    }
}
