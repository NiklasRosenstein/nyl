//! `FakeObserver`, standing in for Argo CD in tier-1 scenarios, as a test
//! scripts it per target:
//!
//! - which workload Applications of a target still exist, for
//!   `FakePublication`'s `observe` teardown wait;
//! - what each Application of a target runs and its health
//!   ([`ScriptedReading`]), for `FakePublication`'s `attest`. The observer
//!   turns it into the neutral [`Reading`] a real reader makes, and the fake
//!   attests it through the real driver's path from a reading to `accepted`
//!   and `healthy` ([`crate::kubernetes_publication::reconciler::evaluate`]),
//!   so the results and their per-Application details are the real rules'.
//!
//! The real driver reads Argo CD itself ([`crate::kubernetes_publication::argocd`]).
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown (`teardownWait: {strategy: observe}`),
//! [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications), and
//! [Fake kinds](../../../../design/reference-scenarios.md#fake-kinds).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nyl_core::orchestration::driver::RemovalObservation;
use nyl_render::gitops::observation::{Expected, ExpectedDeployment};

use crate::kubernetes_publication::reconciler::{
    Health, Observed, Proof, ReadError, ReadErrorKind, Reading, Running, Visibility,
};

/// What the fake reconciler reports for one deployment: the revision it
/// runs and its health. Its source is always the one Nyl generated, and it
/// is never being deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedDeployment {
    pub running: Running,
    pub health: Health,
}

/// What the fake reconciler reports for a target's deployments: `releases`
/// by `<group>/<release>`, and `all` for every other one, the catalog
/// included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedReading {
    pub all: ScriptedDeployment,
    pub releases: BTreeMap<String, ScriptedDeployment>,
}

impl ScriptedReading {
    /// What a deployment reports.
    fn of(&self, deployment: &ExpectedDeployment) -> &ScriptedDeployment {
        deployment
            .release
            .as_ref()
            .and_then(|release| self.releases.get(release))
            .unwrap_or(&self.all)
    }

    /// The neutral reading of every deployment `expected` names: each one
    /// present with the source Nyl generated, every anchor visible, so the
    /// reading proves what it holds.
    pub fn reading(&self, expected: &Expected) -> Reading {
        Reading {
            deployments: expected
                .deployments
                .iter()
                .map(|deployment| {
                    let reported = self.of(deployment);
                    (
                        deployment.id.clone(),
                        Observed {
                            running: reported.running.clone(),
                            health: reported.health,
                            reported: None,
                            deleting: false,
                            terminal: false,
                            source: Some(deployment.source.clone()),
                            synced_source: Some(deployment.source.clone()),
                        },
                    )
                })
                .collect(),
            anchors: expected
                .anchors
                .iter()
                .map(|anchor| (anchor.id.clone(), Visibility::Visible))
                .collect(),
            absent: BTreeMap::new(),
            proof: Proof::PerDeployment,
            unprovable: None,
        }
    }
}

/// Scripted Argo CD observations by target name, shared by every clone.
#[derive(Debug, Clone, Default)]
pub struct FakeObserver {
    targets: Arc<Mutex<BTreeMap<String, RemovalObservation>>>,
    readings: Arc<Mutex<BTreeMap<String, ScriptedReading>>>,
}

impl FakeObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the observer reports for `target` from now on.
    pub fn set(&self, target: &str, observation: RemovalObservation) {
        self.targets
            .lock()
            .expect("observer lock")
            .insert(target.to_owned(), observation);
    }
}

impl FakeObserver {
    /// What the fake reconciler reports for `target`'s deployments from now on.
    pub fn set_reading(&self, target: &str, reading: ScriptedReading) {
        self.readings
            .lock()
            .expect("observer lock")
            .insert(target.to_owned(), reading);
    }

    /// The reading of `target`'s `expected` deployments, made from what a
    /// test scripted; a target nothing was scripted for cannot be read, so
    /// its attestation is unknown, never a failure.
    pub fn reading(&self, target: &str, expected: &Expected) -> Result<Reading, ReadError> {
        self.readings
            .lock()
            .expect("observer lock")
            .get(target)
            .map(|scripted| scripted.reading(expected))
            .ok_or_else(|| {
                ReadError::new(
                    ReadErrorKind::Unreachable,
                    format!("the fake Argo CD reports nothing for target {target}: script it with an argocd step"),
                )
            })
    }

