//! The health of the workloads a release applied, from their live status.
//!
//! Contract: [Reading a direct target](../../../../../design/promotion.md#reading-a-direct-target).
//! Rollout status has the semantics of `kubectl rollout status` for the kinds
//! that roll out: Deployments, StatefulSets, DaemonSets, Jobs, and ReplicaSets
//! no controller owns. Every other kind is not read, as an Argo CD resource
//! without a health check counts as healthy once synced. The objects come from
//! the manifest of the revision (never from keys that stay tracked only
//! because their prune awaits confirmation), and an object that does not exist
//! is a finding, except a Job whose `ttlSecondsAfterFinished` makes its
//! disappearance after completion expected.

use nyl_render::kubernetes::ResourceKey;
use serde_json::Value;

use crate::kubernetes_publication::reconciler::Health;

/// Whether a kind rolls out and is read.
pub fn is_workload(api_version: &str, kind: &str) -> bool {
    matches!(
        (api_version, kind),
        ("apps/v1", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch/v1", "Job")
    )
}

/// What one workload's status says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub health: Health,
    /// Why, when it is not healthy.
    pub why: Option<String>,
    /// Degraded for good: it will not recover without a change to what runs.
    pub terminal: bool,
}

impl Status {
    fn healthy() -> Self {
        Self {
            health: Health::Healthy,
            why: None,
            terminal: false,
        }
    }

    fn of(health: Health, why: impl Into<String>) -> Self {
        Self {
            health,
            why: Some(why.into()),
            terminal: false,
        }
    }

    fn terminal(why: impl Into<String>) -> Self {
        Self {
            terminal: true,
            ..Self::of(Health::Degraded, why)
        }
    }
}

fn count(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(Value::as_i64).unwrap_or(0)
}

fn declared_replicas(live: &Value) -> i64 {
    live.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1)
}

fn observed(live: &Value) -> bool {
    count(live, "/status/observedGeneration") >= count(live, "/metadata/generation")
}

fn condition<'a>(live: &'a Value, kind: &str) -> Option<&'a Value> {
    live.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|condition| condition.get("type").and_then(Value::as_str) == Some(kind))
}

fn condition_is(live: &Value, kind: &str, status: &str) -> bool {
    condition(live, kind)
        .and_then(|condition| condition.get("status"))
        .and_then(Value::as_str)
        == Some(status)
}

/// The rollout status of one live workload of `kind`; `None` for a kind that is
/// not read.
pub fn rollout(kind: &str, live: &Value) -> Option<Status> {
    Some(match kind {
        "Deployment" => deployment(live),
        "StatefulSet" => stateful_set(live),
        "DaemonSet" => daemon_set(live),
        "ReplicaSet" => replica_set(live),
        "Job" => job(live),
        _ => return None,
    })
}

fn deployment(live: &Value) -> Status {
    if live.pointer("/spec/paused").and_then(Value::as_bool) == Some(true) {
        return Status::of(Health::Unknown, "it is paused");
    }
    if !observed(live) {
        return Status::of(
            Health::Progressing,
            "the controller has not observed its latest generation",
        );
    }
    if let Some(progressing) = condition(live, "Progressing") {
        if progressing.get("status").and_then(Value::as_str) == Some("False")
            && progressing.get("reason").and_then(Value::as_str) == Some("ProgressDeadlineExceeded")
        {
            return Status::terminal("its rollout exceeded its progress deadline");
        }
    }
    let replicas = declared_replicas(live);
    let updated = count(live, "/status/updatedReplicas");
    let total = count(live, "/status/replicas");
    let available = count(live, "/status/availableReplicas");
    if updated < replicas {
        Status::of(
            Health::Progressing,
            format!("{updated} of {replicas} replicas are updated"),
        )
    } else if total > updated {
        Status::of(
            Health::Progressing,
            format!("{} old replicas are pending termination", total - updated),
        )
    } else if available < updated {
        Status::of(
            Health::Progressing,
            format!("{available} of {updated} updated replicas are available"),
        )
    } else {
        Status::healthy()
    }
}

