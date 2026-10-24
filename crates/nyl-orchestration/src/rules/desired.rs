//! Desired documents: unit selection, the ownership set, incarnations, and
//! the environment record.
//!
//! Contract: [Units](../../../../design/orchestration-core.md#units),
//! [Identity](../../../../design/orchestration-core.md#identity), and
//! [Desired unit](../../../../design/orchestration-core.md#desired-unit).
//!
//! - The renderer selects units on their literal envelope and renders each
//!   selected spec once; a document that renders to nothing, or whose
//!   `enabled` is false after rendering, is not in the ownership set.
//! - Every unit's kind must have a driver, and its spec must pass the
//!   driver's `validate_spec` and the common fields' validation.
//! - A unit keeps the uid of its desired document while its `apiVersion` and
//!   `kind` stay; a new unit, or one whose kind changed, gets a new uid from
//!   the caller. (Ending the old incarnation is part 5.)
//! - Ownership scopes of the ownership set must not overlap
//!   ([`Code::UnitOwnershipOverlap`](nyl_core::orchestration::codes::Code::UnitOwnershipOverlap)).
//! - Units in the snapshot's desired state that leave the ownership set are
//!   reported as leaving; their deletion is part 5.

use std::collections::BTreeMap;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{CommitId, Uid, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, RenderedSource, RenderedUnit};
use nyl_core::orchestration::state::{DesiredUnit, EnvironmentRecord, SourceChoice};

use crate::run_report::LeavingUnit;

/// What desiring one run's units reads.
pub struct DesireInputs<'a> {
    /// Units rendered at S, with the Environment as S declares it.
    pub source: &'a RenderedSource,
    /// The Environment spec the record keeps: S's Environment with group
    /// defaults applied and the entry worktree's `source`, `state`,
    /// `protectedRefs`, `allowUnprotectedSource`, and `admission`.
    pub environment_spec: &'a serde_json::Value,
    /// How S was chosen, recorded in the environment record.
    pub source_choice: SourceChoice,
    pub snapshot: &'a EnvironmentSnapshot,
    pub drivers: &'a DriverSet,
}

/// The desired state of one run before references are resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub environment: EnvironmentRecord,
    /// Every unit of the ownership set: `spec` rendered with references kept
    /// as expressions, `resolved_spec`, `execution_key`, and `provenance`
    /// empty, `readiness` ready, lifecycle active, `source_commit` and
    /// `source_revision` set to S, and a uid.
    pub units: BTreeMap<UnitName, DesiredUnit>,
    /// Units of the snapshot's desired state outside the ownership set.
    pub leaving: Vec<LeavingUnit>,
    /// Errors that stop the run before anything executes (exit 1).
    pub errors: Vec<Finding>,
}

/// The selected, enabled units: those rendered to a spec whose `enabled`
/// is not `false`.
pub fn ownership_set(source: &RenderedSource) -> BTreeMap<&UnitName, &RenderedUnit> {
    let _ = source;
    todo!("run-model")
}

/// Build the desired documents of one run. `new_uid` allocates a uid for a
/// new incarnation; it is the only identifier source, so the result is a
/// function of the inputs and the identifiers it hands out.
pub fn desire(inputs: &DesireInputs<'_>, new_uid: &mut dyn FnMut() -> Uid) -> Desired {
    let _ = (inputs, new_uid);
    todo!("run-model")
}

/// The source commit S every document of a run records.
pub fn source_commit(desired: &Desired) -> &CommitId {
    &desired.environment.source_commit
}
