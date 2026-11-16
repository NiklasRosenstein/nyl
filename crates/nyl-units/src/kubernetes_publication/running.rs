//! What the publication's `accepted` and `healthy` say runs, per deployment,
//! for promotion: the driver's side of decisions D6, D7, and D8 of the
//! [M6 plan](../../../../design/m6-plan.md#open-decisions).
//!
//! Contract: [Kubernetes publications](../../../../design/promotion.md#kubernetes-publications)
//! ("Coverage", "Manual syncs") and [Choosing the state](../../../../design/promotion.md#promoting-an-environments-source).
//!
//! - `accepted` and `healthy` are read from the reconciler that runs the
//!   target, so they say what runs now: they are the kind's running-scoped
//!   attestations ([`running_attestations`]).
//! - [`deployment_verdicts`] reads, from the details the shared evaluation
//!   wrote ([`super::reconciler::evaluate`]), what each deployment runs and
//!   whether it meets the attestation: a deployment that runs the attested
//!   receipt's publication or another publication of the target is
//!   accepted at what it runs, and healthy when, besides, it is `Healthy`;
//!   every other state (`unmatched`, `mixed`, `missing`, ...) runs nothing a
//!   receipt explains. Another publication carries both the matched commit
//!   and the running revision, and whether the match stopped at its walk
//!   limit (`publicationTruncated`).
//! - [`receipt_publication`] names a receipt's publication by its
//!   `PublishedTree` commit, the commit the details name a deployment's
//!   matched publication by.
//! - [`input_consumer`] maps a Release input of the resolved spec,
//!   `/releases/<group>~1<release>/…`, to the deployment of that Release.

use std::collections::BTreeMap;

use nyl_core::orchestration::driver::{DeploymentRuns, DeploymentVerdict};
use nyl_core::orchestration::state::{ArtifactDocument, Attestation};
use serde_json::Value;

use super::observe::{observes, RECONCILER};
use super::reconciler::evaluate::{ACCEPTED, HEALTHY};

/// `accepted` and `healthy`, for a unit that observes.
pub(crate) fn running_attestations(spec: &Value) -> Vec<String> {
    if observes(spec) {
        vec![ACCEPTED.to_owned(), HEALTHY.to_owned()]
    } else {
        Vec::new()
    }
}

/// Per deployment, what `attestation` (`accepted` or `healthy`) says it runs
/// and whether it meets the attestation there; `None` for another name or
/// details that list no deployments.
pub(crate) fn deployment_verdicts(attestation: &Attestation) -> Option<Vec<DeploymentVerdict>> {
    let health_required = match attestation.name.as_str() {
        ACCEPTED => false,
        HEALTHY => true,
        _ => return None,
    };
    let deployments = attestation.details.get("deployments")?.as_array()?;
    deployments
        .iter()
        .map(|entry| {
            let text = |key: &str| entry.get(key).and_then(Value::as_str);
            let name = text("id")?.to_owned();
            let state = text("state").unwrap_or("unreadable");
            let health = text("health");
            let runs = match state {
                "accepted" => DeploymentRuns::Receipt,
                "other-publication" => {
                    text("publication").map_or(DeploymentRuns::Unproven, |commit| DeploymentRuns::Publication {
                        publication: commit.to_owned(),
                        running: text("running").map(str::to_owned),
                        truncated: entry.get("publicationTruncated").and_then(Value::as_bool) == Some(true),
                    })
                }
                _ => DeploymentRuns::Unproven,
            };
            let healthy = health == Some("Healthy");
            let meets = runs != DeploymentRuns::Unproven && (!health_required || healthy);
            let reason = match &runs {
                DeploymentRuns::Unproven => Some(format!(
                    "{name} is {state}{}",
                    text("reason").map(|reason| format!(": {reason}")).unwrap_or_default()
                )),
                _ if !meets => Some(format!("{name} is {}", health.unwrap_or("not reporting health"))),
                DeploymentRuns::Publication { publication, .. } => {
                    Some(format!("{name} runs another publication ({publication})"))
                }
                DeploymentRuns::Receipt => None,
            };
            Some(DeploymentVerdict {
                group: text("release")
                    .and_then(|release| release.split_once('/'))
                    .map(|(group, _)| group.to_owned()),
                name,
                runs,
                meets,
                reason,
            })
        })
        .collect()
}