fn stateful_set(live: &Value) -> Status {
    if !observed(live) {
        return Status::of(
            Health::Progressing,
            "the controller has not observed its latest generation",
        );
    }
    let replicas = declared_replicas(live);
    let ready = count(live, "/status/readyReplicas");
    if ready < replicas {
        return Status::of(Health::Progressing, format!("{ready} of {replicas} replicas are ready"));
    }
    if live.pointer("/spec/updateStrategy/type").and_then(Value::as_str) == Some("OnDelete") {
        return Status::healthy();
    }
    let updated = count(live, "/status/updatedReplicas");
    if updated < replicas {
        return Status::of(
            Health::Progressing,
            format!("{updated} of {replicas} replicas are updated"),
        );
    }
    let current = live.pointer("/status/currentRevision").and_then(Value::as_str);
    let update = live.pointer("/status/updateRevision").and_then(Value::as_str);
    if current != update {
        return Status::of(Health::Progressing, "its update revision is not current yet");
    }
    Status::healthy()
}

fn daemon_set(live: &Value) -> Status {
    if !observed(live) {
        return Status::of(
            Health::Progressing,
            "the controller has not observed its latest generation",
        );
    }
    let desired = count(live, "/status/desiredNumberScheduled");
    let updated = count(live, "/status/updatedNumberScheduled");
    let available = count(live, "/status/numberAvailable");
    if updated < desired {
        Status::of(Health::Progressing, format!("{updated} of {desired} pods are updated"))
    } else if available < desired {
        Status::of(
            Health::Progressing,
            format!("{available} of {desired} pods are available"),
        )
    } else {
        Status::healthy()
    }
}

fn replica_set(live: &Value) -> Status {
    let replicas = declared_replicas(live);
    let ready = count(live, "/status/readyReplicas");
    if ready < replicas {
        Status::of(Health::Progressing, format!("{ready} of {replicas} replicas are ready"))
    } else {
        Status::healthy()
    }
}

fn job(live: &Value) -> Status {
    if condition_is(live, "Failed", "True") {
        Status::terminal("the Job failed")
    } else if condition_is(live, "Complete", "True") {
        Status::healthy()
    } else {
        Status::of(Health::Progressing, "the Job is running")
    }
}

/// Whether a manifest's Job sets `ttlSecondsAfterFinished`, so its
/// disappearance after completion is expected.
pub fn job_expires(manifest: &Value) -> bool {
    manifest.pointer("/spec/ttlSecondsAfterFinished").is_some()
}

/// Whether the object `key` names, whose manifest is `manifest`, is a Job that
/// sets `ttlSecondsAfterFinished`: a Job whose absence after it ran is expected,
/// and that applying again would run a second time.
pub fn is_ttl_job(key: &ResourceKey, manifest: &Value) -> bool {
    key.gvk.group == "batch" && key.gvk.kind == "Job" && job_expires(manifest)
}

/// One workload of the revision's manifest and what was found of it.
pub struct Found<'a> {
    /// `Kind namespace/name`, for the reason.
    pub label: String,
    pub kind: &'a str,
    /// The manifest document.
    pub manifest: &'a Value,
    /// The live object, `None` when the cluster has none.
    pub live: Option<Value>,
}

