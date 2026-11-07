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

const EXTRA: &str = "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: extra, namespace: extra}\nspec: {}\n---\n\
                     apiVersion: v1\nkind: ConfigMap\nmetadata: {name: extra, namespace: extra}\ndata: {a: b}\n";

/// Add a second Release, `extra`, to the walkthrough group.
fn add_extra(world: &World) -> std::path::PathBuf {
    let extra = world.work().join("applications/web/extra.yaml");
    std::fs::write(&extra, EXTRA).unwrap();
    extra
}

/// Make Release `web` own Namespace `extra`, where Release `extra` keeps its
/// state, and let the Namespaces be deleted with their Release.
fn share_namespace_of_extra_with_web(world: &World) {
    let group = world.work().join("nyl/web.yaml");
    let text = std::fs::read_to_string(&group).unwrap();
    std::fs::write(
        &group,
        text.replace(
            "  applicationNamespace: argocd\n",
            "  applicationNamespace: argocd\n  namespace: {prunePolicy: Automatic, deletePolicy: Automatic}\n  \
             sharedNamespaces:\n    extra: {owner: {kind: Release, applicationGroup: web, release: web}}\n",
        ),
    )
    .unwrap();
    let release = world.work().join("applications/web/release.yaml");
    let text = std::fs::read_to_string(&release).unwrap();
    std::fs::write(
        &release,
        text.replacen(
            "  namespace: web\nspec:\n",
            "  namespace: web\nspec:\n  additionalNamespaces: [extra]\n",
            1,
        ),
    )
    .unwrap();
}

