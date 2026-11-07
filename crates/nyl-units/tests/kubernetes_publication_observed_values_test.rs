//! The real `KubernetesPublication` driver reading observed values, end to
//! end over the shared publication world and local fakes of Argo CD (its
//! Kubernetes API, or its API server) and of the workload Cluster, which is a
//! different Cluster, with a different kubeconfig context, than the Argo CD
//! control plane: resolution records what to read, and `observe` reads a
//! field of a resource the target renders only once the Application that
//! owns it runs the receipt's publication.
//!
//! Contract: [Kubernetes publication unit](../../../design/orchestration-core.md#kubernetes-publication-unit),
//! observed values, and [Observed values](../../../design/orchestration-core.md#observed-values).

mod argocd_stub;
mod publication_world;

use std::collections::BTreeMap;

use argocd_stub::{
    application_with_source, applications, argocd_api_full, deployment, kube_api_with_projects, kube_workload,
    kubeconfig_contexts, Applications, Stub,
};
use nyl_core::orchestration::driver::{
    Driver, DriverOperation, KeyInputs, ObservedValue, ObservedValues, Outcome, ResolutionContext, Supported,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey, RunId};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, Receipt};
use nyl_render::gitops::observation::{ReconcilerFacts, ReconcilerLocation};
use nyl_units::kubernetes_publication::KubernetesPublicationDriver;
use publication_world::{context, desired, git, spec, NoTools, World};
use serde_json::{json, Value};

const TOKEN: &str = "argocd-api-token";

fn real() -> KubernetesPublicationDriver {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    KubernetesPublicationDriver::new(nyl_render::config::ResourceCatalogue(descriptors))
}

fn identity() -> CommitIdentity {
    "Nyl Orchestration <nyl@example.invalid>".parse().unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Kubernetes,
    Api,
}

/// The Deployment the shared project renders, whose ready replicas the
/// observation reads.
fn replicas() -> Value {
    json!({
        "type": "integer",
        "from": {
            "resource": {"apiVersion": "apps/v1", "kind": "Deployment", "namespace": "web", "name": "web"},
            "pointer": "/status/readyReplicas"
        }
    })
}

/// The world with two clusters to read: Argo CD's control plane (its
/// Kubernetes API or API server) and the workload Cluster, as contexts of
/// one admitted kubeconfig.
struct Live {
    world: World,
    stub: Stub,
    workload: Stub,
    served: Applications,
    objects: Applications,
    env: BTreeMap<String, String>,
    directory: tempfile::TempDir,
}

impl Live {
    fn new(method: Method) -> Self {
        let world = World::new();
        let served = applications(Vec::new());
        let objects = applications(Vec::new());
        let directory = tempfile::TempDir::new().unwrap();
        let workload = kube_workload(objects.clone());
        let (stub, mut env) = match method {
            Method::Kubernetes => (
                kube_api_with_projects(served.clone(), applications(Vec::new())),
                BTreeMap::new(),
            ),
            Method::Api => (
                argocd_api_full(
                    TOKEN,
                    served.clone(),
                    std::sync::Arc::default(),
                    applications(Vec::new()),
                ),
                BTreeMap::from([("ARGOCD_AUTH_TOKEN".to_owned(), TOKEN.to_owned())]),
            ),
        };
        let file = directory.path().join("kubeconfig");
        let control = if method == Method::Kubernetes {
            stub.url()
        } else {
            "https://unused.invalid".to_owned()
        };
        kubeconfig_contexts(
            &file,
            &[("argocd-admin", &control), ("workload-admin", &workload.url())],
        );
        env.insert("KUBECONFIG".to_owned(), file.display().to_string());
        let platform = world.work().join("nyl/platform.yaml");
        let mut text = std::fs::read_to_string(&platform).unwrap();
        // The workload Cluster has its own context; Argo CD runs elsewhere.
        text = text.replace(
            "  kubernetes:\n    kubeVersion: 1.31.4",
            "  live: {context: workload-admin}\n  kubernetes:\n    kubeVersion: 1.31.4",
        );
        text = text.replace(
            "  clusterRef: {name: dev}\n  applicationGroupSelector",
            "  clusterRef: {name: dev}\n  argocdRef: {name: control}\n  applicationGroupSelector",
        );
        let access = match method {
            Method::Kubernetes => String::new(),
            Method::Api => format!("  access: {{method: api, api: {{server: '{}'}}}}\n", stub.url()),
        };
        text.push_str(&format!(
            "---\napiVersion: k8s.nyl/v1\nkind: Cluster\nmetadata: {{name: control}}\nspec:\n  destination:\n    server: https://control.example.invalid\n  live: {{context: argocd-admin}}\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions: [v1, apps/v1]\n    clusterScopedKinds: [core/Namespace]\n---\napiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata: {{name: control}}\nspec:\n  clusterRef: {{name: control}}\n  namespace: argocd\n{access}"
        ));
        std::fs::write(&platform, text).unwrap();
        git(&world.work(), &["add", "--all"]);
        git(
            &world.work(),
            &["commit", "--quiet", "--message", "Declare the clusters"],
        );
        Self {
            world,
            stub,
            workload,
            served,
            objects,
            env,
            directory,
        }
    }