/// Fold what was found into one health and the reason that names the worst
/// object, `Degraded` before `Progressing` before `Unknown` before `Healthy`.
pub fn fold(found: &[Found<'_>]) -> (Health, Option<String>, bool) {
    let mut worst: Option<(Health, String, bool)> = None;
    let rank = |health: Health| match health {
        Health::Degraded => 3,
        Health::Progressing => 2,
        Health::Unknown => 1,
        Health::Healthy => 0,
    };
    for item in found {
        let status = match &item.live {
            Some(live) => match rollout(item.kind, live) {
                Some(status) => status,
                None => continue,
            },
            None if item.kind == "Job" && job_expires(item.manifest) => continue,
            None => Status::of(Health::Degraded, "it does not exist"),
        };
        if status.health == Health::Healthy {
            continue;
        }
        let reason = format!("{}: {}", item.label, status.why.unwrap_or_default());
        if worst
            .as_ref()
            .is_none_or(|(health, _, _)| rank(status.health) > rank(*health))
        {
            worst = Some((status.health, reason, status.terminal));
        }
    }
    match worst {
        Some((health, reason, terminal)) => (health, Some(reason), terminal),
        None => (Health::Healthy, None, false),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn deployment_status(
        generation: i64,
        observed: i64,
        replicas: i64,
        updated: i64,
        total: i64,
        available: i64,
    ) -> Value {
        json!({
            "metadata": {"generation": generation},
            "spec": {"replicas": replicas},
            "status": {"observedGeneration": observed, "updatedReplicas": updated, "replicas": total, "availableReplicas": available}
        })
    }

    /// Catches a Deployment that is still rolling out counting as healthy, in
    /// each way a rollout is unfinished.
    #[test]
    fn test_a_deployment_is_healthy_only_when_the_rollout_finished() {
        assert_eq!(
            rollout("Deployment", &deployment_status(2, 2, 3, 3, 3, 3))
                .unwrap()
                .health,
            Health::Healthy
        );
        for unfinished in [
            deployment_status(2, 1, 3, 3, 3, 3),
            deployment_status(2, 2, 3, 2, 3, 3),
            deployment_status(2, 2, 3, 3, 4, 3),
            deployment_status(2, 2, 3, 3, 3, 2),
        ] {
            assert_eq!(
                rollout("Deployment", &unfinished).unwrap().health,
                Health::Progressing,
                "{unfinished}"
            );
        }
    }

    /// Catches a stuck rollout being reported as progressing forever, and a
    /// paused one as healthy.
    #[test]
    fn test_a_deployment_past_its_deadline_is_degraded_and_a_paused_one_unknown() {
        let mut stuck = deployment_status(1, 1, 1, 0, 1, 0);
        stuck["status"]["conditions"] =
            json!([{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded"}]);
        assert_eq!(rollout("Deployment", &stuck).unwrap().health, Health::Degraded);
        let mut paused = deployment_status(1, 1, 1, 1, 1, 1);
        paused["spec"]["paused"] = json!(true);
        assert_eq!(rollout("Deployment", &paused).unwrap().health, Health::Unknown);
    }

    /// Catches a StatefulSet counted healthy before its update revision is
    /// current, and `OnDelete` waiting for an update that never rolls out.
    #[test]
    fn test_a_stateful_set_follows_its_update_strategy() {
        let rolling = json!({"metadata": {"generation": 1}, "spec": {"replicas": 2},
            "status": {"observedGeneration": 1, "readyReplicas": 2, "updatedReplicas": 2, "currentRevision": "a", "updateRevision": "b"}});
        assert_eq!(rollout("StatefulSet", &rolling).unwrap().health, Health::Progressing);
        let mut on_delete = rolling.clone();
        on_delete["spec"]["updateStrategy"] = json!({"type": "OnDelete"});
        assert_eq!(rollout("StatefulSet", &on_delete).unwrap().health, Health::Healthy);
    }

    /// Catches a Job's outcome being read from anything but its conditions.
    #[test]
    fn test_a_job_is_healthy_when_complete_and_degraded_when_failed() {
        let job = |kind: &str| json!({"status": {"conditions": [{"type": kind, "status": "True"}]}});
        assert_eq!(rollout("Job", &job("Complete")).unwrap().health, Health::Healthy);
        assert_eq!(rollout("Job", &job("Failed")).unwrap().health, Health::Degraded);
        assert_eq!(rollout("Job", &json!({})).unwrap().health, Health::Progressing);
    }

    /// Catches kinds without a rollout being judged, which would make every
    /// ConfigMap a finding.
    #[test]
    fn test_other_kinds_are_not_read() {
        assert!(!is_workload("v1", "ConfigMap"));
        assert!(is_workload("apps/v1", "Deployment"));
        assert!(rollout("ConfigMap", &json!({})).is_none());
    }

    /// Catches the fold hiding the worst object, a deleted workload being
    /// healthy, and a Job that expired by its TTL being a finding.
    #[test]
    fn test_fold_names_the_worst_object_and_ignores_expired_jobs() {
        let manifest = json!({});
        let ttl = json!({"spec": {"ttlSecondsAfterFinished": 60}});
        let found = vec![
            Found {
                label: "Deployment a/web".to_owned(),
                kind: "Deployment",
                manifest: &manifest,
                live: Some(deployment_status(1, 1, 2, 1, 2, 1)),
            },
            Found {
                label: "Job a/migrate".to_owned(),
                kind: "Job",
                manifest: &ttl,
                live: None,
            },
            Found {
                label: "StatefulSet a/db".to_owned(),
                kind: "StatefulSet",
                manifest: &manifest,
                live: None,
            },
        ];
        let (health, reason, terminal) = fold(&found);
        assert!(!terminal, "a missing workload may still appear");
        assert_eq!(health, Health::Degraded);
        assert!(reason.unwrap().starts_with("StatefulSet a/db: it does not exist"));
        assert_eq!(fold(&found[1..2]).0, Health::Healthy);
        assert_eq!(fold(&found[..1]).0, Health::Progressing);
    }
}
