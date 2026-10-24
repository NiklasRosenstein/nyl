//! State documents: what the desired, observed, and coordination refs hold.
//!
//! Contract: [State layout](../../../../design/orchestration-core.md#state-layout),
//! [Desired unit](../../../../design/orchestration-core.md#desired-unit),
//! [Observed unit](../../../../design/orchestration-core.md#observed-unit),
//! [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
//! and [Deletion](../../../../design/orchestration-core.md#deletion).
//!
//! Every document carries `apiVersion` and `kind`, and the version in
//! `apiVersion` is the format version of that kind. Readers reject a version
//! or kind they do not know, through the marker types below, so a newer
//! format is never misread. A format change adds a version with a migration;
//! golden files under `crates/nyl-core/tests/golden/state/` cover every kind
//! and version.
//!
//! Lifecycle and conditions are enums that carry their data, so every state
//! a unit can be in is spelled out and exhaustive matching forces each new
//! state through every decision.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::ids::{ChangeDigest, CommitId, EnvironmentName, ExecutionKey, JsonPointer, RunId, Uid, UnitName};
use super::time::Timestamp;
use crate::constants::{API_VERSION_ARTIFACTS, API_VERSION_GITOPS};

macro_rules! document_kind {
    ($(#[$meta:meta])* $name:ident, $api_version:expr, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $name;

        impl $name {
            pub const API_VERSION: &'static str = $api_version;
            pub const KIND: &'static str = $kind;
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(Self::KIND)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                let kind = String::deserialize(deserializer)?;
                if kind == Self::KIND {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom(format_args!(
                        "expected kind {}, got {kind:?}",
                        Self::KIND
                    )))
                }
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({"const": $kind})
            }
        }
    };
}

/// The format version of the `gitops.nyl/v1` state kinds. Deserializing any
/// other `apiVersion` fails, so a reader never misreads a newer format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StateV1;

impl Serialize for StateV1 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(API_VERSION_GITOPS)
    }
}

impl<'de> Deserialize<'de> for StateV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let version = String::deserialize(deserializer)?;
        if version == API_VERSION_GITOPS {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format_args!(
                "unsupported state format {version:?}; this Nyl reads {API_VERSION_GITOPS}, so upgrade Nyl to read newer state"
            )))
        }
    }
}

impl JsonSchema for StateV1 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StateV1".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"const": API_VERSION_GITOPS})
    }
}

document_kind!(StateRecordKind, API_VERSION_GITOPS, "StateRecord");
document_kind!(EnvironmentRecordKind, API_VERSION_GITOPS, "EnvironmentRecord");
document_kind!(DesiredUnitKind, API_VERSION_GITOPS, "DesiredUnit");
document_kind!(ObservedUnitKind, API_VERSION_GITOPS, "ObservedUnit");
document_kind!(ObservationKind, API_VERSION_GITOPS, "Observation");
document_kind!(AttestationKind, API_VERSION_GITOPS, "Attestation");
document_kind!(LeaseKind, API_VERSION_GITOPS, "Lease");
document_kind!(RunKind, API_VERSION_GITOPS, "Run");
document_kind!(SignalKind, API_VERSION_GITOPS, "Signal");

/// Where an environment's state lives, as `state.yaml` records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateLocation {
    /// Credential-free URL of the state repository, or `local` for the clone's own branches.
    pub repository: String,
    #[serde(rename = "desiredRef")]
    pub desired_ref: String,
    #[serde(rename = "observedRef")]
    pub observed_ref: String,
    /// Directory of this environment's state within its refs; empty for the root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(rename = "coordinationRefPrefix")]
    pub coordination_ref_prefix: String,
}

/// `state.yaml`: the state location and its history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateRecord {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: StateRecordKind,
    pub environment: EnvironmentName,
    /// The EnvironmentTemplate of an instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// An instance's parameters.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Value>,
    /// An instance's expiry.
    #[serde(rename = "expiresAt", default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    pub location: StateLocation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<StateEvent>,
}