    fn spec(observations: Value) -> Value {
        let mut spec = spec();
        spec["observe"] = json!({});
        spec["observations"] = observations;
        spec
    }

    /// The unit as a run resolves it: the driver's recorded facts join the spec.
    fn resolve(&self, driver: &dyn Driver, authored: Value) -> Result<DesiredUnit, String> {
        let inputs: KeyInputs = driver
            .execution_key_inputs(
                &ResolutionContext {
                    instances: &nyl_core::orchestration::instance::NO_INSTANCES,
                    source_roots: BTreeMap::default(),
                    environment: EnvironmentName::new("dev").unwrap(),
                    worktree: self.world.work(),
                    project_dir: self.world.work().join("nyl"),
                    env: BTreeMap::new(),
                    deadline: chrono::Utc::now(),
                    now: chrono::Utc::now(),
                    tools: &NoTools,
                    cache_dir: self.world.root.path().join("cache"),
                },
                &authored,
            )
            .map_err(|error| error.to_string())?;
        let mut resolved = authored;
        inputs.record_into(&mut resolved).unwrap();
        // The key leaves out the fields the driver excludes, as the engine's does.
        let mut keyed = resolved.clone();
        for pointer in &inputs.excluded_fields {
            if let Some((parent, last)) = pointer.rsplit_once('/') {
                if let Some(Value::Object(map)) = keyed.pointer_mut(parent) {
                    map.remove(last);
                }
            }
        }
        let key = ExecutionKey::from_digest_of(
            serde_json::to_string(&json!({"spec": keyed, "files": inputs.files}))
                .unwrap()
                .as_bytes(),
        );
        Ok(desired(resolved, key))
    }

    fn publish(&self, driver: &dyn Driver, unit: &DesiredUnit) -> (ArtifactDocument, String) {
        let id = identity();
        let Supported::Supported(Outcome::Succeeded { artifacts, .. }) = driver
            .reconcile(&context(&self.world, DriverOperation::Reconcile, Some(&id)), unit)
            .unwrap()
        else {
            panic!("the publication succeeds");
        };
        let tree = artifacts.into_iter().next().unwrap();
        let commit = tree.spec["commit"].as_str().unwrap().to_owned();
        (tree, commit)
    }

    fn observe(
        &self,
        driver: &dyn Driver,
        unit: &DesiredUnit,
        tree: &ArtifactDocument,
        env: &BTreeMap<String, String>,
    ) -> BTreeMap<String, ObservedValue> {
        let mut ctx = context(&self.world, DriverOperation::Observe, None);
        ctx.env = env.clone();
        ctx.artifacts = BTreeMap::from([("tree".to_owned(), tree.clone())]);
        let Supported::Supported(ObservedValues { values }) = driver.observe(&ctx, unit, &receipt(unit)).unwrap()
        else {
            panic!("an observing unit observes");
        };
        values
    }