/// Catches a shared Namespace being deleted together with another Release's
/// state: Release `web` owns Namespace `extra`, where Release `extra` keeps its
/// release state. Phase 1 must purge both Releases' objects but leave the
/// Namespace with the state in it, and phase 2 delete the state of both and
/// then the Namespace, so the teardown neither wedges nor leaves state behind.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_namespace_owned_by_one_release_holding_the_state_of_another_survives_phase_1() {
    let world = direct_world();
    add_extra(&world);
    share_namespace_of_extra_with_web(&world);
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let tree = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    assert!(
        cluster.exists("v1", "Namespace", None, "extra").await,
        "web applies the shared Namespace"
    );
    assert!(!cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());

    let identity = identity();
    let deleting = deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    ctx.artifacts.insert("tree".to_owned(), tree);
    let outcome = driver.teardown(&ctx, &deleting);
    assert!(
        matches!(outcome, Ok(Supported::Supported(Outcome::Waiting { .. }))),
        "{outcome:?}"
    );
    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(!cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);
    assert!(
        cluster.exists("v1", "Namespace", None, "extra").await,
        "the Namespace that holds the state of Release extra stays through phase 1"
    );
    assert!(!cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
    assert!(!cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());

    ctx.teardown_wait = Some(WaitCompletion::Observed);
    let outcome = driver.teardown(&ctx, &deleting);
    assert!(
        matches!(outcome, Ok(Supported::Supported(Outcome::Succeeded { .. }))),
        "{outcome:?}"
    );
    assert!(cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
    assert!(cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    assert!(
        !cluster.exists("v1", "Namespace", None, "extra").await,
        "phase 2 deletes the Namespace once no state is left in it"
    );
}

/// Catches an object that moves between two Releases being deleted: Release
/// `extra` applies before Release `web` (keys run in order), so when a ClusterRole
/// moves from `web` to `extra`, `web`'s prune would otherwise delete what `extra`
/// just applied. What another current Release renders is left alone.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_object_that_moves_to_another_release_is_not_pruned_by_the_one_that_lost_it() {
    let world = direct_world();
    add_extra(&world);
    let release = world.work().join("applications/web/release.yaml");
    let original = std::fs::read_to_string(&release).unwrap();
    let moving =
        "---\napiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata: {name: moving}\nrules: []\n";
    std::fs::write(&release, format!("{original}{moving}")).unwrap();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let first = tree_of(&reconcile(
        &driver,
        &world,
        &unit_for(&driver, &world, "kubernetes"),
        None,
        &nobody(),
    ));
    let role = cluster.exists("rbac.authorization.k8s.io/v1", "ClusterRole", None, "moving");
    assert!(role.await);

    std::fs::write(&release, &original).unwrap();
    std::fs::write(
        world.work().join("applications/web/extra.yaml"),
        format!("{EXTRA}{moving}"),
    )
    .unwrap();
    let unit = unit_for(&driver, &world, "kubernetes");
    tree_of(&reconcile(&driver, &world, &unit, Some(&first), &nobody()));

    assert!(
        cluster
            .exists("rbac.authorization.k8s.io/v1", "ClusterRole", None, "moving")
            .await,
        "Release extra renders it now, and Release web must not delete it"
    );
    let web = cluster.storage.get_latest_release("web", "web").await.unwrap().unwrap();
    assert!(!web.resource_keys.iter().any(|key| key.name == "moving"));
    let extra = cluster
        .storage
        .get_latest_release("extra", "extra")
        .await
        .unwrap()
        .unwrap();
    assert!(extra.resource_keys.iter().any(|key| key.name == "moving"));
}

/// Catches a unit whose deadline has passed going on to apply: no Release is
/// applied, no state is recorded, and the unit fails retryably as a timeout.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_passed_deadline_applies_nothing_and_fails_as_a_timeout() {
    let world = direct_world();
    add_extra(&world);
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let identity = identity();
    let mut ctx = context(&world, DriverOperation::Reconcile, Some(&identity));
    ctx.deadline = ctx.now;

    let outcome = match driver.reconcile(&ctx, &unit).unwrap() {
        Supported::Supported(outcome) => outcome,
        Supported::Unsupported => panic!("the driver reconciles"),
    };

    let Outcome::Failed {
        category, retryable, ..
    } = outcome
    else {
        panic!("the deadline fails the unit: {outcome:?}");
    };
    assert_eq!(category, FailureCategory::Timeout);
    assert!(retryable);
    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    assert!(cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
}

/// Record a new revision of Release `extra` as another claimant would: the
/// same objects and publication, with `owner` (none for a `--take-over` apply
/// by hand) in place of the unit.
async fn reclaim_extra(cluster: &Cluster, owner: Option<nyl_render::gitops::reconcile::RenderIndexOwner>) {
    let latest = cluster
        .storage
        .get_latest_release("extra", "extra")
        .await
        .unwrap()
        .unwrap();
    let mut next = latest.clone();
    next.revision = latest.revision + 1;
    next.status = ReleaseStatus::Deployed;
    next.meta.owner = owner;
    cluster.storage.save_release(&next).await.unwrap();
}

/// Catches a Release that an apply by hand took over being purged when it
/// leaves Git: no unit owns it, so it is left with its objects, and the run
/// reports it with the stable code instead of deleting what nobody this unit
/// can tell is its own.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_release_taken_over_by_hand_is_left_and_reported_when_it_leaves_the_tree() {
    let world = direct_world();
    let extra = add_extra(&world);
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let first = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    reclaim_extra(&cluster, None).await;

    std::fs::remove_file(&extra).unwrap();
    let unit = unit_for(&driver, &world, "kubernetes");
    let outcome = reconcile(&driver, &world, &unit, Some(&first), &nobody());

    let Outcome::Succeeded { warnings, .. } = outcome else {
        panic!("the apply still succeeds: {outcome:?}");
    };
    assert!(
        warnings.iter().any(|warning| warning.code
            == nyl_core::orchestration::codes::Code::KubernetesReleaseLeftBehind
            && warning.message.contains("extra/extra")),
        "{warnings:?}"
    );
    assert!(cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);
    assert!(!cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
}

/// Catches a teardown that purges a Release another unit owns, or leaves its
/// state Namespace to be deleted: the foreign Release's objects and the
/// Namespace holding its state stay, the unit's own Release is removed, the
/// first phase reports what it left, and the second phase completes with a
/// warning instead of waiting on what is not this unit's.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_teardown_leaves_a_release_another_unit_took_over_and_reports_it() {
    let world = direct_world();
    add_extra(&world);
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let tree = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    let other = nyl_render::gitops::reconcile::RenderIndexOwner::unit("dev", "other", "uid-other");
    reclaim_extra(&cluster, Some(other)).await;
    let alive = Incarnations(BTreeMap::from([("dev/other".to_owned(), IncarnationStatus::Exists)]));

    let identity = identity();
    let deleting = deleting(unit);
    let mut ctx = context_with(&world, DriverOperation::Teardown, Some(&identity), &alive);
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    ctx.artifacts.insert("tree".to_owned(), tree);
    let outcome = driver.teardown(&ctx, &deleting);
    let Ok(Supported::Supported(Outcome::Waiting { reason, .. })) = outcome else {
        panic!("phase 1 waits: {outcome:?}");
    };
    assert!(reason.contains("extra/extra"), "phase 1 names what it left: {reason}");
    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);
    assert!(cluster.exists("v1", "Namespace", None, "extra").await);

    ctx.teardown_wait = Some(WaitCompletion::Observed);
    let outcome = driver.teardown(&ctx, &deleting);
    let Ok(Supported::Supported(Outcome::Succeeded { warnings, .. })) = outcome else {
        panic!("phase 2 completes: {outcome:?}");
    };
    assert!(
        warnings
            .iter()
            .any(|warning| warning.code == nyl_core::orchestration::codes::Code::KubernetesReleaseLeftBehind),
        "{warnings:?}"
    );
    assert!(cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    assert!(!cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
    assert!(cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);
    assert!(
        cluster.exists("v1", "Namespace", None, "extra").await,
        "the Namespace that holds the foreign Release's state is never deleted"
    );
}

/// Catches a teardown that knows only the newest publication when the unit
/// has no stored receipt (the engine's stored artifacts are empty for a unit
/// that never completed): a Release that already left the tree is still in the
/// cluster, and the teardown removes it with the rest.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_teardown_without_a_receipt_removes_a_release_that_left_the_tree() {
    let world = direct_world();
    let extra = add_extra(&world);
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    // The Release leaves Git, and the unit is torn down before another reconcile.
    std::fs::remove_file(&extra).unwrap();
    let unit = unit_for(&driver, &world, "kubernetes");
    // A publication without the Release whose apply never ran: the deadline had passed.
    let identity = identity();
    let mut ctx = context(&world, DriverOperation::Reconcile, Some(&identity));
    ctx.deadline = ctx.now;
    let _ = driver.reconcile(&ctx, &unit);
    let files = world.files("deploy");
    assert!(files.iter().any(|path| path.starts_with("dev/web/web")), "{files:?}");
    assert!(!files.iter().any(|path| path.starts_with("dev/web/extra")), "{files:?}");
    assert!(cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);

    let deleting = deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    assert!(ctx.artifacts.is_empty(), "no stored artifacts reach the driver");
    driver.teardown(&ctx, &deleting).unwrap();

    assert!(!cluster.exists("v1", "ConfigMap", Some("extra"), "extra").await);
    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
}