/// One initialization, move, or restore of a state location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StateEvent {
    StateInitialized {
        at: Timestamp,
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// A deliberate fresh start: every unit is a new incarnation.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fresh: bool,
    },
    StateMoved {
        at: Timestamp,
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The location the state moved from.
        from: Box<StateLocation>,
        /// The location the state moved to.
        to: Box<StateLocation>,
    },
    StateRestored {
        at: Timestamp,
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The commit whose content was restored.
        to: CommitId,
    },
}

/// `desired/environment.yaml`: the resolved Environment, its source commit,
/// and its ownership set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRecord {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: EnvironmentRecordKind,
    pub environment: EnvironmentName,
    /// The resolved Environment spec with group defaults applied.
    pub spec: Value,
    /// The source commit S of the run that wrote this record.
    #[serde(rename = "sourceCommit")]
    pub source_commit: CommitId,
    /// How S was chosen.
    #[serde(rename = "sourceChosenBy")]
    pub source_chosen_by: SourceChoice,
    #[serde(
        rename = "appliedPromotionSequence",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub applied_promotion_sequence: Option<u64>,
    /// The ownership set: every selected, enabled unit, with the uid of its incarnation when it has one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ownership: BTreeMap<UnitName, Option<Uid>>,
}

/// How a run's source commit was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "by", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SourceChoice {
    /// The entry worktree's commit.
    Entry,
    /// The tip of the Environment's `source.revision`.
    Revision { revision: String },
    /// The Environment's `source.commit` lock.
    Commit { revision: String },
    /// The newest PromotionRecord on a path.
    Promotion { path: String, sequence: u64 },
    /// The current deployment admission.
    Admission { generation: String },
    /// `--source` for this run.
    Override { revision: String },
}

/// A unit's identity in state documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnitIdentity {
    pub environment: EnvironmentName,
    pub name: UnitName,
    /// The incarnation; absent for a held unit without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<Uid>,
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
}

/// `desired/units/<unit>.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredUnit {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: DesiredUnitKind,
    pub unit: UnitIdentity,
    pub lifecycle: Lifecycle,
    #[serde(rename = "sourceCommit")]
    pub source_commit: CommitId,
    /// The effective revision for repository content, S.
    #[serde(rename = "sourceRevision")]
    pub source_revision: CommitId,
    /// The rendered spec, references still as expressions.
    pub spec: Value,
    /// The spec with references replaced by values; absent while blocked.
    #[serde(rename = "resolvedSpec", default, skip_serializing_if = "Option::is_none")]
    pub resolved_spec: Option<Value>,
    /// Per reference, by JSON Pointer, where its value came from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<JsonPointer, Provenance>,
    /// Absent while blocked.
    #[serde(rename = "executionKey", default, skip_serializing_if = "Option::is_none")]
    pub execution_key: Option<ExecutionKey>,
    /// A command unit's fingerprint output, truncated to 4 KiB, for audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The attestations the unit's receipts can carry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attestations: Vec<String>,
    pub readiness: Readiness,
}

/// Where a unit is in its life: running, being deleted, frozen, or retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Lifecycle {
    Active,
    Deleting {
        deletion: Deletion,
    },
    /// Frozen by an operator; a held deleting unit keeps its deletion block.
    Held {
        hold: Hold,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deletion: Option<Deletion>,
    },
    /// A `Retain` unit outside the ownership set, keeping its last receipt.
    Retained {
        tombstone: Tombstone,
    },
}

