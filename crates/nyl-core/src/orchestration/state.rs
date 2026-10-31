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
use super::ports::StateSide;
use super::time::Timestamp;
use crate::constants::{API_VERSION_ARTIFACTS, API_VERSION_GITOPS};

/// A unit struct that serializes as the constant string `$value` and
/// refuses every other string on deserialization with `$message`.
macro_rules! constant_marker {
    ($(#[$meta:meta])* $name:ident, $value:expr, |$got:ident| $message:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $name;

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str($value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                let $got = String::deserialize(deserializer)?;
                if $got == $value {
                    Ok(Self)
                } else {
                    Err(serde::de::Error::custom($message))
                }
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({"const": $value})
            }
        }
    };
}

macro_rules! document_kind {
    ($name:ident, $kind:literal) => {
        constant_marker!($name, $kind, |kind| format_args!(
            "expected kind {}, got {kind:?}",
            $kind
        ));
    };
}

constant_marker!(
    /// The format version of the `gitops.nyl/v1` state kinds. Deserializing any
    /// other `apiVersion` fails, so a reader never misreads a newer format.
    StateV1,
    API_VERSION_GITOPS,
    |version| format_args!(
        "unsupported state format {version:?}; this Nyl reads {API_VERSION_GITOPS}, so upgrade Nyl to read newer state"
    )
);

document_kind!(StateRecordKind, "StateRecord");
document_kind!(EnvironmentRecordKind, "EnvironmentRecord");
document_kind!(DesiredUnitKind, "DesiredUnit");
document_kind!(ObservedUnitKind, "ObservedUnit");
document_kind!(ObservationKind, "Observation");
document_kind!(AttestationKind, "Attestation");
document_kind!(LeaseKind, "Lease");
document_kind!(RunKind, "Run");
document_kind!(SignalKind, "Signal");

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

impl StateLocation {
    /// The `repository` of a location that is the clone's own branches, used
    /// when no state repository is configured and the checkout has no `origin`.
    pub const LOCAL_REPOSITORY: &'static str = "local";

    pub fn is_local(&self) -> bool {
        self.repository == Self::LOCAL_REPOSITORY
    }

    /// Whether a configured state ref name has a branch in the clone: a
    /// short name or a `refs/heads/…` name. Other full `refs/…` names have none.
    pub fn is_branch_ref(configured: &str) -> bool {
        !configured.starts_with("refs/") || configured.starts_with("refs/heads/")
    }

    /// Whether both state refs have a branch in the clone, by [`Self::is_branch_ref`].
    pub fn has_local_branches(&self) -> bool {
        Self::is_branch_ref(&self.desired_ref) && Self::is_branch_ref(&self.observed_ref)
    }

    /// The full name of a configured state ref: a short name is a branch.
    pub fn full_name(configured: &str) -> String {
        if configured.starts_with("refs/") {
            configured.to_owned()
        } else {
            format!("refs/heads/{configured}")
        }
    }