/// Catches a purge that takes the union of every stored revision: an object an
/// earlier revision tracked and a later one untracked (as `Prune=false` does)
/// is still in the cluster by design, and the teardown must leave it. The purge
/// reads what a prune reads, the latest `Deployed` revision's objects and those
/// of the `Failed` revisions after it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_teardown_leaves_an_object_an_earlier_revision_tracked_and_a_later_one_dropped() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let tree = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    let kept = serde_json::json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": "kept", "namespace": "web", "annotations": {"argocd.argoproj.io/sync-options": "Prune=false"}}
    });
    let key = nyl_render::kubernetes::ResourceKey::from_json_value(&kept).unwrap();
    cluster
        .client
        .add_resource(key.clone(), serde_json::from_value(kept).unwrap());
    let mut first = cluster.storage.get_release("web", "web", 1).await.unwrap().unwrap();
    let current = first.clone();
    first.resource_keys.push(key);
    first.status = ReleaseStatus::Superseded;
    cluster.storage.complete_release(&first).await.unwrap();
    let mut second = current;
    second.revision = 2;
    cluster.storage.save_release(&second).await.unwrap();

    let identity = identity();
    let deleting = deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    ctx.artifacts.insert("tree".to_owned(), tree);
    driver.teardown(&ctx, &deleting).unwrap();

    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(
        cluster.exists("v1", "ConfigMap", Some("web"), "kept").await,
        "no current revision tracks it"
    );
}

