//! The real `KubernetesPublication` driver against a target that declares
//! `reconciler: {direct: {}}`: Nyl publishes the tree and then applies its
//! Releases itself. The cluster is a mock client and an in-memory release
//! storage, so the order, the fence, the purge of Releases that left the tree,
//! and the two teardown phases run without a cluster.
//!
//! Contract: [Direct reconciler](../../../design/orchestration-core.md#direct-reconciler).

mod publication_world;

use std::collections::BTreeMap;
use std::sync::Arc;

use nyl_core::orchestration::driver::{
    Driver, DriverOperation, IncarnationStatus, Outcome, ResolutionContext, Supported, WaitCompletion,
};
use nyl_core::orchestration::ids::{EnvironmentName, ExecutionKey};
use nyl_core::orchestration::ports::CommitIdentity;
use nyl_core::orchestration::state::{ArtifactDocument, DesiredUnit, FailureCategory};
use nyl_render::kubernetes::{
    GroupVersionKind, InMemoryReleaseStorage, KubeClient, MockKubeClient, ReleaseStatus, ReleaseStorage,
};
use nyl_units::kubernetes_publication::direct::{ClusterAccess, ClusterHandle};
use nyl_units::kubernetes_publication::KubernetesPublicationDriver;
use publication_world::{context, context_with, deleting, desired, git, spec, Incarnations, NoTools, World};

fn catalogue() -> nyl_render::config::ResourceCatalogue {
    let mut descriptors = nyl_render::resources::catalog::resource_catalogue();
    descriptors.extend_from_slice(nyl_units::catalog::RESOURCES);
    nyl_render::config::ResourceCatalogue(descriptors)
}

fn identity() -> CommitIdentity {
    "Nyl Orchestration <nyl@example.invalid>".parse().unwrap()
}

/// The walkthrough project with its target declared direct, and its Cluster
/// declaring the context a direct target is applied through.
fn direct_world() -> World {
    let world = World::new();
    let platform = world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    let edited = text
        .replace(
            "  destination:\n    server: https://kubernetes.default.svc\n",
            "  destination:\n    server: https://kubernetes.default.svc\n  live: {context: kind}\n",
        )
        .replace(
            "  clusterRef: {name: dev}\n",
            "  clusterRef: {name: dev}\n  reconciler: {direct: {}}\n",
        );
    assert_ne!(text, edited);
    std::fs::write(&platform, edited).unwrap();
    world
}

/// A cluster that holds what the driver applied to it.
struct Cluster {
    client: Arc<MockKubeClient>,
    storage: Arc<InMemoryReleaseStorage>,
}

impl Cluster {
    fn new() -> Self {
        Self {
            client: Arc::new(MockKubeClient::new()),
            storage: Arc::new(InMemoryReleaseStorage::new()),
        }
    }

    fn driver(&self) -> KubernetesPublicationDriver {
        KubernetesPublicationDriver::new(catalogue()).with_cluster_access(ClusterAccess::Fixed(ClusterHandle {
            client: self.client.clone(),
            storage: self.storage.clone(),
        }))
    }

    async fn exists(&self, api_version: &str, kind: &str, namespace: Option<&str>, name: &str) -> bool {
        let gvk = GroupVersionKind::from_api_version_kind(api_version, kind).unwrap();
        self.client.get_resource(&gvk, namespace, name).await.unwrap().is_some()
    }
}

/// A unit resolved the way a run resolves it: the driver's recorded facts join the spec.
fn unit_for(driver: &dyn Driver, world: &World, name: &str) -> DesiredUnit {
    unit_with(driver, world, name, spec())
}

fn unit_with(driver: &dyn Driver, world: &World, name: &str, authored: serde_json::Value) -> DesiredUnit {
    let inputs = driver
        .execution_key_inputs(
            &ResolutionContext {
                instances: &nyl_core::orchestration::instance::NO_INSTANCES,
                source_roots: BTreeMap::default(),
                environment: EnvironmentName::new("dev").unwrap(),
                worktree: world.work(),
                project_dir: world.work().join("nyl"),
                env: BTreeMap::new(),
                deadline: chrono::Utc::now(),
                now: chrono::Utc::now(),
                tools: &NoTools,
                cache_dir: world.root.path().join("cache"),
            },
            &authored,
        )
        .unwrap();
    let mut resolved = authored;
    inputs.record_into(&mut resolved).unwrap();
    let key: ExecutionKey = ExecutionKey::from_digest_of(serde_json::to_string(&inputs.files).unwrap().as_bytes());
    let mut unit = desired(resolved, key);
    unit.unit.name = name.parse().unwrap();
    unit
}