    /// The configured name of one of the two state refs.
    pub fn state_ref(&self, side: StateSide) -> &str {
        match side {
            StateSide::Desired => &self.desired_ref,
            StateSide::Observed => &self.observed_ref,
        }
    }
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
    /// `nyl state delete`: the environment's other state files were removed
    /// in the same commit, and `state.yaml` stays as the deleted marker. A
    /// plain `nyl state init` refuses on it (`NYL-STATE-DELETED`); `--fresh`
    /// appends a new initialization. Contract:
    /// [Instances](../../../../design/orchestration-core.md#instances).
    StateDeleted {
        at: Timestamp,
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl StateRecord {
    /// The `state-deleted` event that marks the location deleted: the last
    /// event, when it is one. A later initialization (`--fresh`) ends it.
    pub fn deleted(&self) -> Option<&StateEvent> {
        self.events
            .last()
            .filter(|event| matches!(event, StateEvent::StateDeleted { .. }))
    }
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
    /// An invocation-selected revision. A `refs/nyl-worktrees/` revision is
    /// archived worktree provenance and cannot establish clean promotion evidence.
    Override { revision: String },
}

impl SourceChoice {
    /// Whether the recorded source is an explicit worktree snapshot.
    pub fn is_worktree(&self) -> bool {
        matches!(self, Self::Override { revision } if revision.starts_with(super::ports::WORKTREE_SNAPSHOT_PREFIX))
    }
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
    /// The unit's kind; empty only in a held marker, which `nyl hold` writes
    /// for a unit it never rendered ([`DesiredUnit::held_marker`]).
    #[serde(rename = "apiVersion", default, skip_serializing_if = "String::is_empty")]
    pub api_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
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
    /// The source commit S the document was rendered from; absent only in a
    /// held marker.
    #[serde(rename = "sourceCommit", default, skip_serializing_if = "Option::is_none")]
    pub source_commit: Option<CommitId>,
    /// The effective revision for repository content, S; absent only in a
    /// held marker.
    #[serde(rename = "sourceRevision", default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<CommitId>,
    /// The rendered spec, references still as expressions; null only in a
    /// held marker.
    #[serde(default, skip_serializing_if = "Value::is_null")]
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

impl DesiredUnit {
    /// A held marker: the desired file `nyl hold` writes for a unit with no
    /// desired document in state (never reconciled in the environment). It
    /// has no uid and no rendering (no kind, source commit, or spec); `nyl
    /// hold` never renders source. Reconcile excludes it until `nyl resume`;
    /// the first reconcile after the resume renders the unit as a new
    /// incarnation, or removes the file when the unit is outside the
    /// ownership set. Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
    /// "Holds freeze a unit".
    pub fn held_marker(environment: EnvironmentName, name: UnitName, hold: Hold) -> Self {
        Self {
            api_version: StateV1,
            kind: DesiredUnitKind,
            unit: UnitIdentity {
                environment,
                name,
                uid: None,
                api_version: String::new(),
                kind: String::new(),
            },
            lifecycle: Lifecycle::Held { hold, deletion: None },
            source_commit: None,
            source_revision: None,
            spec: Value::Null,
            resolved_spec: None,
            provenance: BTreeMap::new(),
            execution_key: None,
            fingerprint: None,
            attestations: Vec::new(),
            readiness: Readiness::Ready,
        }
    }

    /// Whether this is a held marker, a document never rendered
    /// ([`Self::held_marker`]): it names no kind.
    pub fn is_held_marker(&self) -> bool {
        self.unit.kind.is_empty()
    }
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
    /// `nyl teardown --hold`: hold the unit once its teardown completes,
    /// also when that is a later run after `nyl resume`, so it is not
    /// recreated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold: bool,
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
    /// The desired document the receipt was produced for: a copy written by
    /// the same transition that writes the receipt, absent when there is no
    /// receipt. A hold reverts an active unit to it directly, never through
    /// commit history. Contract: [Recovery](../../../../design/orchestration-core.md#recovery),
    /// "Holds on uncertain units", and [Deletion](../../../../design/orchestration-core.md#deletion),
    /// "Holds freeze a unit".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired: Option<Box<DesiredUnit>>,
    /// A two-phase teardown that published phase 1 and waits under its
    /// `teardownWait`: what `Outcome::Waiting` reported, kept between runs.
    /// Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
    /// Teardown, and [Drivers](../../../../design/orchestration-core.md#drivers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<TeardownWaiting>,
    /// A native repair requires an explicit approval before the next execution.
    #[serde(rename = "operatorModified", default, skip_serializing_if = "Option::is_none")]
    pub operator_modified: Option<OperatorModification>,
    /// A driver-requested retry acknowledgement, bound to this unit incarnation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryAcknowledgement>,
}

/// Driver-owned retry context and the operator's acknowledgement.
/// Context contains no secrets and is interpreted only by the emitting driver.
/// A successful receipt consumes the acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RetryAcknowledgement {
    Required { context: Value },
    Acknowledged { context: Value, request: Request },
}

/// A native operator call is fenced before it starts and records its result afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperatorModification {
    #[serde(flatten)]
    pub request: Request,
    /// Absent if the runner stopped before recording the tool's exit.
    pub status: Option<super::driver::ExecStatus>,
}

/// A teardown waiting after its first phase, as the observed unit records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TeardownWaiting {
    /// The driver's phase name, such as `applications-removed`.
    pub phase: String,
    pub reason: String,
    /// When the phase was published: the start of a `delay` wait.
    pub since: Timestamp,
    /// When the wait ends by itself, such as a `delay`'s end; `None` for a
    /// `manual` wait, which never keeps a run waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// The phase 1 commit, the teardown's published evidence; a
    /// confirmation is bound to it.
    #[serde(rename = "phase1Commit", default, skip_serializing_if = "Option::is_none")]
    pub phase1_commit: Option<CommitId>,
    /// The run that published phase 1.
    pub run: RunId,
}

impl ObservedUnit {
    /// The run of the document's latest change: its condition's run, else
    /// its receipt's.
    pub fn last_run(&self) -> Option<&RunId> {
        match &self.condition {
            Some(condition) => Some(condition.run()),
            None => self.receipt.as_ref().map(|receipt| &receipt.run),
        }
    }