    /// Serve every Application the unit's facts name, each running the
    /// revision `running(name)` returns, if any.
    fn serve(&self, unit: &DesiredUnit, running: impl Fn(&str) -> Option<String>) {
        let facts = facts(unit);
        *self.served.lock().unwrap() = facts
            .expected
            .deployments
            .iter()
            .filter_map(|deployment| {
                let (namespace, name) = (
                    deployment.locator["namespace"].as_str().unwrap(),
                    deployment.locator["name"].as_str().unwrap(),
                );
                let revision = running(name)?;
                let mut source = deployment.source.options.clone();
                if source.is_null() {
                    source = json!({});
                }
                source["repoURL"] = json!(deployment.source.repository);
                source["path"] = json!(deployment.source.path);
                source["targetRevision"] = json!("deploy");
                Some(application_with_source(
                    name,
                    namespace,
                    &source,
                    &[&revision],
                    "Healthy",
                ))
            })
            .collect();
    }

    /// The requests of the workload Cluster that read an object, not discover kinds.
    fn workload_reads(&self) -> usize {
        self.workload
            .requests()
            .iter()
            .filter(|request| request.path().contains("/deployments/"))
            .count()
    }
}

fn facts(unit: &DesiredUnit) -> ReconcilerFacts {
    serde_json::from_value(unit.resolved_spec.as_ref().unwrap()["reconciler"].clone()).unwrap()
}

fn receipt(unit: &DesiredUnit) -> Receipt {
    Receipt {
        execution_key: unit.execution_key.clone().unwrap(),
        behavior_version: 1,
        run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
        read_desired: None,
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        versions: BTreeMap::new(),
        approval: None,
        provenance: BTreeMap::new(),
        outputs: json!({}),
        artifacts: Vec::new(),
    }
}

fn pending_reason(value: &ObservedValue) -> &str {
    match value {
        ObservedValue::Pending { reason } => reason,
        other => panic!("expected pending, got {other:?}"),
    }
}

fn failed_reason(value: &ObservedValue) -> &str {
    match value {
        ObservedValue::Failed { reason } => reason,
        other => panic!("expected failed, got {other:?}"),
    }
}

/// Catches resolution recording nothing about what an observation reads, or
/// the wrong cluster: the owning Application (so `observe` renders nothing)
/// and the workload Cluster with its own context, which is not the
/// control-plane Cluster's. A unit without `observe` records none, and the
/// context, which says only how to read, never keys the unit.
#[test]
fn test_resolution_records_the_owning_application_and_the_workload_cluster() {
    let driver = real();
    let live = Live::new(Method::Kubernetes);
    let unit = live.resolve(&driver, Live::spec(json!({"ready": replicas()}))).unwrap();
    let resolved = unit.resolved_spec.as_ref().unwrap();
    assert_eq!(resolved["observationSources"], json!({"ready": {"deployment": "web"}}));
    assert_eq!(
        resolved["workload"],
        json!({"cluster": "dev", "context": "workload-admin"})
    );
    let ReconcilerLocation::ArgoCd(location) = facts(&unit).location;
    assert_eq!(location.cluster, "control", "Argo CD runs in another Cluster");
    assert_eq!(location.context.as_deref(), Some("argocd-admin"));
    assert!(driver.capabilities(&Live::spec(json!({}))).observed_values);
    assert!(!driver.capabilities(&spec()).observed_values);
    let publishing = live.resolve(&driver, spec()).unwrap();
    assert!(publishing.resolved_spec.as_ref().unwrap().get("workload").is_none());

    // Another local context for the same Cluster changes what is read through,
    // not what the unit produces: the key is unchanged.
    let platform = live.world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform)
        .unwrap()
        .replace("workload-admin", "renamed-admin");
    std::fs::write(&platform, text).unwrap();
    git(
        &live.world.work(),
        &["commit", "--quiet", "--all", "--message", "Rename the context"],
    );
    let renamed = live.resolve(&driver, Live::spec(json!({"ready": replicas()}))).unwrap();
    assert_eq!(
        renamed.resolved_spec.as_ref().unwrap()["workload"]["context"],
        "renamed-admin"
    );
    assert_eq!(renamed.execution_key, unit.execution_key);
}