/// The commit of the receipt's `PublishedTree`.
pub(crate) fn receipt_publication(artifacts: &BTreeMap<String, ArtifactDocument>) -> Option<String> {
    artifacts
        .get("tree")
        .and_then(|tree| tree.spec.get("commit"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The deployment of the Release whose input `pointer` reads, from the
/// deployments the resolved spec recorded.
pub(crate) fn input_consumer(spec: &Value, pointer: &str) -> Option<String> {
    let token = pointer.strip_prefix("/releases/")?.split('/').next()?;
    let release = token.replace("~1", "/").replace("~0", "~");
    spec.pointer(&format!("{RECONCILER}/expected"))?
        .as_array()?
        .iter()
        .find(|deployment| deployment.get("release").and_then(Value::as_str) == Some(release.as_str()))
        .and_then(|deployment| deployment.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::state::{AttestationKind, AttestationResult, StateV1};
    use serde_json::json;

    use super::*;

    fn attestation(name: &str, deployments: &Value) -> Attestation {
        Attestation {
            api_version: StateV1,
            kind: AttestationKind,
            name: name.to_owned(),
            unit: None,
            execution_key: "sha256:k".to_owned(),
            result: AttestationResult::Fail,
            at: "2026-09-25T12:00:00Z".parse().unwrap(),
            source: json!({"driver": {}}),
            reason: None,
            details: json!({"deployments": deployments}),
        }
    }

    /// What the shared evaluation's details say each deployment runs, as
    /// promotion reads it: the attested publication, another one by its
    /// matched commit, or nothing a receipt explains; `healthy` also needs
    /// `Healthy`. Catches a verdict that treats another publication as
    /// unproven (value-only paths could never take an older value) or an
    /// unmatched deployment as running something.
    #[test]
    fn test_verdicts_read_what_each_deployment_runs() {
        let deployments = json!([
            {"id": "web", "release": "web/web", "state": "accepted", "health": "Degraded"},
            {"id": "api", "release": "web/api", "state": "other-publication", "health": "Healthy", "publication": "p1", "running": "r1", "publicationTruncated": true},
            {"id": "worker", "release": "jobs/worker", "state": "unmatched", "health": "Healthy", "reason": "edited"},
            {"id": "catalog", "state": "accepted", "health": "Healthy"},
        ]);
        let accepted = deployment_verdicts(&attestation(ACCEPTED, &deployments)).unwrap();
        assert_eq!(
            accepted.iter().map(|verdict| verdict.meets).collect::<Vec<_>>(),
            [true, true, false, true]
        );
        assert_eq!(
            accepted[1].runs,
            DeploymentRuns::Publication {
                publication: "p1".to_owned(),
                running: Some("r1".to_owned()),
                truncated: true,
            }
        );
        assert_eq!(accepted[1].group.as_deref(), Some("web"));
        assert_eq!(accepted[2].runs, DeploymentRuns::Unproven);
        assert_eq!(accepted[2].reason.as_deref(), Some("worker is unmatched: edited"));
        assert_eq!(accepted[3].group, None);
        let healthy = deployment_verdicts(&attestation(HEALTHY, &deployments)).unwrap();
        assert!(!healthy[0].meets);
        assert_eq!(healthy[0].reason.as_deref(), Some("web is Degraded"));
        assert_eq!(healthy[0].runs, DeploymentRuns::Receipt);
        assert!(deployment_verdicts(&attestation("qa", &json!([]))).is_none());
    }

    /// A Release input maps to its Release's deployment, with `~1` decoded;
    /// any other pointer has no deployment consumer.
    #[test]
    fn test_input_consumer_is_the_deployment_of_the_release() {
        let spec = json!({"reconciler": {"expected": [
            {"id": "catalog"},
            {"id": "web-api", "release": "web/api"},
        ]}});
        assert_eq!(
            input_consumer(&spec, "/releases/web~1api/image").as_deref(),
            Some("web-api")
        );
        assert_eq!(input_consumer(&spec, "/releases/web~1web/image"), None);
        assert_eq!(input_consumer(&spec, "/target"), None);
    }

    /// A receipt's publication is its `PublishedTree`'s commit, the commit
    /// a deployment's matched publication is compared with; a receipt
    /// without one names none. Catches a value-only path that can never
    /// find the state a deployment runs.
    #[test]
    fn test_receipt_publication_is_the_published_tree_commit() {
        let tree = |spec: Value| -> ArtifactDocument {
            serde_json::from_value(json!({
                "apiVersion": "artifacts.gitops.nyl/v1",
                "kind": "PublishedTree",
                "metadata": {"name": "tree", "unit": {
                    "environment": "dev", "name": "kubernetes", "uid": "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90"
                }},
                "executionKey": format!("sha256:{}", "a".repeat(64)),
                "spec": spec,
            }))
            .unwrap()
        };
        let artifacts = BTreeMap::from([("tree".to_owned(), tree(json!({"commit": "c0ffee", "branch": "deploy"})))]);
        assert_eq!(receipt_publication(&artifacts).as_deref(), Some("c0ffee"));
        assert_eq!(receipt_publication(&BTreeMap::new()), None);
        let without = BTreeMap::from([("tree".to_owned(), tree(json!({"branch": "deploy"})))]);
        assert_eq!(receipt_publication(&without), None);
    }
}
