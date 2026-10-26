//! `FakeObserver`, standing in for Argo CD in M3: reports which workload
//! Applications of a target still exist, as a test scripts them, for a
//! publication's `observe` teardown wait.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown (`teardownWait: {strategy: observe}`), and
//! [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::ports::{ApplicationsObservation, ArgoObserver};

/// Scripted Argo CD observations by target name, shared by every clone.
#[derive(Debug, Clone, Default)]
pub struct FakeObserver {
    targets: Arc<Mutex<BTreeMap<String, ApplicationsObservation>>>,
}

impl FakeObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the observer reports for `target` from now on.
    pub fn set(&self, target: &str, observation: ApplicationsObservation) {
        let _ = (target, observation);
        todo!("publication")
    }
}

impl ArgoObserver for FakeObserver {
    fn workload_applications(&self, target: &str) -> ApplicationsObservation {
        let _ = (target, &self.targets);
        todo!("publication")
    }
}