fn tree_of(outcome: &Outcome) -> ArtifactDocument {
    match outcome {
        Outcome::Succeeded { artifacts, .. } => artifacts[0].clone(),
        other => panic!("expected a success, got {other:?}"),
    }
}

fn reconcile(
    driver: &dyn Driver,
    world: &World,
    unit: &DesiredUnit,
    receipt: Option<&ArtifactDocument>,
    others: &Incarnations,
) -> Outcome {
    let identity = identity();
    let mut ctx = context_with(world, DriverOperation::Reconcile, Some(&identity), others);
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    if let Some(receipt) = receipt {
        ctx.artifacts.insert("tree".to_owned(), receipt.clone());
    }
    match driver.reconcile(&ctx, unit).unwrap() {
        Supported::Supported(outcome) => outcome,
        Supported::Unsupported => panic!("the driver reconciles"),
    }
}

fn nobody() -> Incarnations {
    Incarnations(BTreeMap::new())
}

/// Catches a direct target publishing an Argo CD catalog, applying before
/// the tree is published, or recording a revision that cannot be tied to its
/// publication: the tree holds the Release and no Application, and the
/// revision is `Deployed`, owned by the unit, and cites the published commit
/// and destination.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_direct_reconcile_publishes_the_tree_then_applies_it_and_records_the_state() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");

    let outcome = reconcile(&driver, &world, &unit, None, &nobody());
    let tree = tree_of(&outcome);

    let files = world.files("deploy");
    assert!(files.iter().any(|path| path.starts_with("dev/web/")), "{files:?}");
    assert!(
        !files.iter().any(|path| path.starts_with("dev/_nyl/catalog")),
        "a direct tree holds no Application or AppProject: {files:?}"
    );
    assert_eq!(tree.spec["reconciler"], "direct");
    assert_eq!(tree.spec["cluster"], "dev");

    assert!(cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    let state = cluster.storage.get_latest_release("web", "web").await.unwrap().unwrap();
    assert_eq!(state.status, ReleaseStatus::Deployed);
    let owner = state.meta.owner.expect("the revision records its owner");
    assert_eq!(
        owner.to_string(),
        "unit dev/kubernetes (uid 00000000-0000-7000-8000-000000000004)"
    );
    let publication = state.meta.publication.expect("the revision records its publication");
    assert_eq!(
        Some(publication.commit.as_deref().unwrap()),
        tree.spec["commit"].as_str()
    );
    assert_eq!(publication.branch, "deploy");
    assert_eq!(publication.path_prefix, "dev");
}

/// Catches a second unit applying a Release another unit owns: the cluster
/// fence refuses before Git is touched, with its stable code, and the owner's
/// Release is left alone.
#[tokio::test(flavor = "multi_thread")]
async fn test_another_units_release_is_fenced_before_anything_is_published() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let first = unit_for(&driver, &world, "kubernetes");
    tree_of(&reconcile(&driver, &world, &first, None, &nobody()));
    let head = git(&world.bare(), &["rev-parse", "refs/heads/deploy"]);

    let second = unit_for(&driver, &world, "other");
    let owners = Incarnations(BTreeMap::from([(
        "dev/kubernetes".to_owned(),
        IncarnationStatus::Exists,
    )]));
    let outcome = reconcile(&driver, &world, &second, None, &owners);

    let Outcome::Failed {
        category,
        retryable,
        message,
    } = outcome
    else {
        panic!("the second unit is refused: {outcome:?}");
    };
    assert_eq!(category, FailureCategory::Ownership);
    assert!(retryable);
    assert!(message.contains("NYL-KUBERNETES-RELEASE-FENCED"), "{message}");
    assert_eq!(git(&world.bare(), &["rev-parse", "refs/heads/deploy"]), head);
    let state = cluster.storage.get_latest_release("web", "web").await.unwrap().unwrap();
    assert_eq!(state.revision, 1, "the owner's release is untouched");
}

