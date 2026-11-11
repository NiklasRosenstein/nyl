//! The health of the workloads a release applied, from their live status.
//!
//! Contract: [Reading a direct target](../../../../design/promotion.md#reading-a-direct-target)
//! and [Direct reconciler](../../../../design/orchestration-core.md#direct-reconciler),
//! applying. Rollout status has the semantics of `kubectl rollout status` for the
//! kinds that roll out: Deployments, StatefulSets, DaemonSets, Jobs, and ReplicaSets
//! no controller owns, a Pod follows its phase, and an Argo Workflow its
//! `status.phase`. Every other kind is not read, as an Argo CD resource without a
//! health check counts as healthy once synced; Argo CD also reads Ingresses,
//! LoadBalancer Services, PersistentVolumeClaims, APIServices, and
//! HorizontalPodAutoscalers, and runs custom Lua checks, which Nyl does not.
//!
//! Two callers read it: the apply's waits between Argo CD sync waves and phases
//! ([`super::hooks`]), and the direct reconciler's reader, which takes the objects
//! from the manifest of the revision (never from keys that stay tracked only
//! because their prune awaits confirmation, and never a hook). An object that does
//! not exist is a finding, except a Job whose `ttlSecondsAfterFinished` makes its
//! disappearance after completion expected.

use serde_json::Value;

use super::ResourceKey;

/// How healthy a workload is. The direct reader maps it onto the reconciler-neutral
/// health it reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Progressing,
    /// Held on purpose, as a paused Deployment is: a sync waits for it, as for
    /// `Progressing`, but no rollout is under way.
    Suspended,
    Degraded,
    /// Anything else, and nothing reported: never a pass.
    Unknown,
}

/// Whether a kind rolls out and is read.
pub fn is_workload(api_version: &str, kind: &str) -> bool {
    matches!(
        (api_version, kind),
        ("apps/v1", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch/v1", "Job")
    )
}

/// Whether an apply reads the health of this kind ([`rollout`]): the workloads, Pods,
/// and Argo Workflows. Any other kind counts as healthy once it exists.
pub fn assessed(group: &str, kind: &str) -> bool {
    matches!(
        (group, kind),
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet")
            | ("batch", "Job")
            | ("", "Pod")
            | ("argoproj.io", "Workflow")
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
        "Pod" => pod(live),
        "Workflow" => workflow(live),
        _ => return None,
    })
}

fn deployment(live: &Value) -> Status {
    if live.pointer("/spec/paused").and_then(Value::as_bool) == Some(true) {
        return Status::of(Health::Suspended, "it is paused");
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

/// A Pod's phase, as Argo CD's Pod health reads it: a Pod that runs to completion
/// is healthy once it succeeded and degraded once it failed, and one that keeps
/// running (`restartPolicy: Always`) is healthy once it is ready.
fn pod(live: &Value) -> Status {
    let phase = live
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let keeps_running = live.pointer("/spec/restartPolicy").and_then(Value::as_str) == Some("Always");
    match phase {
        "Succeeded" => Status::healthy(),
        "Failed" => Status::terminal("the Pod failed"),
        "Running" if keeps_running && condition_is(live, "Ready", "True") => Status::healthy(),
        "Running" => Status::of(Health::Progressing, "the Pod is running"),
        _ => Status::of(Health::Progressing, "the Pod is pending"),
    }
}

/// An Argo Workflow's `status.phase`, as Argo CD's health check for it reads it:
/// `Succeeded` is healthy, `Failed` and `Error` are degraded, and anything else, a
/// Workflow no controller has picked up included, is still progressing.
fn workflow(live: &Value) -> Status {
    let phase = live
        .pointer("/status/phase")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match phase {
        "Succeeded" => Status::healthy(),
        "Failed" | "Error" => {
            let message = live
                .pointer("/status/message")
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty());
            Status::terminal(match message {
                Some(message) => format!("the Workflow {}: {message}", phase.to_lowercase()),
                None => format!("the Workflow {}", phase.to_lowercase()),
            })
        }
        "" => Status::of(Health::Progressing, "the Workflow has not started"),
        other => Status::of(Health::Progressing, format!("the Workflow is {}", other.to_lowercase())),
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
        Health::Progressing | Health::Suspended => 2,
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
    /// paused one as healthy or as unknown, which a sync would fail on instead of
    /// waiting for it.
    #[test]
    fn test_a_deployment_past_its_deadline_is_degraded_and_a_paused_one_suspended() {
        let mut stuck = deployment_status(1, 1, 1, 0, 1, 0);
        stuck["status"]["conditions"] =
            json!([{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded"}]);
        assert_eq!(rollout("Deployment", &stuck).unwrap().health, Health::Degraded);
        let mut paused = deployment_status(1, 1, 1, 1, 1, 1);
        paused["spec"]["paused"] = json!(true);
        assert_eq!(rollout("Deployment", &paused).unwrap().health, Health::Suspended);
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

    /// Catches a hook Pod that keeps the sync waiting after it succeeded, or passes
    /// while it still runs: a Pod that runs to completion follows its phase, and
    /// one that keeps running is healthy once it is ready.
    #[test]
    fn test_a_pod_follows_its_phase_and_readiness() {
        let pod = |phase: &str, restart: &str, ready: bool| {
            json!({"spec": {"restartPolicy": restart}, "status": {"phase": phase,
                "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
        };
        let health = |live: Value| rollout("Pod", &live).unwrap().health;

        assert_eq!(health(pod("Succeeded", "Never", false)), Health::Healthy);
        assert_eq!(health(pod("Running", "Never", true)), Health::Progressing);
        assert_eq!(health(pod("Pending", "OnFailure", false)), Health::Progressing);
        assert_eq!(health(pod("Failed", "Never", false)), Health::Degraded);
        assert_eq!(health(pod("Running", "Always", true)), Health::Healthy);
        assert_eq!(health(pod("Running", "Always", false)), Health::Progressing);
        assert!(assessed("", "Pod") && assessed("batch", "Job") && !assessed("example.com", "Job"));
    }

    /// Catches a hook Workflow being passed while it runs, or failing the sync
    /// before it ended: Argo CD reads `status.phase`.
    #[test]
    fn test_a_workflow_follows_its_phase() {
        let workflow = |phase: Option<&str>| match phase {
            Some(phase) => json!({"status": {"phase": phase, "message": "boom"}}),
            None => json!({}),
        };
        let health = |phase| rollout("Workflow", &workflow(phase)).unwrap();

        assert_eq!(health(Some("Succeeded")).health, Health::Healthy);
        assert_eq!(health(Some("Running")).health, Health::Progressing);
        assert_eq!(health(Some("Pending")).health, Health::Progressing);
        assert_eq!(health(None).health, Health::Progressing);
        for phase in ["Failed", "Error"] {
            let status = health(Some(phase));
            assert_eq!(status.health, Health::Degraded, "{phase}");
            assert!(status.terminal && status.why.unwrap().contains("boom"), "{phase}");
        }
        assert!(assessed("argoproj.io", "Workflow") && !assessed("example.com", "Workflow"));
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