/// A cluster client that behaves like an API server where the mock is lenient:
/// a custom kind is unknown until its CustomResourceDefinition was applied, and
/// deleting a named object can be made to fail.
struct StrictClient {
    inner: Arc<MockKubeClient>,
    registered: std::sync::Mutex<std::collections::HashSet<String>>,
    failing_deletes: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl StrictClient {
    fn new(inner: Arc<MockKubeClient>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            registered: std::sync::Mutex::default(),
            failing_deletes: std::sync::Mutex::default(),
        })
    }
}

#[async_trait::async_trait]
impl KubeClient for StrictClient {
    async fn get_resource(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
    ) -> nyl_render::Result<Option<kube::api::DynamicObject>> {
        self.inner.get_resource(gvk, namespace, name).await
    }

    async fn apply_resource(
        &self,
        resource: &kube::api::DynamicObject,
        manager: &str,
        dry_run: bool,
    ) -> nyl_render::Result<nyl_render::kubernetes::ApplyOutcome> {
        let object = serde_json::to_value(resource).unwrap();
        if object["kind"] == "CustomResourceDefinition" && !dry_run {
            if let Some(kind) = object.pointer("/spec/names/kind").and_then(serde_json::Value::as_str) {
                self.registered.lock().unwrap().insert(kind.to_owned());
            }
        }
        self.inner.apply_resource(resource, manager, dry_run).await
    }

    async fn get_server_version(&self) -> nyl_render::Result<String> {
        self.inner.get_server_version().await
    }

    async fn get_api_versions(&self) -> nyl_render::Result<Vec<String>> {
        self.inner.get_api_versions().await
    }

    async fn is_namespaced(&self, gvk: &GroupVersionKind) -> nyl_render::Result<bool> {
        if gvk.group == "example.com" && !self.registered.lock().unwrap().contains(&gvk.kind) {
            return Err(nyl_render::NylError::ApiResourceNotFound(gvk.kind.clone()));
        }
        self.inner.is_namespaced(gvk).await
    }

    fn default_namespace(&self) -> &str {
        self.inner.default_namespace()
    }

    async fn delete_resource(
        &self,
        gvk: &GroupVersionKind,
        namespace: Option<&str>,
        name: &str,
    ) -> nyl_render::Result<()> {
        if gvk.kind == "Namespace" && self.failing_deletes.lock().unwrap().contains(name) {
            return Err(nyl_render::NylError::Kubernetes(
                "the API server is unavailable".to_owned(),
            ));
        }
        self.inner.delete_resource(gvk, namespace, name).await
    }

    async fn get_normalized_resource(
        &self,
        resource: &kube::api::DynamicObject,
        manager: &str,
    ) -> nyl_render::Result<kube::api::DynamicObject> {
        self.inner.get_normalized_resource(resource, manager).await
    }

    async fn release_field_manager(
        &self,
        resource: &kube::api::DynamicObject,
        manager: &str,
    ) -> nyl_render::Result<bool> {
        self.inner.release_field_manager(resource, manager).await
    }
}

impl Cluster {
    /// The same cluster, reached through `client`.
    fn driver_through(&self, client: Arc<dyn KubeClient>) -> KubernetesPublicationDriver {
        KubernetesPublicationDriver::new(catalogue()).with_cluster_access(ClusterAccess::Fixed(ClusterHandle {
            client,
            storage: self.storage.clone(),
        }))
    }
}

