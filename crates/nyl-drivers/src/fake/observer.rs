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
        self.targets
            .lock()
            .expect("observer lock")
            .insert(target.to_owned(), observation);
    }
}

impl ArgoObserver for FakeObserver {
    /// What a test set for `target`; a target nothing was set for has no
    /// workload Applications left.
    fn workload_applications(&self, target: &str) -> ApplicationsObservation {
        self.targets
            .lock()
            .expect("observer lock")
            .get(target)
            .cloned()
            .unwrap_or(ApplicationsObservation::Remaining(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches clones observing separately, which would let a scenario's
    /// script miss the observer the run holds.
    #[test]
    fn test_clones_share_scripted_observations_and_default_to_removed() {
        let observer = FakeObserver::new();
        let run_side = observer.clone();
        assert_eq!(
            run_side.workload_applications("dev"),
            ApplicationsObservation::Remaining(Vec::new())
        );
        observer.set("dev", ApplicationsObservation::Remaining(vec!["web".to_owned()]));
        assert_eq!(
            run_side.workload_applications("dev"),
            ApplicationsObservation::Remaining(vec!["web".to_owned()])
        );
        assert_eq!(
            run_side.workload_applications("prod"),
            ApplicationsObservation::Remaining(Vec::new())
        );
    }
}
