//! Tests of how a sync runs hooks and waves ([`super::sync_run`]), over a cluster
//! that gives Jobs and Deployments the status a real one would. The release
//! lifecycle's tests use the same cluster.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use kube::api::DynamicObject;
use serde_json::{json, Value};

use super::field_manager::FieldOwnership;
use super::hooks::{HookPhase, HELM_HOOK, HELM_HOOK_WEIGHT, HOOK, HOOK_DELETE_POLICY, SYNC_WAVE};
use super::lifecycle::{apply_sorted_manifests, ApplyEvent, ApplyExecutionResult};
use super::{
    ApplyOutcome, DeleteOptions, DeletePropagation, GroupVersionKind, KubeClient, MockKubeClient, ResourceKey,
    WaitSettings,
};
use crate::Result;

/// What a Job reaches once it is applied.
#[derive(Clone, Copy)]
pub(super) enum Outcome {
    Completes,
    /// Runs for this many reads, then completes.
    CompletesAfter(usize),
    Fails,
    /// Never finishes.
    Runs,
}

/// A [`MockKubeClient`] that gives Jobs and Deployments the status a cluster would,
/// and logs every apply and delete, and when a Job or Deployment is first seen done.
pub(super) struct HookCluster {
    pub(super) inner: MockKubeClient,
    log: Mutex<Vec<String>>,
    outcomes: Mutex<HashMap<String, Outcome>>,
    /// Deployments that are not healthy for this many reads (`usize::MAX`: never).
    unready: Mutex<HashMap<String, usize>>,
    /// Deleted objects stay visible for this many more reads.
    lingering: Mutex<HashMap<String, usize>>,
    /// Objects that are being deleted: they stay visible while they linger.
    deleting: Mutex<HashMap<String, ResourceKey>>,
    announced: Mutex<Vec<String>>,
    /// Names applied through this client.
    applied: Mutex<Vec<String>>,
}

impl HookCluster {
    pub(super) fn new() -> Self {
        Self {
            inner: MockKubeClient::new(),
            log: Mutex::default(),
            outcomes: Mutex::default(),
            unready: Mutex::default(),
            lingering: Mutex::default(),
            deleting: Mutex::default(),
            announced: Mutex::default(),
            applied: Mutex::default(),
        }
    }

    pub(super) fn outcome(&self, name: &str, outcome: Outcome) {
        self.outcomes.lock().unwrap().insert(name.to_owned(), outcome);
    }

    pub(super) fn unready(&self, name: &str, reads: usize) {
        self.unready.lock().unwrap().insert(name.to_owned(), reads);
    }

    /// A deleted object `name` stays visible for `reads` reads.
    pub(super) fn linger(&self, name: &str, reads: usize) {
        self.lingering.lock().unwrap().insert(name.to_owned(), reads);
    }

    /// Put an object in the cluster as if an earlier sync created it.
    pub(super) fn seed(&self, manifest: &Value) {
        let key = ResourceKey::from_json_value(manifest).unwrap();
        self.inner
            .add_resource(key, serde_json::from_value(manifest.clone()).unwrap());
    }

