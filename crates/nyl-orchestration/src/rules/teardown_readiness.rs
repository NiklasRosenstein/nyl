//! Whether a publication's teardown can complete, from its target's
//! effective settings: the preflight every teardown runs.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! teardown readiness and catalog ownership. Three blocking requirements
//! decide whether the teardown can succeed (the catalog prunes removed
//! workload Applications on its own, `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`;
//! deleting an Application deletes its workloads,
//! `NYL-TEARDOWN-APPLICATIONS-ORPHANED`; owned Namespaces do not stop
//! deletion, `NYL-TEARDOWN-NAMESPACE-CONFIRM`); two remnant rows leave
//! objects behind (`NYL-TEARDOWN-NAMESPACES-REMAIN`,
//! `NYL-TEARDOWN-CATALOG-REMAINS`). Settings a parent Application owns are
//! `external: not checked` and never block. An unmet blocking requirement
//! keeps the unit pending teardown unless the invocation passes
//! `--allow-incomplete`, and an incomplete teardown is recorded with a
//! condition listing what may remain.

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::TeardownSettings;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};

/// The preflight's verdict for one unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readiness {
    /// Unmet blocking requirements, each with the setting that fixes it.
    pub blocking: Vec<Finding>,
    /// What teardown leaves behind.
    pub remnants: Vec<Finding>,
    /// Requirements Nyl cannot see, such as a parent's catalog settings.
    pub not_checked: Vec<String>,
}

impl Readiness {
    /// Whether no blocking requirement is unmet.
    pub fn ready(&self) -> bool {
        self.blocking.is_empty()
    }
}

/// Evaluate a unit's teardown settings.
pub fn evaluate(environment: &EnvironmentName, unit: &UnitName, settings: &TeardownSettings) -> Readiness {
    let _ = (environment, unit, settings);
    todo!("publication")
}