/// Catches a unit that is gone not releasing its Release: its successor takes
/// the release over, and the new revision names it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_release_of_a_gone_owner_is_taken_over() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    tree_of(&reconcile(
        &driver,
        &world,
        &unit_for(&driver, &world, "kubernetes"),
        None,
        &nobody(),
    ));

    let successor = unit_for(&driver, &world, "other");
    tree_of(&reconcile(&driver, &world, &successor, None, &nobody()));
    let state = cluster.storage.get_latest_release("web", "web").await.unwrap().unwrap();
    assert_eq!(state.revision, 2);
    assert!(state.meta.owner.unwrap().to_string().starts_with("unit dev/other"));
}

/// Catches a Release that left the tree staying in the cluster, a purge before
/// the receipt's tree is compared, and a purge that takes the current Release
/// with it: the Release removed from Git is deleted with its state after the
/// next complete apply, and the other is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_release_that_left_the_tree_is_purged_after_a_complete_apply() {
    let world = direct_world();
    let extra = world.work().join("applications/web/extra.yaml");
    std::fs::write(
        &extra,
        "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: extra, namespace: extra}\nspec: {}\n---\n\
         apiVersion: v1\nkind: ConfigMap\nmetadata: {name: extra, namespace: extra}\ndata: {a: b}\n",
    )
    .unwrap();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let first = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    assert!(cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);

    std::fs::remove_file(&extra).unwrap();
    let unit = unit_for(&driver, &world, "kubernetes");
    tree_of(&reconcile(&driver, &world, &unit, Some(&first), &nobody()));

    assert!(!cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);
    assert!(cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
    assert!(cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(!cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
}

/// Catches a teardown that deletes the state it still needs, and one that
/// leaves workloads: phase 1 publishes the tree without its Releases and
/// deletes the workloads but keeps the release state and the Namespace that
/// holds it; phase 2 deletes the state. The Namespace's default `deletePolicy`
/// keeps it, as a remnant.
#[tokio::test(flavor = "multi_thread")]
async fn test_teardown_deletes_workloads_first_and_the_state_in_the_second_phase() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let tree = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    let identity = identity();

    let deleting = deleting(unit.clone());
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    ctx.artifacts.insert("tree".to_owned(), tree.clone());
    let outcome = driver.teardown(&ctx, &deleting);
    let Ok(Supported::Supported(Outcome::Waiting { phase, published, .. })) = outcome else {
        panic!("phase 1 waits: {outcome:?}");
    };
    assert_eq!(phase, "resources-deleted");
    assert!(published.is_some(), "phase 1's commit is its evidence");
    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(
        cluster.exists("v1", "Namespace", None, "web").await,
        "the Namespace that holds the release state stays until phase 2"
    );
    assert!(!cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    let files = world.files("deploy");
    assert!(!files.iter().any(|path| path.starts_with("dev/web/")), "{files:?}");
    assert!(files.contains(&"dev/_nyl/index.json".to_owned()), "{files:?}");

    ctx.teardown_wait = Some(WaitCompletion::Observed);
    let Supported::Supported(Outcome::Succeeded { .. }) = driver.teardown(&ctx, &deleting).unwrap() else {
        panic!("phase 2 completes");
    };
    assert!(cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    assert!(
        cluster.exists("v1", "Namespace", None, "web").await,
        "deletePolicy Confirm keeps a managed Namespace"
    );
    assert!(world.files("deploy").is_empty() || !world.files("deploy").contains(&"dev/_nyl/index.json".to_owned()));
}

/// Catches a Namespace with `deletePolicy: Automatic` surviving a teardown:
/// phase 2 deletes it after the state it held.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_automatic_namespace_is_deleted_with_the_state_it_held() {
    let world = direct_world();
    let group = world.work().join("nyl/web.yaml");
    let text = std::fs::read_to_string(&group).unwrap();
    std::fs::write(
        &group,
        text.replace(
            "  applicationNamespace: argocd\n",
            "  applicationNamespace: argocd\n  namespace: {deletePolicy: Automatic}\n",
        ),
    )
    .unwrap();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let tree = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    assert!(cluster.exists("v1", "Namespace", None, "web").await);
    let identity = identity();
    let deleting = deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    ctx.artifacts.insert("tree".to_owned(), tree);
    driver.teardown(&ctx, &deleting).unwrap();
    assert!(
        cluster.exists("v1", "Namespace", None, "web").await,
        "phase 1 defers it"
    );
    ctx.teardown_wait = Some(WaitCompletion::Observed);
    driver.teardown(&ctx, &deleting).unwrap();
    assert!(!cluster.exists("v1", "Namespace", None, "web").await);
}

/// Catches a direct target resolving without the context it is applied
/// through, which would otherwise fail only at the first apply.
#[test]
fn test_a_direct_cluster_without_a_context_fails_resolution_before_effects() {
    let world = World::new();
    let platform = world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    std::fs::write(
        &platform,
        text.replace(
            "  clusterRef: {name: dev}\n",
            "  clusterRef: {name: dev}\n  reconciler: {direct: {}}\n",
        ),
    )
    .unwrap();
    let driver = KubernetesPublicationDriver::new(catalogue());
    let error = publication_world::resolve_error(&driver, &world, &spec());
    assert!(error.contains("NYL-KUBERNETES-CLUSTER-CONTEXT-MISSING"), "{error}");
}

/// A clock that moves only when the driver waits, so a wait for health takes no time.
struct SteppingClock(std::sync::Mutex<chrono::DateTime<chrono::Utc>>);

impl nyl_core::orchestration::ports::Clock for SteppingClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().unwrap()
    }

    fn wait(&self, duration: nyl_core::orchestration::time::Duration) {
        let mut now = self.0.lock().unwrap();
        *now = duration.after(*now);
    }
}