    pub(super) fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The applies, in order.
    pub(super) fn applies(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|entry| entry.starts_with("apply"))
            .collect()
    }

    pub(super) fn position(&self, entry: &str) -> usize {
        self.log()
            .iter()
            .position(|logged| logged == entry)
            .unwrap_or_else(|| panic!("{entry:?} is not in {:?}", self.log()))
    }

    pub(super) fn exists(&self, name: &str) -> bool {
        self.inner.get_all_resources().keys().any(|key| key.name == name)
    }

    fn record(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    /// Log, once, that `kind name` was seen done.
    fn announce(&self, what: &str, kind: &str, name: &str) {
        // What an earlier sync left behind is not news.
        if !self.applied.lock().unwrap().iter().any(|applied| applied == name) {
            return;
        }
        let entry = format!("{what} {kind} {name}");
        let mut announced = self.announced.lock().unwrap();
        if !announced.contains(&entry) {
            announced.push(entry.clone());
            drop(announced);
            self.record(entry);
        }
    }

    fn with_status(&self, mut object: DynamicObject) -> DynamicObject {
        let kind = object
            .types
            .as_ref()
            .map(|types| types.kind.clone())
            .unwrap_or_default();
        let name = object.metadata.name.clone().unwrap_or_default();
        match kind.as_str() {
            "Job" => {
                let outcome = self
                    .outcomes
                    .lock()
                    .unwrap()
                    .get(&name)
                    .copied()
                    .unwrap_or(Outcome::Completes);
                let condition = |kind: &str| json!({"conditions": [{"type": kind, "status": "True"}]});
                match outcome {
                    Outcome::Completes | Outcome::CompletesAfter(0) => {
                        self.announce("complete", "Job", &name);
                        object.data["status"] = condition("Complete");
                    }
                    Outcome::CompletesAfter(reads) => {
                        self.outcomes
                            .lock()
                            .unwrap()
                            .insert(name.clone(), Outcome::CompletesAfter(reads - 1));
                    }
                    Outcome::Fails => object.data["status"] = condition("Failed"),
                    Outcome::Runs => {}
                }
            }
            "Deployment" => {
                let mut unready = self.unready.lock().unwrap();
                let waiting = unready.get_mut(&name).filter(|reads| **reads > 0);
                if let Some(reads) = waiting {
                    *reads = reads.saturating_sub(1);
                    object.data["status"] = json!({"replicas": 1, "updatedReplicas": 0, "availableReplicas": 0});
                } else {
                    drop(unready);
                    self.announce("healthy", "Deployment", &name);
                    object.data["status"] = json!({"replicas": 1, "updatedReplicas": 1, "availableReplicas": 1});
                }
            }
            _ => {}
        }
        object
    }
}

#[async_trait]
impl KubeClient for HookCluster {
    async fn get_resource(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<DynamicObject>> {
        let deleting = self.deleting.lock().unwrap().get(name).cloned();
        if let Some(key) = deleting {
            let gone = {
                let mut lingering = self.lingering.lock().unwrap();
                let reads = lingering.entry(name.to_owned()).or_default();
                if *reads == 0 {
                    true
                } else {
                    *reads -= 1;
                    false
                }
            };
            if gone {
                self.deleting.lock().unwrap().remove(name);
                self.inner
                    .delete_resource_with(&key.gvk, key.namespace.as_deref(), &key.name, &DeleteOptions::default())
                    .await?;
                self.record(format!("gone {} {name}", gvk.kind));
            }
        }
        Ok(self
            .inner
            .get_resource(gvk, namespace, name)
            .await?
            .map(|object| self.with_status(object)))
    }

    async fn apply_resource(&self, resource: &DynamicObject, manager: &str, dry_run: bool) -> Result<ApplyOutcome> {
        use kube::ResourceExt;
        let kind = resource
            .types
            .as_ref()
            .map(|types| types.kind.clone())
            .unwrap_or_default();
        self.record(format!("apply {kind} {}", resource.name_any()));
        self.applied.lock().unwrap().push(resource.name_any());
        self.inner.apply_resource(resource, manager, dry_run).await
    }

    async fn get_server_version(&self) -> Result<String> {
        self.inner.get_server_version().await
    }

    async fn get_api_versions(&self) -> Result<Vec<String>> {
        self.inner.get_api_versions().await
    }

    async fn is_namespaced(&self, gvk: &GroupVersionKind) -> Result<bool> {
        self.inner.is_namespaced(gvk).await
    }

    fn default_namespace(&self) -> &str {
        self.inner.default_namespace()
    }

    async fn delete_resource_with(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
        options: &DeleteOptions,
    ) -> Result<()> {
        self.record(format!("delete {} {name}", gvk.kind));
        if self.lingering.lock().unwrap().get(name).is_some_and(|reads| *reads > 0) {
            // The object stays until the reads that let it linger are used up.
            self.inner.delete_resource_with(gvk, namespace, "\0", options).await?;
            self.deleting.lock().unwrap().insert(
                name.to_owned(),
                ResourceKey {
                    gvk: gvk.clone(),
                    namespace: namespace.map(str::to_owned),
                    name: name.to_owned(),
                },
            );
            return Ok(());
        }
        self.inner.delete_resource_with(gvk, namespace, name, options).await
    }

    async fn get_normalized_resource(&self, resource: &DynamicObject, manager: &str) -> Result<DynamicObject> {
        self.inner.get_normalized_resource(resource, manager).await
    }

    async fn release_field_manager(&self, resource: &DynamicObject, manager: &str) -> Result<bool> {
        self.inner.release_field_manager(resource, manager).await
    }
}

pub(super) fn job(name: &str, annotations: Value) -> Value {
    json!({
        "apiVersion": "batch/v1", "kind": "Job",
        "metadata": {"name": name, "namespace": "app", "annotations": annotations},
    })
}

pub(super) fn deployment(name: &str, annotations: Value) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name, "namespace": "app", "annotations": annotations},
    })
}