/// Catches the first deploy of a custom resource failing before it starts: Releases
/// are prepared when their wave comes, because a namespaced custom resource
/// without a `metadata.namespace` cannot be resolved until the wave-0 Release
/// that registers its CustomResourceDefinition applied.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_custom_resource_in_wave_1_resolves_after_its_crd_applied_in_wave_0() {
    let world = direct_world();
    std::fs::write(
        world.work().join("applications/web/crds.yaml"),
        "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: crds, namespace: crds}\nspec: {}\n---\n\
         apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata: {name: widgets.example.com}\n\
         spec:\n  group: example.com\n  scope: Namespaced\n  names: {kind: Widget, plural: widgets}\n  \
         versions: [{name: v1, served: true, storage: true, schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}}]\n",
    )
    .unwrap();
    std::fs::write(
        world.work().join("applications/web/widgets.yaml"),
        "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: widgets, namespace: widgets}\nspec: {}\n---\n\
         apiVersion: example.com/v1\nkind: Widget\nmetadata: {name: first}\nspec: {size: 1}\n",
    )
    .unwrap();
    let cluster = Cluster::new();
    let strict: Arc<dyn KubeClient> = StrictClient::new(cluster.client.clone());
    let driver = cluster.driver_through(strict);
    let unit = unit_for(&driver, &world, "kubernetes");

    let outcome = reconcile(&driver, &world, &unit, None, &nobody());

    tree_of(&outcome);
    assert!(
        cluster
            .exists("example.com/v1", "Widget", Some("widgets"), "first")
            .await,
        "the custom resource lands in its Release's namespace"
    );
    assert!(
        cluster
            .exists(
                "apiextensions.k8s.io/v1",
                "CustomResourceDefinition",
                None,
                "widgets.example.com"
            )
            .await
    );
}

/// Catches a teardown retry that cannot find a Namespace: Release `web` owns
/// Namespace `extra`, where Release `extra` keeps its state. When phase 2 fails to
/// delete the Namespace, the state of `web`, the only record that it tracked
/// it, stays, so the retry finds it and the Namespace does not leak.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_namespace_deletion_keeps_the_state_that_tracks_it_for_the_retry() {
    let world = direct_world();
    add_extra(&world);
    share_namespace_of_extra_with_web(&world);
    let cluster = Cluster::new();
    let strict = StrictClient::new(cluster.client.clone());
    let driver = cluster.driver_through(strict.clone());
    let unit = unit_for(&driver, &world, "kubernetes");
    let tree = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    let identity = identity();
    let deleting = deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    ctx.artifacts.insert("tree".to_owned(), tree);
    driver.teardown(&ctx, &deleting).unwrap();

    strict.failing_deletes.lock().unwrap().insert("extra".to_owned());
    ctx.teardown_wait = Some(WaitCompletion::Observed);
    let outcome = driver.teardown(&ctx, &deleting);
    let Ok(Supported::Supported(Outcome::Failed { retryable, .. })) = outcome else {
        panic!("phase 2 fails: {outcome:?}");
    };
    assert!(retryable);
    assert!(cluster.exists("v1", "Namespace", None, "extra").await);
    assert!(
        !cluster.storage.list_revisions("web", "web").await.unwrap().is_empty(),
        "the state of the Release that tracked the Namespace stays for the retry"
    );

    strict.failing_deletes.lock().unwrap().clear();
    let outcome = driver.teardown(&ctx, &deleting);
    assert!(
        matches!(outcome, Ok(Supported::Supported(Outcome::Succeeded { .. }))),
        "{outcome:?}"
    );
    assert!(
        !cluster.exists("v1", "Namespace", None, "extra").await,
        "the retry deleted it"
    );
    assert!(cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    assert!(cluster
        .storage
        .list_revisions("extra", "extra")
        .await
        .unwrap()
        .is_empty());
}