fn waiting_spec(timeout: &str) -> serde_json::Value {
    let mut spec = spec();
    spec["observe"] = serde_json::json!({"waitHealthy": {"timeout": timeout}});
    spec
}

fn reconcile_waiting(driver: &dyn Driver, world: &World, unit: &DesiredUnit) -> Outcome {
    let identity = identity();
    let mut ctx = context(world, DriverOperation::Reconcile, Some(&identity));
    let clock = SteppingClock(std::sync::Mutex::new(ctx.now));
    ctx.clock = &clock;
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    match driver.reconcile(&ctx, unit).unwrap() {
        Supported::Supported(outcome) => outcome,
        Supported::Unsupported => panic!("the driver reconciles"),
    }
}

/// Catches a configured wait for health reporting success for a rollout that
/// never completes: the Deployment never reports its replicas available, so
/// the unit fails, retryable, with the stable code naming the Deployment and its
/// last health, after the tree was published and applied (no receipt).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_wait_for_health_that_times_out_fails_naming_the_worst_deployment() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_with(&driver, &world, "kubernetes", waiting_spec("30s"));

    let outcome = tokio::task::spawn_blocking({
        let (world, unit) = (world, unit);
        let driver = driver.clone();
        move || (reconcile_waiting(&driver, &world, &unit), world)
    })
    .await
    .unwrap();
    let (outcome, world) = outcome;

    let Outcome::Failed {
        category,
        retryable,
        message,
    } = outcome
    else {
        panic!("the wait fails: {outcome:?}");
    };
    assert_eq!(category, FailureCategory::Timeout);
    assert!(retryable);
    assert!(message.starts_with("NYL-KUBERNETES-HEALTH-TIMEOUT:"), "{message}");
    assert!(message.contains("web/web is accepted (Progressing"), "{message}");
    assert!(cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(world.files("deploy").iter().any(|path| path.starts_with("dev/web/")));
}

/// Catches the attestations a wait ends with being anything but what `nyl
/// attest` would record: a Deployment that needs no replicas is healthy at
/// once, and the reconcile reports `accepted` and `healthy` passing.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_wait_for_health_that_passes_reports_the_attestations() {
    let world = direct_world();
    let release = world.work().join("applications/web/release.yaml");
    let text = std::fs::read_to_string(&release).unwrap();
    std::fs::write(&release, text.replace("replicas: 1", "replicas: 0")).unwrap();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_with(&driver, &world, "kubernetes", waiting_spec("30s"));

    let outcome = tokio::task::spawn_blocking(move || reconcile_waiting(&driver, &world, &unit))
        .await
        .unwrap();

    let Outcome::Succeeded { attestations, .. } = outcome else {
        panic!("the wait passes: {outcome:?}");
    };
    let names = attestations
        .iter()
        .map(|attestation| attestation.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["accepted", "healthy"]);
    assert!(attestations
        .iter()
        .all(|attestation| attestation.result == nyl_core::orchestration::state::AttestationResult::Pass));
}