    /// When the document last changed: its condition's `since`, else its
    /// receipt's `finishedAt`.
    pub fn changed_at(&self) -> Option<Timestamp> {
        match &self.condition {
            Some(condition) => Some(condition.since()),
            None => self.receipt.as_ref().map(|receipt| receipt.finished_at),
        }
    }
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

impl Condition {
    /// The run that set the condition.
    pub fn run(&self) -> &RunId {
        match self {
            Self::Uncertain { run, .. } | Self::Failed { run, .. } => run,
        }
    }

    /// When the condition was set.
    pub fn since(&self) -> Timestamp {
        match self {
            Self::Uncertain { since, .. } | Self::Failed { since, .. } => *since,
        }
    }

    /// The uncertainty's reason or the failure's message.
    pub fn message(&self) -> &str {
        match self {
            Self::Uncertain { reason, .. } => reason,
            Self::Failed { message, .. } => message,
        }
    }

    /// `uncertain` or `failed`, as the document's `state` field names it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Uncertain { .. } => "uncertain",
            Self::Failed { .. } => "failed",
        }
    }
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

impl ArtifactDocument {
    /// The digest a receipt lists for this document, and the one reference
    /// resolution checks before reading it: `sha256:<hex>` over the RFC 8785
    /// canonical JSON of the whole document, including `metadata.unit` and
    /// `executionKey`. The shell computes it after filling those fields.
    ///
    /// Contract: [Artifacts](../../../../design/orchestration-core.md#artifacts).
    pub fn digest(&self) -> String {
        let bytes = crate::digest::canonical_json_bytes(self).expect("artifact documents serialize to JSON");
        crate::digest::sha256_prefixed(&bytes)
    }

    /// Whether the document is a built-in artifact kind.
    pub fn is_builtin(&self) -> bool {
        self.api_version == API_VERSION_ARTIFACTS
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObservationType {
    Drift,
    Health,
}

impl ObservationType {
    /// `drift` or `health`, as the document's `type` field names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drift => "drift",
            Self::Health => "health",
        }
    }
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
    /// Set by `nyl lease break`: the holder is declared gone, so the next run
    /// takes over at once and records the break in its transition commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<Request>,
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

/// What a signal asks the running run to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SignalAction {
    /// `nyl teardown --confirm-removed`: the publication's removal is
    /// confirmed; bound to the phase 1 commit.
    ConfirmRemoved,
    /// `nyl hold`: the run starts no further work on the unit and writes
    /// the hold in its own transition commit.
    Hold,
}

/// A message to a running run: a teardown confirmation or a hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    #[serde(rename = "apiVersion")]
    pub api_version: StateV1,
    pub kind: SignalKind,
    pub run: RunId,
    pub action: SignalAction,
    pub unit: UnitName,
    /// The phase 1 commit a `confirm-removed` signal confirms; absent for a hold.
    #[serde(rename = "phase1Commit", default, skip_serializing_if = "Option::is_none")]
    pub phase1_commit: Option<CommitId>,
    pub by: String,
    pub identity: ApprovalIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: Timestamp,
    /// Where the signal came from, such as a CI run URL.
    pub from: String,
}

/// JSON Schemas of the state documents, published with the resource
/// references under `gitops.nyl/v1/state/`.
pub fn state_document_schemas() -> BTreeMap<String, serde_json::Value> {
    fn schema<T: JsonSchema>(title: &str) -> serde_json::Value {
        let mut schema = serde_json::to_value(schemars::schema_for!(T)).expect("schemas serialize");
        schema["title"] = serde_json::Value::String(title.to_owned());
        schema
    }
    let schemas: BTreeMap<_, _> = [
        ("state-record", schema::<StateRecord>("StateRecord")),
        ("environment-record", schema::<EnvironmentRecord>("EnvironmentRecord")),
        ("desired-unit", schema::<DesiredUnit>("DesiredUnit")),
        ("observed-unit", schema::<ObservedUnit>("ObservedUnit")),
        ("artifact", schema::<ArtifactDocument>("Artifact")),
        ("observation", schema::<Observation>("Observation")),
        ("attestation", schema::<Attestation>("Attestation")),
        ("lease", schema::<Lease>("Lease")),
        ("run", schema::<Run>("Run")),
        ("signal", schema::<Signal>("Signal")),
    ]
    .into_iter()
    .map(|(slug, schema)| (format!("{API_VERSION_GITOPS}/state/{slug}.schema.json"), schema))
    .collect();
    schemas
}
