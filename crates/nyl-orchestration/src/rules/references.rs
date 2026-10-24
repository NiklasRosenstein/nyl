//! The one resolver of `fromUnit` references.
//!
//! Contract: [References](../../../../design/orchestration-core.md#references),
//! [Desired unit](../../../../design/orchestration-core.md#desired-unit)
//! (provenance, readiness), and consumer freshness in
//! [Observed unit](../../../../design/orchestration-core.md#observed-unit).
//!
//! Unit specs, KubernetesPublication `releases`, `nyl get output`/`get
//! artifact`, and later PromotionPath selectors resolve through
//! [`resolve_reference`], so they cannot disagree.
//!
//! - A reference resolves only against the producer's current receipt: its
//!   uid and execution key equal the producer's desired document as this
//!   resolution sees it ([`super::freshness`]). A producer whose desired
//!   document changed blocks its consumers, even ones with receipts.
//! - An output reference reads a declared, non-sensitive output; an artifact
//!   reference reads an artifact the receipt lists, after checking the
//!   artifact document's digest against the receipt. `pointer` (RFC 6901)
//!   selects inside the output or the artifact's `spec`; `kind` must match.
//! - A unit outside the ownership set, an undeclared or sensitive output, a
//!   kind mismatch, or a value of the wrong type for the declared output is a
//!   resolution error ([`Code::ReferenceInvalid`](nyl_core::orchestration::codes::Code::ReferenceInvalid)).
//!   A producer without a current receipt blocks the reference instead.
//! - Provenance cites the receipt: `{unit, uid, executionKey, observedCommit}`
//!   for a receipt read from the snapshot, or `{…, run}` for one produced
//!   earlier in the same run; never a checkpoint or the commit being written.
//! - A citation is stable: when a reference resolves to the receipt (unit,
//!   uid, executionKey) that the stored desired document already cites at
//!   that pointer, the stored citation (its `run` or `observedCommit`) is
//!   kept unchanged. Re-resolving against unchanged receipts therefore
//!   yields byte-identical provenance, and a run that changes nothing writes
//!   nothing, although the observed tip and the run ID differ.
//! - The artifact digest is [`ArtifactDocument::digest`], the one function
//!   receipts are written and checked with.
//! - Cross-environment references (`environment:`) are rejected until M6.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::bindings::UnitInputSource;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, JsonPointer, RunId, UnitName};
use nyl_core::orchestration::state::{
    ArtifactDocument, DesiredUnit, ObservedUnit, Provenance, Readiness, ReceiptProvenance,
};
use serde_json::Value;

/// What references resolve against.
pub struct ReferenceView<'a> {
    pub environment: &'a EnvironmentName,
    /// Desired documents of the ownership set as this resolution sees them:
    /// producers already resolved in this pass carry their new execution keys.
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    /// Observed documents: the snapshot's, overlaid with this run's checkpoints.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    /// Artifacts of the latest receipts, overlaid likewise.
    pub artifacts: &'a BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    /// The observed commit the snapshot was read at, cited by provenance.
    pub observed_commit: Option<&'a CommitId>,
    /// This run, when receipts of `produced_in_run` came from it.
    pub run: Option<&'a RunId>,
    pub produced_in_run: &'a BTreeSet<UnitName>,
    /// The snapshot's stored desired documents, whose provenance
    /// [`resolve_unit`] keeps where it still cites the same receipt.
    pub previous: &'a BTreeMap<UnitName, DesiredUnit>,
}

/// What one reference found.
#[derive(Debug, Clone, PartialEq)]
pub enum ReferenceValue {
    Value {
        value: Value,
        provenance: ReceiptProvenance,
    },
    /// The producer has no current receipt yet.
    Blocked { unit: UnitName, reason: String },
}

/// Resolve one reference. `Err` is a resolution error, reported with
/// [`Code::ReferenceInvalid`](nyl_core::orchestration::codes::Code::ReferenceInvalid).
pub fn resolve_reference(view: &ReferenceView<'_>, source: &UnitInputSource) -> Result<ReferenceValue, String> {
    let _ = (view, source);
    todo!("references-graph")
}

/// A unit's spec with its references resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The spec with every reference replaced, or `None` while blocked.
    pub resolved_spec: Option<Value>,
    pub provenance: BTreeMap<JsonPointer, Provenance>,
    /// `Blocked` lists each blocked pointer with its reason.
    pub readiness: Readiness,
    /// Resolution errors, each naming the pointer.
    pub errors: Vec<String>,
}

/// Resolve every reference of `unit.spec` through
/// [`nyl_core::bindings::resolve_document`] with providers backed by
/// [`resolve_reference`]. Where `view.previous[unit]` cites the same
/// receipt (unit, uid, executionKey) at a pointer, that stored citation is
/// returned instead of a new one. An explicit `dependsOn` entry whose
/// producer has no current receipt blocks at `/dependsOn/<index>`.
pub fn resolve_unit(view: &ReferenceView<'_>, unit: &DesiredUnit) -> Resolution {
    let _ = (view, unit);
    todo!("references-graph")
}
