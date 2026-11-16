//! What a unit's running-scoped attestations say, per recorded state, as
//! the unit's driver decides it from its own details.
//!
//! Contract: [Choosing the state](../../../../../design/promotion.md#promoting-an-environments-source)
//! and [Kubernetes publications](../../../../../design/promotion.md#kubernetes-publications)
//! ("Coverage", "Manual syncs"); decisions D6, D7, and D8 of the
//! [M6 plan](../../../../../design/m6-plan.md#open-decisions).
//!
//! The engine never reads a driver's attestation `details`. The shell asks
//! each unit's driver ([`Driver::running_attestations`],
//! [`Driver::deployment_verdicts`], [`Driver::receipt_publication`], and
//! [`Driver::input_consumer`]) when it reads the source's history, and the
//! rules ([`super::standing`]) judge the typed answers. Nothing here is
//! persisted: it is computed again from the recorded attestations whenever
//! promotion reads them, so the state format does not change.
//!
//! [`Driver::running_attestations`]: nyl_core::orchestration::driver::Driver::running_attestations
//! [`Driver::deployment_verdicts`]: nyl_core::orchestration::driver::Driver::deployment_verdicts
//! [`Driver::receipt_publication`]: nyl_core::orchestration::driver::Driver::receipt_publication
//! [`Driver::input_consumer`]: nyl_core::orchestration::driver::Driver::input_consumer

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::DeploymentVerdict;

/// One unit of one recorded state, as its driver reads its running-scoped
/// attestations. Empty for a kind that declares none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitRunning {
    /// The attestation names that say what runs now, for the unit's spec
    /// in the state.
    pub names: BTreeSet<String>,
    /// What the state's receipt of the unit produced, as a deployment's
    /// [`DeploymentRuns::Publication`](nyl_core::orchestration::driver::DeploymentRuns::Publication)
    /// names it.
    pub publication: Option<String>,
    /// Per running-scoped attestation recorded for the state's receipt, what
    /// it says about each deployment; absent when the driver cannot tell.
    pub verdicts: BTreeMap<String, Vec<DeploymentVerdict>>,
}