pub(super) fn config_map(name: &str, annotations: Value) -> Value {
    json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": name, "namespace": "app", "annotations": annotations},
    })
}

/// Waits that fail within a simulated half minute.
pub(super) fn waits() -> WaitSettings {
    WaitSettings {
        timeout: Some(Duration::from_secs(30)),
        poll_interval: Duration::from_secs(1),
    }
}

async fn sync(cluster: &HookCluster, manifests: &[Value]) -> ApplyExecutionResult {
    apply_sorted_manifests(cluster, manifests, &FieldOwnership::Nyl, 8, waits(), &mut |_| {})
        .await
        .unwrap()
}

/// Catches a PreSync hook that does not finish before the workload starts, and
/// a PostSync hook that starts before the workload is healthy: the order Argo
/// CD gives a sync, and the one that makes a migration or a smoke test useful.
#[tokio::test(start_paused = true)]
async fn test_presync_completes_before_the_workload_and_postsync_waits_for_its_health() {
    let cluster = HookCluster::new();
    cluster.outcome("migrate", Outcome::CompletesAfter(2));
    cluster.unready("web", 2);
    let manifests = vec![
        job("smoke-test", json!({HOOK: "PostSync"})),
        deployment("web", json!({})),
        job("migrate", json!({HOOK: "PreSync"})),
        job("cleanup", json!({HOOK: "SyncFail"})),
    ];

    let result = sync(&cluster, &manifests).await;

    assert!(!result.sync_failed(), "{:?}", result.hooks);
    assert!(cluster.position("complete Job migrate") < cluster.position("apply Deployment web"));
    assert!(cluster.position("healthy Deployment web") < cluster.position("apply Job smoke-test"));
    assert_eq!(
        cluster.applies(),
        ["apply Job migrate", "apply Deployment web", "apply Job smoke-test"],
        "a SyncFail hook runs only after a failure"
    );
    let tracked: Vec<&str> = result.resource_keys.iter().map(|key| key.name.as_str()).collect();
    assert_eq!(tracked, ["web"], "hooks are not tracked");
    let ran: Vec<(HookPhase, &str)> = result
        .hooks
        .iter()
        .map(|hook| (hook.phase, hook.key.name.as_str()))
        .collect();
    assert_eq!(
        ran,
        [(HookPhase::PreSync, "migrate"), (HookPhase::PostSync, "smoke-test")]
    );
}