    /// What a test set for `target`; a target nothing was set for has no
    /// workload Applications left.
    pub fn workload_applications(&self, target: &str) -> RemovalObservation {
        self.targets
            .lock()
            .expect("observer lock")
            .get(target)
            .cloned()
            .unwrap_or(RemovalObservation::Remaining(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nyl_render::gitops::observation::{Anchor, DeploymentRole, ObservedPublication, Source};
    use serde_json::{json, Value};

    fn deployment(role: DeploymentRole, id: &str, release: Option<&str>) -> ExpectedDeployment {
        ExpectedDeployment {
            id: id.to_owned(),
            display: String::new(),
            role,
            path: format!("dev/{id}"),
            release: release.map(str::to_owned),
            source: Source {
                repository: "https://example.invalid/deploy.git".to_owned(),
                path: format!("dev/{id}"),
                options: json!({"directory": {"recurse": true}}),
            },
            locator: Value::Null,
        }
    }

    fn expected() -> Expected {
        Expected {
            publication: ObservedPublication {
                url: "https://example.invalid/deploy.git".to_owned(),
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            },
            deployments: vec![
                deployment(DeploymentRole::Catalog, "dev-catalog", None),
                deployment(DeploymentRole::Workload, "api", Some("web/api")),
                deployment(DeploymentRole::Workload, "web", Some("web/web")),
            ],
            anchors: vec![Anchor {
                id: "dev-catalog".to_owned(),
                locator: Value::Null,
            }],
        }
    }

    fn runs(revision: &str, health: Health) -> ScriptedDeployment {
        ScriptedDeployment {
            running: Running::Exact(revision.to_owned()),
            health,
        }
    }

    /// A Release named in the script reports its own reading and every
    /// other deployment, the catalog included, reports `all`; each runs the
    /// source Nyl generated and every anchor is visible, so the shared rules
    /// judge only what was scripted. Catches a reading that would make the
    /// rules report `other-source` or an unprovable absence instead.
    #[test]
    fn test_scripted_reading_covers_every_expected_deployment() {
        let observer = FakeObserver::new();
        let expected = expected();
        observer.set_reading(
            "dev",
            ScriptedReading {
                all: runs("p2", Health::Healthy),
                releases: BTreeMap::from([("web/api".to_owned(), runs("p1", Health::Degraded))]),
            },
        );
        let reading = observer.reading("dev", &expected).unwrap();
        let running = |id: &str| {
            let deployment = &reading.deployments[id];
            (deployment.running.clone(), deployment.health)
        };
        assert_eq!(
            running("dev-catalog"),
            (Running::Exact("p2".to_owned()), Health::Healthy)
        );
        assert_eq!(running("web"), (Running::Exact("p2".to_owned()), Health::Healthy));
        assert_eq!(running("api"), (Running::Exact("p1".to_owned()), Health::Degraded));
        for deployment in &expected.deployments {
            let reported = &reading.deployments[&deployment.id];
            assert_eq!(reported.source.as_ref(), Some(&deployment.source));
            assert_eq!(reported.synced_source.as_ref(), Some(&deployment.source));
        }
        assert_eq!(reading.anchors["dev-catalog"], Visibility::Visible);
        assert!(crate::kubernetes_publication::reconciler::evaluate::ensure_provable(
            &expected,
            expected.deployments.iter(),
            &reading,
            crate::kubernetes_publication::reconciler::evaluate::Conclusion::Presence,
        )
        .is_ok());
    }

    /// A target nothing was scripted for cannot be read, so the fake's
    /// attestation is unknown instead of a failure that only says "could
    /// not look".
    #[test]
    fn test_unscripted_target_cannot_be_read() {
        let error = FakeObserver::new().reading("dev", &expected()).unwrap_err();
        assert_eq!(error.kind, ReadErrorKind::Unreachable);
        assert!(error.message.contains("argocd step"), "{}", error.message);
    }

    /// Catches clones observing separately, which would let a scenario's
    /// script miss the observer the run holds.
    #[test]
    fn test_clones_share_scripted_observations_and_default_to_removed() {
        let observer = FakeObserver::new();
        let run_side = observer.clone();
        assert_eq!(
            run_side.workload_applications("dev"),
            RemovalObservation::Remaining(Vec::new())
        );
        observer.set("dev", RemovalObservation::Remaining(vec!["web".to_owned()]));
        assert_eq!(
            run_side.workload_applications("dev"),
            RemovalObservation::Remaining(vec!["web".to_owned()])
        );
        assert_eq!(
            run_side.workload_applications("prod"),
            RemovalObservation::Remaining(Vec::new())
        );
    }
}
