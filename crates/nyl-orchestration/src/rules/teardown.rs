//! Which `deleting` units a run tears down, in which order, and why the
//! others wait.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Teardown" (reverse creation order: a unit is torn down only after every
//! deleting unit that depends on it; a publication's `teardownWait` gates
//! the units it depends on, transitively; units without a path to a waiting
//! unit proceed in the same run; omission and kind-change teardown need
//! `--allow-teardown`), "Retained dependents" (`NYL-TEARDOWN-HAS-DEPENDENTS`
//! unless `--allow-dependents`), "Explicit teardown" and "Holds" (a held
//! unit never progresses: an explicit teardown of it only records the
//! request, see [`super::lifecycle`]), [Recovery](../../../../design/orchestration-core.md#recovery)
//! (the driver's teardown recovery policy), and
//! [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! teardown readiness (an unmet blocking requirement keeps the unit pending
//! unless `--allow-incomplete`).
//!
//! One uniform reverse-dependency rule (part 5 decision Q6): every drop
//! (a release, or a kind change's removal of a tombstone) and every declared
//! retention waits for its deleting dependents exactly as a teardown does,
//! so this plan covers every unit leaving with any intent, and the run
//! writes a drop or retention only once it is in [`TeardownPlan::ready`].
//!
//! Dependencies between deleting units come from their stored specs
//! ([`dependencies`]), not from a resolution of source, so a teardown never
//! needs the units' current declarations.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit};

/// What the invocation allows.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct TeardownOptions<'a> {
    /// `--allow-teardown`: omission and kind-change teardowns proceed.
    pub allow_teardown: bool,
    /// `--allow-dependents`: a unit a retained tombstone cites is torn down anyway.
    pub allow_dependents: bool,
    /// `--allow-incomplete`: an unmet blocking readiness requirement does not stop the teardown.
    pub allow_incomplete: bool,
    /// `nyl teardown --unit`: only these units are torn down; `None` acts on
    /// every deleting unit the invocation may tear down.
    pub named: Option<&'a BTreeSet<UnitName>>,
    /// An explicitly requested teardown (`nyl teardown`, `state delete
    /// --teardown`), which needs no `--allow-teardown`. Held units are
    /// pending either way.
    pub explicit: bool,
}

/// What deciding a run's teardowns reads.
pub struct TeardownInputs<'a> {
    pub environment: &'a EnvironmentName,
    /// Every desired document after this run's lifecycle changes: deleting,
    /// held, retained, and active units.
    pub desired: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The observed documents, with this run's checkpoints.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    pub drivers: &'a DriverSet,
    pub options: TeardownOptions<'a>,
    /// Units this run already tore down or tried to.
    pub attempted: &'a BTreeSet<UnitName>,
    /// Units whose teardown waits under a `teardownWait` that has not
    /// completed; the units they depend on wait with them.
    pub waiting: &'a BTreeSet<UnitName>,
    /// Unmet blocking readiness requirements per unit, from the preflight
    /// ([`super::teardown_readiness`]).
    pub readiness: &'a BTreeMap<UnitName, Vec<Finding>>,
}

/// Why a deleting unit is not torn down in this wave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Omission or kind change without `--allow-teardown`.
    NeedsAllowTeardown,
    /// Retained tombstones cite it, without `--allow-dependents`.
    HasDependents { tombstones: Vec<UnitName> },
    /// Deleting units that depend on it are not torn down yet.
    Dependents { units: Vec<UnitName> },
    /// A publication it is a dependency of, directly or transitively, waits under its `teardownWait`.
    Waiting { units: Vec<UnitName> },
    /// Held: a held unit never progresses; a teardown request on it is
    /// recorded and carried out after `nyl resume`.
    Held,
    /// Unmet blocking readiness requirements, without `--allow-incomplete`.
    Readiness { findings: Vec<Finding> },
    /// An uncertain teardown whose teardown recovery policy is not `converge`.
    Uncertain,
    /// A non-retryable failed teardown waits for `nyl recover --retry`.
    FailedNonRetryable,
    /// Not named by `nyl teardown --unit`.
    NotNamed,
    /// Already torn down or tried in this run.
    AlreadyAttempted,
    /// The unit declares `approval: manual`; teardown approvals (`nyl
    /// teardown --approve`, destroy-plan digests) arrive with M3 part 6, so
    /// until then it waits (exit 2, open question Q50).
    AwaitingApproval,
}

/// The decision of one wave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeardownPlan {
    /// Units to tear down now, in order: consumers before producers.
    pub ready: Vec<UnitName>,
    /// Every other deleting unit and why it waits.
    pub pending: BTreeMap<UnitName, Pending>,
    /// Units torn down although retained tombstones cite them
    /// (`--allow-dependents`), recorded in the transition commit.
    pub overrides: BTreeMap<UnitName, Vec<UnitName>>,
}

/// Decide which deleting units to tear down now.
pub fn plan(inputs: &TeardownInputs<'_>) -> TeardownPlan {
    let _ = inputs;
    todo!("lifecycle")
}

/// The retained tombstones whose receipt cites `unit`'s incarnation in its
/// provenance, which a teardown of `unit` would leave without their producer.
pub fn citing_tombstones(desired: &BTreeMap<UnitName, DesiredUnit>, unit: &DesiredUnit) -> Vec<UnitName> {
    let _ = (desired, unit);
    todo!("lifecycle")
}

/// The dependencies among `units` from their stored specs (`fromUnit`
/// references and `dependsOn`), ignoring producers outside `units`: for
/// each unit, the units it depends on.
pub fn dependencies(units: &BTreeMap<UnitName, DesiredUnit>) -> BTreeMap<UnitName, BTreeSet<UnitName>> {
    let _ = units;
    todo!("lifecycle")
}

/// The warning a pending unit reports, with its stable code and fix, such
/// as `NYL-TEARDOWN-PENDING` naming `--allow-teardown` or
/// `NYL-TEARDOWN-HAS-DEPENDENTS` naming the tombstones and `--allow-dependents`.
pub fn finding(environment: &EnvironmentName, unit: &UnitName, pending: &Pending) -> Option<Finding> {
    let _ = (environment, unit, pending);
    todo!("lifecycle")
}