/// Catches an existing hook object being applied over (and, for a finished Job,
/// never running again) instead of being deleted and waited for first.
#[tokio::test(start_paused = true)]
async fn test_before_hook_creation_deletes_the_old_object_and_waits_for_it_to_be_gone() {
    let cluster = HookCluster::new();
    let migrate = job("migrate", json!({HOOK: "PreSync"}));
    cluster.seed(&migrate);
    cluster.linger("migrate", 2);

    let result = sync(&cluster, &[migrate]).await;

    assert!(!result.sync_failed());
    assert_eq!(
        cluster.log(),
        [
            "delete Job migrate",
            "gone Job migrate",
            "apply Job migrate",
            "complete Job migrate"
        ]
    );
    assert_eq!(
        cluster.inner.deletion_options()[0].propagation,
        Some(DeletePropagation::Foreground),
        "Argo CD deletes hooks in the foreground"
    );
}

/// Catches a hook with another policy being deleted before its creation.
#[tokio::test(start_paused = true)]
async fn test_a_hook_without_before_hook_creation_is_applied_over_the_old_object() {
    let cluster = HookCluster::new();
    let migrate = job("migrate", json!({HOOK: "PreSync", HOOK_DELETE_POLICY: "HookFailed"}));
    cluster.seed(&migrate);

    let result = sync(&cluster, &[migrate]).await;

    assert!(!result.sync_failed());
    assert_eq!(
        cluster.log()[0],
        "apply Job migrate",
        "no delete first: {:?}",
        cluster.log()
    );
}

/// Catches `HookSucceeded` hooks piling up in the cluster after a good sync, and
/// a hook being deleted before the sync that ran it ended (Argo CD deletes them
/// at the end of the sync).
#[tokio::test(start_paused = true)]
async fn test_hook_succeeded_deletes_the_hook_after_the_sync_succeeded() {
    let cluster = HookCluster::new();
    let manifests = vec![
        job("migrate", json!({HOOK: "PreSync", HOOK_DELETE_POLICY: "HookSucceeded"})),
        deployment("web", json!({})),
        job(
            "smoke-test",
            json!({HOOK: "PostSync", HOOK_DELETE_POLICY: "HookFailed"}),
        ),
    ];

    let result = sync(&cluster, &manifests).await;

    assert!(!result.sync_failed());
    assert!(cluster.position("delete Job migrate") > cluster.position("apply Job smoke-test"));
    assert!(!cluster.exists("migrate"));
    assert!(cluster.exists("smoke-test"), "HookFailed keeps what succeeded");
}

/// Catches a failed hook not failing the apply, the workload being applied after
/// a failed PreSync hook, SyncFail hooks not running, and the HookFailed policy
/// not following a failure.
#[tokio::test(start_paused = true)]
async fn test_a_failed_presync_hook_fails_the_apply_and_runs_syncfail_hooks() {
    let cluster = HookCluster::new();
    cluster.outcome("migrate", Outcome::Fails);
    let manifests = vec![
        job("migrate", json!({HOOK: "PreSync", HOOK_DELETE_POLICY: "HookFailed"})),
        deployment("web", json!({})),
        job("notify", json!({HOOK: "SyncFail"})),
        job("smoke-test", json!({HOOK: "PostSync"})),
    ];

    let result = sync(&cluster, &manifests).await;

    assert!(result.sync_failed());
    assert_eq!(result.hook_failed_count, 1);
    assert!(result.resource_keys.is_empty(), "the workload was never applied");
    assert_eq!(cluster.applies(), ["apply Job migrate", "apply Job notify"]);
    assert!(!cluster.exists("migrate"), "HookFailed deletes the failed hook");
    let error = result.error("Apply").unwrap().to_string();
    assert!(error.contains("PreSync Job app/migrate: the Job failed"), "{error}");
}

/// Catches a hook that hangs the apply for good: the wait ends, and says how to
/// raise it.
#[tokio::test(start_paused = true)]
async fn test_a_hook_that_never_finishes_fails_when_the_wait_runs_out() {
    let cluster = HookCluster::new();
    cluster.outcome("migrate", Outcome::Runs);

    let result = sync(&cluster, &[job("migrate", json!({HOOK: "PreSync"}))]).await;

    assert_eq!(result.hook_failed_count, 1);
    let why = result.hooks[0].failure.clone().unwrap();
    assert!(
        why.contains("the Job is running") && why.contains("--hook-timeout"),
        "{why}"
    );
}

