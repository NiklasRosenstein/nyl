//! The decisions of the scenario, as pure functions over the JSON the cluster
//! and `nyl` print: is the Deployment running the new revision, did the
//! attestations pass, have the observed values arrived, has Argo CD synced
//! the commit. Each returns [`Poll::Pending`] with the reason it is not so
//! yet, which a timed-out wait then reports.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{Context, Error, Result};
use crate::nyl::unit;
use crate::wait::Poll;

/// Whether the Deployment's pod template runs `image` with `database_host`,
/// and every replica of it is updated, ready, and available (the rollout is
/// over, so no pod of the previous revision remains).
pub fn deployment_ready(deployment: &Value, image: &str, database_host: &str) -> Poll<()> {
    let container = deployment.pointer("/spec/template/spec/containers/0");
    let template_image = container
        .and_then(|container| container.get("image"))
        .and_then(Value::as_str);
    if template_image != Some(image) {
        return Poll::pending(format!(
            "the pod template runs image {} instead of {image}",
            template_image.unwrap_or("(none)")
        ));
    }
    let host = container
        .and_then(|container| container.get("env"))
        .and_then(Value::as_array)
        .and_then(|variables| {
            variables
                .iter()
                .find(|variable| variable.get("name").and_then(Value::as_str) == Some("DATABASE_HOST"))
        })
        .and_then(|variable| variable.get("value"))
        .and_then(Value::as_str);
    if host != Some(database_host) {
        return Poll::pending(format!(
            "DATABASE_HOST is {} instead of {database_host}",
            host.unwrap_or("(unset)")
        ));
    }
    let generation = deployment
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let observed = deployment
        .pointer("/status/observedGeneration")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if observed < generation {
        return Poll::pending(format!(
            "the controller has observed generation {observed} of {generation}"
        ));
    }
    let desired = deployment
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    for field in ["replicas", "updatedReplicas", "readyReplicas", "availableReplicas"] {
        let count = deployment
            .pointer(&format!("/status/{field}"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if count != desired {
            return Poll::pending(format!("{field} is {count} of {desired}"));
        }
    }
    Poll::Ready(())
}

/// Whether `nyl attest` recorded `accepted` and `healthy` as passing for `unit`.
pub fn attestation_ready(report: &Value, unit_name: &str) -> Poll<()> {
    let Some(entry) = unit(report, unit_name) else {
        return Poll::pending(format!("the report has no unit {unit_name}"));
    };
    let check = entry.get("check").and_then(Value::as_str).unwrap_or("(none)");
    if check != "attested" {
        let reason = entry.get("reason").and_then(Value::as_str).unwrap_or("");
        return Poll::pending(format!("the check is {check}: {reason}"));
    }
    for name in ["accepted", "healthy"] {
        match entry.pointer(&format!("/attestations/{name}")) {
            Some(Value::String(result)) if result == "pass" => {}
            other => {
                return Poll::pending(format!(
                    "attestation {name} is {}",
                    other.map_or("(absent)".to_owned(), Value::to_string)
                ))
            }
        }
    }
    Poll::Ready(())
}

/// The live values the publication unit observes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    pub ready_replicas: i64,
    pub service_cluster_ip: String,
}

/// Whether `nyl observe` read both declared observations of `unit_name`.
pub fn observations_ready(report: &Value, unit_name: &str) -> Poll<Observed> {
    let Some(entry) = unit(report, unit_name) else {
        return Poll::pending(format!("the report has no unit {unit_name}"));
    };
    let check = entry.get("check").and_then(Value::as_str).unwrap_or("(none)");
    if check != "observed" {
        let reason = entry.get("reason").and_then(Value::as_str).unwrap_or("");
        return Poll::pending(format!("the check is {check}: {reason}"));
    }
    let mut values = Vec::new();
    for name in ["readyReplicas", "serviceClusterIP"] {
        let observation = entry.pointer(&format!("/observations/{name}"));
        match observation
            .and_then(|value| value.get("observation"))
            .and_then(Value::as_str)
        {
            Some("value") => values.push(observation.and_then(|value| value.get("value")).cloned()),
            Some(status) => {
                let reason = observation
                    .and_then(|value| value.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                return Poll::pending(format!("observation {name} is {status}: {reason}"));
            }
            None => return Poll::pending(format!("observation {name} is absent from the report")),
        }
    }
    let ready_replicas = values[0].as_ref().and_then(Value::as_i64);
    let service_cluster_ip = values[1].as_ref().and_then(Value::as_str);
    match (ready_replicas, service_cluster_ip) {
        (Some(ready_replicas), Some(service_cluster_ip)) if !service_cluster_ip.is_empty() => Poll::Ready(Observed {
            ready_replicas,
            service_cluster_ip: service_cluster_ip.to_owned(),
        }),
        _ => Poll::pending(format!("observed values have the wrong types: {values:?}")),
    }
}

/// Whether the Application is Synced to `head` and Healthy.
pub fn application_converged(application: &Value, head: &str) -> Poll<()> {
    let sync = application
        .pointer("/status/sync/status")
        .and_then(Value::as_str)
        .unwrap_or("(none)");
    let revision = application
        .pointer("/status/sync/revision")
        .and_then(Value::as_str)
        .unwrap_or("(none)");
    let health = application
        .pointer("/status/health/status")
        .and_then(Value::as_str)
        .unwrap_or("(none)");
    if sync == "Synced" && revision == head && health == "Healthy" {
        Poll::Ready(())
    } else {
        Poll::pending(format!(
            "sync {sync} at {}, health {health}, want Synced at {} and Healthy{}",
            short(revision),
            short(head),
            operation_summary(application)
        ))
    }
}

/// Whether the suite should start a sync of the Application: it is not
/// Synced to `head` and no operation is queued or running (a manual sync is
/// an `operation` on the Application, which Argo CD clears when it ends).
pub fn needs_sync(application: &Value, head: &str) -> bool {
    let synced = application.pointer("/status/sync/status").and_then(Value::as_str) == Some("Synced")
        && application.pointer("/status/sync/revision").and_then(Value::as_str) == Some(head);
    let queued = application
        .get("operation")
        .is_some_and(|operation| !operation.is_null());
    let running = application
        .pointer("/status/operationState/phase")
        .and_then(Value::as_str)
        == Some("Running");
    !synced && !queued && !running
}

fn operation_summary(application: &Value) -> String {
    match (
        application
            .pointer("/status/operationState/phase")
            .and_then(Value::as_str),
        application
            .pointer("/status/operationState/message")
            .and_then(Value::as_str),
    ) {
        (Some(phase), Some(message)) => format!("; last operation {phase}: {message}"),
        (Some(phase), None) => format!("; last operation {phase}"),
        _ => String::new(),
    }
}

fn short(revision: &str) -> &str {
    revision.get(..8).unwrap_or(revision)
}

/// One attestation of an `AttestationRecord` in the observed state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedAttestation {
    /// `pass`, `fail`, ...
    pub result: String,
    /// Who recorded it: `attest` for `nyl attest`, `reconcile` for the wait of
    /// `observe.waitHealthy`.
    pub driver: String,
}

/// The attestations of an `observed/units/<unit>/attestations.yaml` file, by
/// name. The file is Nyl's own, written in one fixed layout (see the golden
/// file `attestation-record.v1.yaml`), so a line scan is enough.
pub fn parse_attestation_record(text: &str) -> BTreeMap<String, RecordedAttestation> {
    let mut found = BTreeMap::new();
    let mut in_block = false;
    let mut name: Option<String> = None;
    let mut result = None;
    let mut driver = None;
    let mut finish = |name: &mut Option<String>, result: &mut Option<String>, driver: &mut Option<String>| {
        if let (Some(name), Some(result), Some(driver)) = (name.take(), result.take(), driver.take()) {
            found.insert(name, RecordedAttestation { result, driver });
        }
    };
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let content = line.trim();
        if content.is_empty() {
            continue;
        }
        if indent == 0 {
            finish(&mut name, &mut result, &mut driver);
            in_block = content == "attestations:";
            continue;
        }
        if !in_block {
            continue;
        }
        let value = |key: &str| {
            content
                .strip_prefix(key)
                .map(|value| value.trim().trim_matches('"').to_owned())
        };
        if indent == 2 && content.ends_with(':') {
            finish(&mut name, &mut result, &mut driver);
            name = Some(content.trim_end_matches(':').to_owned());
        } else if indent == 4 {
            if let Some(value) = value("result:") {
                result = Some(value);
            }
        } else if indent == 6 {
            if let Some(value) = value("driver:") {
                driver = Some(value);
            }
        }
    }
    finish(&mut name, &mut result, &mut driver);
    found
}