/// Catches an observation of something the target does not render being
/// accepted (the cluster would then be asked about anything), and a Secret
/// being readable, with the driver's own codes and what to do.
#[test]
fn test_an_observation_must_read_a_resource_the_target_renders_and_never_a_secret() {
    let driver = real();
    let live = Live::new(Method::Kubernetes);
    let mut elsewhere = replicas();
    elsewhere["from"]["resource"]["name"] = json!("another");
    let error = live
        .resolve(&driver, Live::spec(json!({"ready": elsewhere})))
        .unwrap_err();
    assert!(error.starts_with("NYL-KUBERNETES-OBSERVATION-UNOWNED:"), "{error}");
    assert!(error.contains("Deployment web/another"), "{error}");
    let mut other_namespace = replicas();
    other_namespace["from"]["resource"]["namespace"] = json!("other");
    assert!(live
        .resolve(&driver, Live::spec(json!({"ready": other_namespace})))
        .unwrap_err()
        .starts_with("NYL-KUBERNETES-OBSERVATION-UNOWNED:"));
    let secret = json!({"type": "string", "from": {
        "resource": {"apiVersion": "v1", "kind": "Secret", "namespace": "web", "name": "web"},
        "pointer": "/data/token"
    }});
    let message = driver.validate_spec(&Live::spec(json!({"token": secret}))).unwrap_err();
    assert!(message.contains("never reads"), "{message}");
}

/// The whole path, for each Argo CD access method: until the Application
/// that owns the Deployment runs the receipt's publication nothing is read
/// from the workload Cluster and the value is pending; a slow, unrelated
/// Application (the catalog) holds nothing back; once it runs, the field is
/// read from the workload Cluster through its own context, and a field the
/// controller has not set yet is pending, a resource that is gone failed.
#[test]
fn test_observe_reads_the_field_only_once_its_application_runs_the_publication() {
    let driver = real();
    for method in [Method::Kubernetes, Method::Api] {
        let live = Live::new(method);
        let unit = live.resolve(&driver, Live::spec(json!({"ready": replicas()}))).unwrap();
        let (tree, commit) = live.publish(&driver, &unit);
        let ready = |live: &Live| live.observe(&driver, &unit, &tree, &live.env)["ready"].clone();
        *live.objects.lock().unwrap() = vec![deployment("web", "web", &json!({"readyReplicas": 3}))];

        // Argo CD knows the catalog, but not the Application yet.
        live.serve(&unit, |name| (name == "dev-catalog").then(|| "0".repeat(40)));
        let reason = pending_reason(&ready(&live)).to_owned();
        assert!(
            reason.starts_with("application-not-running-publication:") && reason.contains("Application argocd/web"),
            "{method:?}: {reason}"
        );
        // It runs an older publication.
        live.serve(&unit, |_| Some("0".repeat(40)));
        assert!(pending_reason(&ready(&live)).starts_with("application-not-running-publication:"));
        assert_eq!(
            live.workload_reads(),
            0,
            "{method:?}: nothing is read before the publication runs"
        );

        // The owning Application runs the publication; the catalog is slow.
        live.serve(&unit, |name| {
            Some(if name == "web" { commit.clone() } else { "0".repeat(40) })
        });
        assert_eq!(ready(&live), ObservedValue::Value { value: json!(3) }, "{method:?}");
        assert!(live.workload_reads() > 0);
        // Argo CD was read through its own endpoint, never the workload's.
        assert!(!live.stub.requests().is_empty());
        assert!(live
            .workload
            .requests()
            .iter()
            .all(|request| !request.path().contains("argoproj.io")));

        // The controller has not filled the field in yet.
        *live.objects.lock().unwrap() = vec![deployment("web", "web", &json!({}))];
        let reason = pending_reason(&ready(&live)).to_owned();
        assert!(
            reason.starts_with("field-not-set:") && reason.contains("/status/readyReplicas"),
            "{reason}"
        );
        // The resource is gone from the workload Cluster although its Application runs the publication.
        live.objects.lock().unwrap().clear();
        let reason = failed_reason(&ready(&live)).to_owned();
        assert!(reason.contains("does not exist in the workload Cluster"), "{reason}");
    }
}