/// Catches a workload that stays unhealthy letting the next wave, or PostSync,
/// start anyway, and SyncFail hooks not following.
#[tokio::test(start_paused = true)]
async fn test_an_unhealthy_wave_stops_the_sync_before_later_waves_and_postsync() {
    let cluster = HookCluster::new();
    cluster.unready("web", usize::MAX);
    let manifests = vec![
        deployment("web", json!({})),
        config_map("late", json!({SYNC_WAVE: "1"})),
        job("smoke-test", json!({HOOK: "PostSync"})),
        job("notify", json!({HOOK: "SyncFail"})),
    ];

    let result = sync(&cluster, &manifests).await;

    assert!(result.sync_failed());
    assert_eq!(result.unhealthy.len(), 1);
    assert!(result.unhealthy[0].1.contains("updated"), "{:?}", result.unhealthy);
    assert!(!cluster.exists("late") && !cluster.exists("smoke-test"));
    assert!(cluster.exists("notify"), "SyncFail runs after any failure");
}

/// Catches sync waves being ignored for ordinary resources, which Argo CD
/// orders by wave, and a wave starting before the workloads of the one before
/// are healthy. The Helm weight stands in for a missing sync wave.
#[tokio::test(start_paused = true)]
async fn test_sync_waves_apply_in_order_and_wait_for_the_workloads_of_the_wave_before() {
    let cluster = HookCluster::new();
    cluster.unready("web", 3);
    let manifests = vec![
        config_map("last", json!({HELM_HOOK_WEIGHT: "5"})),
        deployment("web", json!({})),
        config_map("first", json!({SYNC_WAVE: "-1"})),
    ];

    let result = sync(&cluster, &manifests).await;

    assert!(!result.sync_failed());
    assert_eq!(
        cluster.applies(),
        ["apply ConfigMap first", "apply Deployment web", "apply ConfigMap last"]
    );
    assert!(cluster.position("healthy Deployment web") < cluster.position("apply ConfigMap last"));
}

/// Catches a Release that declares nothing about waves or hooks waiting for
/// workload health: it is one step, applied as it always was, so no apply of an
/// ordinary Release is held up by a rollout.
#[tokio::test(start_paused = true)]
async fn test_a_release_without_hooks_or_waves_does_not_wait_for_health() {
    let cluster = HookCluster::new();
    cluster.unready("web", usize::MAX);

    let result = sync(&cluster, &[deployment("web", json!({})), config_map("cfg", json!({}))]).await;

    assert!(!result.sync_failed());
    assert_eq!(result.resource_keys.len(), 2);
}

/// Catches Helm hooks being applied as ordinary resources, Skip applying, and
/// hooks with no sync phase being applied or going unreported.
#[tokio::test(start_paused = true)]
async fn test_helm_hooks_run_in_their_phase_and_unsupported_or_skipped_ones_are_left_out() {
    let cluster = HookCluster::new();
    let manifests = vec![
        job("install", json!({HELM_HOOK: "pre-install", HELM_HOOK_WEIGHT: "1"})),
        job("test", json!({HELM_HOOK: "test"})),
        job("skipped", json!({HOOK: "Skip"})),
        deployment("web", json!({})),
    ];
    let mut ignored = Vec::new();

    let result = apply_sorted_manifests(&cluster, &manifests, &FieldOwnership::Nyl, 8, waits(), &mut |event| {
        if let ApplyEvent::HooksIgnored { hooks } = event {
            ignored.push(hooks.len());
        }
    })
    .await
    .unwrap();

    assert!(!result.sync_failed());
    assert_eq!(cluster.applies(), ["apply Job install", "apply Deployment web"]);
    assert_eq!(ignored, [1], "one warning for the Release, naming the hook");
    assert_eq!(result.ignored_hooks[0].key.name, "test");
    assert_eq!(result.untracked.len(), 3);
}