/// Catches the purge of Releases that left Git deleting a Namespace that another
/// Release's state is in: Release `web` (namespace `web`, sorts last) owns
/// Namespace `aaa-state`, where Release `aaa` keeps its state, and both leave the
/// tree. Planned together, the Namespace is deleted after all state is gone, so
/// the purge neither destroys state early nor leaks the Namespace.
#[tokio::test(flavor = "multi_thread")]
async fn test_vanished_releases_sharing_a_state_namespace_are_purged_together() {
    let world = direct_world();
    let applications = world.work().join("applications/web");
    std::fs::write(
        applications.join("keep.yaml"),
        "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: keep, namespace: keep}\nspec: {}\n---\n\
         apiVersion: v1\nkind: ConfigMap\nmetadata: {name: keep, namespace: keep}\ndata: {a: b}\n",
    )
    .unwrap();
    std::fs::write(
        applications.join("aaa.yaml"),
        "apiVersion: k8s.nyl/v1\nkind: Release\nmetadata: {name: aaa, namespace: aaa-state}\nspec: {}\n---\n\
         apiVersion: v1\nkind: ConfigMap\nmetadata: {name: aaa, namespace: aaa-state}\ndata: {a: b}\n",
    )
    .unwrap();
    let group = world.work().join("nyl/web.yaml");
    let original = std::fs::read_to_string(&group).unwrap();
    std::fs::write(
        &group,
        original.replace(
            "  applicationNamespace: argocd\n",
            "  applicationNamespace: argocd\n  namespace: {prunePolicy: Automatic, deletePolicy: Automatic}\n  \
             sharedNamespaces:\n    aaa-state: {owner: {kind: Release, applicationGroup: web, release: web}}\n",
        ),
    )
    .unwrap();
    let release = applications.join("release.yaml");
    let web = std::fs::read_to_string(&release).unwrap();
    std::fs::write(
        &release,
        web.replacen(
            "  namespace: web\nspec:\n",
            "  namespace: web\nspec:\n  additionalNamespaces: [aaa-state]\n",
            1,
        ),
    )
    .unwrap();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    let first = tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    assert!(cluster.exists("v1", "Namespace", None, "aaa-state").await);
    assert!(!cluster
        .storage
        .list_revisions("aaa", "aaa-state")
        .await
        .unwrap()
        .is_empty());

    // `web` and `aaa` leave the tree, and the group stops sharing the Namespace.
    std::fs::remove_file(&release).unwrap();
    std::fs::remove_file(applications.join("aaa.yaml")).unwrap();
    std::fs::write(&group, original).unwrap();
    let platform = world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    let start = text.find("  releaseInputs:\n").unwrap();
    let end = text[start..].find("---\n").unwrap() + start;
    std::fs::write(&platform, format!("{}{}", &text[..start], &text[end..])).unwrap();
    let unit = unit_with(&driver, &world, "kubernetes", serde_json::json!({"target": "dev"}));
    let outcome = reconcile(&driver, &world, &unit, Some(&first), &nobody());

    assert!(matches!(outcome, Outcome::Succeeded { .. }), "{outcome:?}");
    assert!(!cluster.exists("apps/v1", "Deployment", Some("web"), "web").await);
    assert!(!cluster.exists("v1", "ConfigMap", Some("aaa-state"), "aaa").await);
    assert!(
        !cluster.exists("v1", "Namespace", None, "aaa-state").await,
        "deleted once the state in it was gone"
    );
    assert!(cluster.storage.list_revisions("web", "web").await.unwrap().is_empty());
    assert!(cluster
        .storage
        .list_revisions("aaa", "aaa-state")
        .await
        .unwrap()
        .is_empty());
    assert!(cluster.exists("v1", "ConfigMap", Some("keep"), "keep").await);
    assert!(!cluster.storage.list_revisions("keep", "keep").await.unwrap().is_empty());
}

/// Catches the teardown refusal after a reconciler change pointing at the very
/// operation that is refused: the teardown says to restore the declaration.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_teardown_after_a_reconciler_change_says_to_restore_the_declaration() {
    let world = direct_world();
    let cluster = Cluster::new();
    let driver = cluster.driver();
    let unit = unit_for(&driver, &world, "kubernetes");
    tree_of(&reconcile(&driver, &world, &unit, None, &nobody()));
    let platform = world.work().join("nyl/platform.yaml");
    let text = std::fs::read_to_string(&platform).unwrap();
    std::fs::write(&platform, text.replace("  reconciler: {direct: {}}\n", "")).unwrap();

    let identity = identity();
    let deleting = deleting(unit);
    let mut ctx = context(&world, DriverOperation::Teardown, Some(&identity));
    ctx.deadline = ctx.now + chrono::Duration::minutes(10);
    let outcome = driver.teardown(&ctx, &deleting);

    let Ok(Supported::Supported(Outcome::Failed { message, .. })) = outcome else {
        panic!("the teardown is refused: {outcome:?}");
    };
    assert!(message.contains("NYL-KUBERNETES-RECONCILER-CHANGED"), "{message}");
    assert!(message.contains("Restore the declaration"), "{message}");
    assert!(!message.contains("nyl teardown"), "{message}");
}