/// Why and how a unit is being deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Deletion {
    pub reason: DeletionReason,
    pub intent: DeletionIntent,
    /// Absent for `intent: retain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown: Option<TeardownProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<Request>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DeletionReason {
    /// The unit left the ownership set.
    Omission,
    /// `nyl teardown --unit` of a unit outside the ownership set.
    Teardown,
    /// `nyl teardown --unit` of a unit inside it.
    Replace,
    /// The unit's `apiVersion` or `kind` changed.
    KindChange,
    /// An expired template instance.
    Expired,
    /// `teardown --all` or `state delete --teardown`.
    Decommission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum DeletionIntent {
    Retain,
    Teardown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TeardownProgress {
    /// Awaiting `--allow-teardown`, an approval, a dependent, or a readiness override.
    Pending,
    /// The driver's teardown has started.
    InProgress,
}

/// Who requested an operator action, when, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub by: String,
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An operator's hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    pub since: Timestamp,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Newer source not applied while held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<HoldPending>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HoldPending {
    #[serde(rename = "sourceCommit")]
    pub source_commit: CommitId,
    #[serde(rename = "executionKey", default, skip_serializing_if = "Option::is_none")]
    pub execution_key: Option<ExecutionKey>,
    /// Whether the unit left the ownership set.
    #[serde(default)]
    pub deletion: bool,
}

/// What a `retained` unit keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub since: Timestamp,
    /// The last receipt; a returning unit is current at once while it still matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
}

/// Where one referenced value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Provenance {
    Receipt(ReceiptProvenance),
    Promotion(PromotionProvenance),
}

/// A value read from a producer's receipt: from an observed commit, or from
/// a receipt the same run produced earlier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiptProvenance {
    /// The producer's environment, for cross-environment references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentName>,
    pub unit: UnitName,
    pub uid: Uid,
    #[serde(rename = "executionKey")]
    pub execution_key: ExecutionKey,
    #[serde(rename = "observedCommit", default, skip_serializing_if = "Option::is_none")]
    pub observed_commit: Option<CommitId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
}

/// A value read from a PromotionRecord.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromotionProvenance {
    pub promotion: String,
    pub sequence: u64,
}

/// Whether a desired document can execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Readiness {
    Ready,
    Blocked { reasons: Vec<BlockedReason> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockedReason {
    /// Location of the unresolved reference, or empty for a `dependsOn` entry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pointer: String,
    pub reason: String,
}

/// `observed/units/<unit>.yaml`: the latest receipt and condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservedUnit {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: ObservedUnitKind,
    pub unit: UnitIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
    #[serde(default)]
    pub condition: Option<Condition>,
}

/// What one successful execution recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    #[serde(rename = "executionKey")]
    pub execution_key: ExecutionKey,
    #[serde(rename = "behaviorVersion")]
    pub behavior_version: u32,
    pub run: RunId,
    /// The desired commit the run started from.
    #[serde(rename = "readDesired", default, skip_serializing_if = "Option::is_none")]
    pub read_desired: Option<CommitId>,
    #[serde(rename = "startedAt")]
    pub started_at: Timestamp,
    #[serde(rename = "finishedAt")]
    pub finished_at: Timestamp,
    /// Nyl's and the tools' versions, for audit.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub versions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<JsonPointer, Provenance>,
    /// Declared, non-sensitive outputs.
    #[serde(default = "empty_object", skip_serializing_if = "is_empty_object")]
    pub outputs: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactDigest>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn is_empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(serde_json::Map::is_empty)
}

/// An artifact a receipt lists, with the digest of its canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDigest {
    pub name: String,
    pub kind: String,
    pub digest: String,
}

/// Who approved an execution, how Nyl knows, and what they saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<String>,
    pub identity: ApprovalIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The approved change digest; `null` for an approval without one.
    pub digest: Option<ChangeDigest>,
    /// Whether someone saw the exact changes: false without a digest.
    pub reviewed: bool,
}

/// How Nyl learned an approver's identity. An audit record, not authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalIdentity {
    /// Read from the CI system, by a built-in lookup or the project's hook.
    Verified,
    /// Passed with `--approved-by`.
    Asserted,
    /// The Git user of a workstation run.
    Local,
}

/// The latest failure or uncertainty, kept until the unit next succeeds or
/// an operator recovers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Condition {
    /// Effects may or may not have happened.
    Uncertain {
        since: Timestamp,
        run: RunId,
        reason: String,
    },
    /// The execution made no effect it did not report.
    Failed {
        since: Timestamp,
        run: RunId,
        category: FailureCategory,
        retryable: bool,
        message: String,
    },
}