/// The token in the body Argo CD answers a session or token request with.
pub fn parse_token(body: &str) -> Result<String> {
    let value = serde_json::from_str::<Value>(body)
        .map_err(|error| Error::new(format!("the Argo CD API answered with invalid JSON ({error}): {body}")))?;
    value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context(format!("the Argo CD API answered without a token: {body}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn deployment(image: &str, host: &str, ready: i64, updated: i64, replicas: i64) -> Value {
        json!({
            "metadata": {"generation": 2},
            "spec": {"replicas": 1, "template": {"spec": {"containers": [{
                "image": image,
                "env": [{"name": "OTHER", "value": "x"}, {"name": "DATABASE_HOST", "value": host}],
            }]}}},
            "status": {
                "observedGeneration": 2,
                "replicas": replicas,
                "updatedReplicas": updated,
                "readyReplicas": ready,
                "availableReplicas": ready,
            },
        })
    }

    fn reason<T: std::fmt::Debug>(poll: Poll<T>) -> String {
        match poll {
            Poll::Pending(reason) => reason,
            Poll::Ready(value) => panic!("expected pending, got ready({value:?})"),
        }
    }

    #[test]
    fn test_a_finished_rollout_of_the_new_revision_is_ready() {
        let ready = deployment("r/web@sha256:b", "dev.r2.db.internal", 1, 1, 1);
        assert_eq!(
            deployment_ready(&ready, "r/web@sha256:b", "dev.r2.db.internal"),
            Poll::Ready(())
        );
    }

    #[test]
    fn test_the_old_revision_still_running_is_not_ready_and_says_which() {
        let old = deployment("r/web@sha256:a", "dev.r1.db.internal", 1, 1, 1);
        assert!(reason(deployment_ready(&old, "r/web@sha256:b", "dev.r2.db.internal"))
            .contains("image r/web@sha256:a instead of r/web@sha256:b"));
        let host = deployment("r/web@sha256:b", "dev.r1.db.internal", 1, 1, 1);
        assert!(reason(deployment_ready(&host, "r/web@sha256:b", "dev.r2.db.internal"))
            .contains("DATABASE_HOST is dev.r1.db.internal"));
    }

    #[test]
    fn test_a_rollout_with_an_old_pod_left_or_a_pod_not_ready_is_pending() {
        let surge = deployment("i", "h", 1, 1, 2);
        assert_eq!(reason(deployment_ready(&surge, "i", "h")), "replicas is 2 of 1");
        let starting = deployment("i", "h", 0, 1, 1);
        assert_eq!(reason(deployment_ready(&starting, "i", "h")), "readyReplicas is 0 of 1");
        let mut stale = deployment("i", "h", 1, 1, 1);
        stale["status"]["observedGeneration"] = json!(1);
        assert!(reason(deployment_ready(&stale, "i", "h")).contains("observed generation 1 of 2"));
    }

    fn attest_report(check: Value, accepted: &str, healthy: &str) -> Value {
        json!({"environments": [{"units": {"kubernetes": {
            "check": check,
            "attestations": {"accepted": accepted, "healthy": healthy},
        }}}]})
    }

    #[test]
    fn test_attestation_needs_both_names_to_pass() {
        assert_eq!(
            attestation_ready(&attest_report(json!("attested"), "pass", "pass"), "kubernetes"),
            Poll::Ready(())
        );
        assert_eq!(
            reason(attestation_ready(
                &attest_report(json!("attested"), "pass", "fail"),
                "kubernetes"
            )),
            "attestation healthy is \"fail\""
        );
        assert!(reason(attestation_ready(
            &attest_report(json!("attested"), "fail", "pass"),
            "kubernetes"
        ))
        .contains("accepted"));
    }

    #[test]
    fn test_an_unknown_check_reports_the_reason_nyl_gave() {
        let report = json!({"environments": [{"units": {"kubernetes": {
            "check": "unknown",
            "reason": "the catalog Application argocd/dev-catalog is not in the reading",
        }}}]});
        let unknown = reason(attestation_ready(&report, "kubernetes"));
        assert!(
            unknown.starts_with("the check is unknown: the catalog Application"),
            "{unknown}"
        );
        assert!(reason(attestation_ready(&json!({}), "kubernetes")).contains("no unit kubernetes"));
    }

    fn observe_report(replicas: Value, ip: Value) -> Value {
        json!({"environments": [{"units": {"kubernetes": {
            "check": "observed",
            "observations": {"readyReplicas": replicas, "serviceClusterIP": ip},
        }}}]})
    }

    #[test]
    fn test_observations_are_ready_when_both_have_a_value_of_the_declared_type() {
        let report = observe_report(
            json!({"observation": "value", "value": 1}),
            json!({"observation": "value", "value": "10.96.12.7"}),
        );
        assert_eq!(
            observations_ready(&report, "kubernetes"),
            Poll::Ready(Observed {
                ready_replicas: 1,
                service_cluster_ip: "10.96.12.7".to_owned()
            })
        );
    }

    #[test]
    fn test_a_pending_observation_names_its_reason() {
        let report = observe_report(
            json!({"observation": "pending", "reason": "application-not-running-publication"}),
            json!({"observation": "value", "value": "10.96.12.7"}),
        );
        assert_eq!(
            reason(observations_ready(&report, "kubernetes")),
            "observation readyReplicas is pending: application-not-running-publication"
        );
    }

    fn application(sync: &str, revision: &str, health: &str) -> Value {
        json!({"status": {"sync": {"status": sync, "revision": revision}, "health": {"status": health}}})
    }

    #[test]
    fn test_an_application_converges_only_when_synced_to_the_head_and_healthy() {
        let head = "0123456789abcdef";
        assert_eq!(
            application_converged(&application("Synced", head, "Healthy"), head),
            Poll::Ready(())
        );
        // Synced to the previous publication is not the new one.
        let stale = reason(application_converged(
            &application("Synced", "fedcba9876543210", "Healthy"),
            head,
        ));
        assert!(
            stale.contains("Synced at fedcba98") && stale.contains("want Synced at 01234567"),
            "{stale}"
        );
        assert!(
            reason(application_converged(&application("Synced", head, "Progressing"), head))
                .contains("health Progressing")
        );
    }

    #[test]
    fn test_a_sync_starts_only_when_none_is_queued_or_running() {
        let head = "abc";
        assert!(needs_sync(&application("OutOfSync", head, "Missing"), head));
        assert!(needs_sync(&application("Synced", "old", "Healthy"), head));
        assert!(!needs_sync(&application("Synced", head, "Progressing"), head));
        let mut queued = application("OutOfSync", head, "Missing");
        queued["operation"] = json!({"sync": {}});
        assert!(!needs_sync(&queued, head));
        let mut running = application("OutOfSync", head, "Missing");
        running["status"]["operationState"] = json!({"phase": "Running"});
        assert!(!needs_sync(&running, head));
        let mut failed = application("OutOfSync", head, "Missing");
        failed["status"]["operationState"] = json!({"phase": "Failed", "message": "project not found"});
        assert!(needs_sync(&failed, head));
        assert!(reason(application_converged(&failed, head)).ends_with("last operation Failed: project not found"));
    }

    #[test]
    fn test_an_attestation_record_is_read_by_name_with_its_result_and_driver() {
        // The layout of the golden file crates/nyl-core/tests/golden/state/attestation-record.v1.yaml.
        let record = [
            "apiVersion: gitops.nyl/v1",
            "kind: AttestationRecord",
            "unit:",
            "  environment: dev",
            "  name: kubernetes",
            "attestations:",
            "  accepted:",
            "    executionKey: sha256:9f2c",
            "    result: pass",
            "    at: \"2026-09-25T16:40:00Z\"",
            "    source:",
            "      driver: reconcile",
            "    reason: null",
            "  healthy:",
            "    executionKey: sha256:9f2c",
            "    result: fail",
            "    source:",
            "      driver: attest",
            "    reason: Degraded",
            "    details:",
            "      applications:",
            "        web:",
            "          health: Degraded",
            "",
        ]
        .join("\n");
        let recorded = parse_attestation_record(&record);
        assert_eq!(recorded.len(), 2, "{recorded:?}");
        assert_eq!(
            recorded["accepted"],
            RecordedAttestation {
                result: "pass".to_owned(),
                driver: "reconcile".to_owned()
            }
        );
        assert_eq!(
            recorded["healthy"],
            RecordedAttestation {
                result: "fail".to_owned(),
                driver: "attest".to_owned()
            }
        );
        assert!(parse_attestation_record("kind: AttestationRecord\n").is_empty());
    }

    #[test]
    fn test_parse_token_requires_a_non_empty_token() {
        assert_eq!(parse_token(r#"{"token":"abc.def"}"#).unwrap(), "abc.def");
        assert!(parse_token(r#"{"token":""}"#).is_err());
        let error = parse_token(r#"{"error":"invalid session"}"#).unwrap_err().to_string();
        assert!(
            error.contains("without a token") && error.contains("invalid session"),
            "{error}"
        );
        assert!(parse_token("<html>").is_err());
    }
}