/// Catches a reading made with ambient credentials, or a missing credential
/// reported as a pending value: each is a failed read naming the variable
/// and the `env` entry that admits it, and the declaration that is missing.
#[test]
fn test_observe_fails_without_the_admitted_credentials_and_never_reads_ambient_ones() {
    let driver = real();
    let live = Live::new(Method::Kubernetes);
    let unit = live.resolve(&driver, Live::spec(json!({"ready": replicas()}))).unwrap();
    let (tree, commit) = live.publish(&driver, &unit);
    live.serve(&unit, |_| Some(commit.clone()));
    *live.objects.lock().unwrap() = vec![deployment("web", "web", &json!({"readyReplicas": 3}))];

    // No kubeconfig admitted: nothing is read, from either cluster.
    let values = live.observe(&driver, &unit, &tree, &BTreeMap::new());
    let reason = failed_reason(&values["ready"]);
    assert!(
        reason.contains("KUBECONFIG") && reason.contains("env.passthrough"),
        "{reason}"
    );
    assert!(live.stub.requests().is_empty() && live.workload.requests().is_empty());

    // A kubeconfig without the workload Cluster's context fails there.
    let file = live.directory.path().join("partial");
    kubeconfig_contexts(&file, &[("argocd-admin", &live.stub.url())]);
    let env = BTreeMap::from([("KUBECONFIG".to_owned(), file.display().to_string())]);
    let reason = failed_reason(&live.observe(&driver, &unit, &tree, &env)["ready"]).to_owned();
    assert!(reason.contains("workload-admin"), "{reason}");
    assert!(live.workload.requests().is_empty());

    // A workload Cluster that declares no context cannot be read at all.
    let platform = live.world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform)
        .unwrap()
        .replace("  live: {context: workload-admin}\n", "");
    std::fs::write(&platform, text).unwrap();
    git(
        &live.world.work(),
        &["commit", "--quiet", "--all", "--message", "No context"],
    );
    let undeclared = live.resolve(&driver, Live::spec(json!({"ready": replicas()}))).unwrap();
    let reason = failed_reason(&live.observe(&driver, &undeclared, &tree, &live.env)["ready"]).to_owned();
    assert!(
        reason.contains("Cluster dev")
            && reason.contains("spec.live.context")
            && reason.contains("never uses the current context"),
        "{reason}"
    );
}

/// A document resolved before the unit observed values records nothing to
/// read: every observation fails with the way out, never a driver error.
#[test]
fn test_observe_of_a_document_without_recorded_sources_fails_with_the_way_out() {
    let driver = real();
    let live = Live::new(Method::Kubernetes);
    let authored = Live::spec(json!({"ready": replicas()}));
    let unit = desired(authored, ExecutionKey::from_digest_of(b"old"));
    let values = live.observe(
        &driver,
        &unit,
        &ArtifactDocument {
            api_version: nyl_core::constants::API_VERSION_ARTIFACTS.to_owned(),
            kind: "PublishedTree".to_owned(),
            metadata: serde_json::from_value(json!({
                "name": "tree",
                "unit": {"environment": "dev", "name": "kubernetes", "uid": "00000000-0000-7000-8000-000000000004"}
            }))
            .unwrap(),
            execution_key: ExecutionKey::from_digest_of(b"old"),
            spec: json!({"commit": "a".repeat(40)}),
        },
        &live.env,
    );
    assert!(failed_reason(&values["ready"]).contains("reconcile it"), "{values:?}");
}