/// The closed set of failure categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FailureCategory {
    ToolError,
    OutputRejected,
    MissingTool,
    Credentials,
    Ownership,
    Timeout,
}

/// An artifact document, under `observed/units/<unit>/artifacts/<name>.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactDocument {
    /// The artifact kind's group, such as `artifacts.gitops.nyl/v1`.
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: ArtifactMetadata,
    #[serde(rename = "executionKey")]
    pub execution_key: ExecutionKey,
    pub spec: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub name: String,
    pub unit: ArtifactUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactUnit {
    pub environment: EnvironmentName,
    pub name: UnitName,
    pub uid: Uid,
}

impl ArtifactDocument {
    /// Whether the document is a built-in artifact kind.
    pub fn is_builtin(&self) -> bool {
        self.api_version == API_VERSION_ARTIFACTS
    }
}

/// `observed/units/<unit>/observations/<type>.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: ObservationKind,
    pub unit: UnitIdentity,
    #[serde(rename = "type")]
    pub observation_type: ObservationType,
    #[serde(rename = "executionKey")]
    pub execution_key: ExecutionKey,
    pub at: Timestamp,
    pub result: Drift,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObservationType {
    Drift,
    Health,
}

/// The result of a drift verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "drift", rename_all = "lowercase", deny_unknown_fields)]
pub enum Drift {
    Clean,
    Drifted { summary: String },
    Unknown { reason: String },
}

/// An attestation of a receipt, or of an environment's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: AttestationKind,
    pub name: String,
    /// Absent for an environment-wide attestation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<ArtifactUnit>,
    /// The receipt attested; environment-wide, the desired commit of the state.
    #[serde(rename = "executionKey")]
    pub execution_key: String,
    pub result: AttestationResult,
    pub at: Timestamp,
    pub source: Value,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AttestationResult {
    Pass,
    Fail,
}

/// The operations that write state, as the `Nyl-Operation` trailer names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Reconcile,
    Promote,
    Teardown,
    Hold,
    Resume,
    Recover,
    Verify,
    Attest,
    Exec,
    CiAdmit,
    StateInit,
    StateMove,
    StateDelete,
    StateForget,
    StateRestore,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Promote => "promote",
            Self::Teardown => "teardown",
            Self::Hold => "hold",
            Self::Resume => "resume",
            Self::Recover => "recover",
            Self::Verify => "verify",
            Self::Attest => "attest",
            Self::Exec => "exec",
            Self::CiAdmit => "ci-admit",
            Self::StateInit => "state-init",
            Self::StateMove => "state-move",
            Self::StateDelete => "state-delete",
            Self::StateForget => "state-forget",
            Self::StateRestore => "state-restore",
        }
    }
}

/// The lease ref's document: which run holds the environment until when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: LeaseKind,
    pub run: RunId,
    /// Who runs it, such as a CI run URL or `user@host`.
    pub runner: String,
    pub operation: Operation,
    /// Units executing now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<UnitName>,
    pub deadline: Timestamp,
    #[serde(rename = "takenAt")]
    pub taken_at: Timestamp,
}

/// A run ref's record, updated per phase together with the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Run {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: RunKind,
    pub run: RunId,
    pub runner: String,
    pub operation: Operation,
    #[serde(rename = "sourceCommit", default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    #[serde(rename = "readDesired", default, skip_serializing_if = "Option::is_none")]
    pub read_desired: Option<CommitId>,
    #[serde(rename = "readObserved", default, skip_serializing_if = "Option::is_none")]
    pub read_observed: Option<CommitId>,
    pub deadline: Timestamp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<UnitName>,
    /// Set when the run stopped without its transition commit, so the run
    /// ref no longer counts as a held lease.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub finished: bool,
}

/// A message to a running run, such as a teardown confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: SignalKind,
    pub run: RunId,
    pub unit: UnitName,
    #[serde(rename = "phase1Commit")]
    pub phase1_commit: CommitId,
    pub by: String,
    pub identity: ApprovalIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: Timestamp,
    /// Where the confirmation came from, such as a CI run URL.
    pub from: String,
}
