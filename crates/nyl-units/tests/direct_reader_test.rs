//! The direct reader over a mock cluster: what it makes of the release state
//! Nyl recorded and of the workloads, so the shared evaluation can judge a
//! direct target exactly as it judges Argo CD.
//!
//! Contract: [Reading a direct target](../../../design/promotion.md#reading-a-direct-target).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kube::api::DynamicObject;
use nyl_render::gitops::observation::{
    Anchor, DeploymentRole, DirectLocation, ExpectedDeployment, ObservedPublication, Source, DIRECT_ANCHOR,
};
use nyl_render::kubernetes::{
    GroupVersionKind, InMemoryReleaseStorage, MockKubeClient, ReleaseMeta, ReleasePublication, ReleaseState,
    ReleaseStatus, ReleaseStorage, ResourceKey,
};
use nyl_units::kubernetes_publication::direct::{ClusterAccess, ClusterHandle, DirectReader};
use nyl_units::kubernetes_publication::reconciler::{
    Health, Proof, ReadRequest, ReconcilerReader, Running, Visibility,
};
use serde_json::json;

const REPOSITORY: &str = "https://example.invalid/deploy.git";

fn publication(prefix: &str, commit: Option<&str>) -> ReleasePublication {
    ReleasePublication {
        repository: REPOSITORY.to_owned(),
        branch: "deploy".to_owned(),
        path_prefix: prefix.to_owned(),
        commit: commit.map(str::to_owned),
    }
}

fn deployment() -> ExpectedDeployment {
    ExpectedDeployment {
        id: "web/web".to_owned(),
        display: "web/web".to_owned(),
        role: DeploymentRole::Workload,
        path: "dev/web/web".to_owned(),
        release: Some("web/web".to_owned()),
        source: Source {
            repository: REPOSITORY.to_owned(),
            path: "dev/web/web".to_owned(),
            options: serde_json::Value::Null,
        },
        locator: json!({"name": "web", "namespace": "web", "destinationNamespace": "web"}),
    }
}

fn anchors() -> Vec<Anchor> {
    vec![Anchor {
        id: DIRECT_ANCHOR.to_owned(),
        locator: serde_json::Value::Null,
    }]
}

const MANIFEST: &str =
    "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: web, namespace: web}\nspec: {replicas: 2}\n";

fn key(api_version: &str, kind: &str, namespace: Option<&str>, name: &str) -> ResourceKey {
    ResourceKey {
        gvk: GroupVersionKind::from_api_version_kind(api_version, kind).unwrap(),
        namespace: namespace.map(str::to_owned),
        name: name.to_owned(),
    }
}

fn revision(number: u32, status: ReleaseStatus, publication: Option<ReleasePublication>) -> ReleaseState {
    ReleaseState {
        release_name: "web".to_owned(),
        release_namespace: "web".to_owned(),
        revision: number,
        resource_keys: vec![
            key("apps/v1", "Deployment", Some("web"), "web"),
            key("v1", "Namespace", None, "web"),
        ],
        manifest: MANIFEST.to_owned(),
        status,
        rendered_at: Utc::now(),
        applied_at: None,
        error: None,
        meta: ReleaseMeta {
            owner: None,
            publication,
        },
    }
}

struct World {
    client: Arc<MockKubeClient>,
    storage: Arc<InMemoryReleaseStorage>,
}

impl World {
    fn new() -> Self {
        Self {
            client: Arc::new(MockKubeClient::new()),
            storage: Arc::new(InMemoryReleaseStorage::new()),
        }
    }

    fn add(&self, key: ResourceKey, object: serde_json::Value) {
        let object: DynamicObject = serde_json::from_value(object).unwrap();
        self.client.add_resource(key, object);
    }

    /// A Deployment with `available` of its 2 replicas available.
    fn deployment(&self, available: i64) {
        self.add(
            key("apps/v1", "Deployment", Some("web"), "web"),
            json!({
                "apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": "web", "namespace": "web", "generation": 1},
                "spec": {"replicas": 2},
                "status": {"observedGeneration": 1, "updatedReplicas": 2, "replicas": 2, "availableReplicas": available}
            }),
        );
    }

    async fn save(&self, state: ReleaseState) {
        self.storage.save_release(&state).await.unwrap();
    }

    fn reader(&self, server: Option<&str>) -> DirectReader {
        DirectReader::new(
            DirectLocation {
                cluster: "dev".to_owned(),
                context: Some("kind".to_owned()),
                server: server.map(str::to_owned),
            },
            ObservedPublication {
                url: REPOSITORY.to_owned(),
                branch: "deploy".to_owned(),
                path_prefix: "dev".to_owned(),
            },
            None,
            Duration::from_secs(10),
            ClusterAccess::Fixed(ClusterHandle {
                client: self.client.clone(),
                storage: self.storage.clone(),
            }),
        )
    }
}

fn read(reader: &DirectReader, concluding_absence: bool) -> nyl_units::kubernetes_publication::reconciler::Reading {
    let deployments = [deployment()];
    let anchors = anchors();
    reader
        .read(&ReadRequest {
            deployments: &deployments,
            anchors: &anchors,
            concluding_absence,
        })
        .unwrap()
}

/// Catches the running revision being read from anything but the commit the
/// latest `Deployed` revision recorded, and health from anything but the
/// workloads' rollout: the Deployment is half available, so it is Progressing.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_deployed_revision_runs_its_recorded_commit_with_the_rollout_health() {
    let world = World::new();
    world
        .save(revision(
            1,
            ReleaseStatus::Deployed,
            Some(publication("dev", Some("abc"))),
        ))
        .await;
    world.deployment(1);
    let reading = tokio::task::spawn_blocking(move || read(&world.reader(None), false))
        .await
        .unwrap();
    let observed = &reading.deployments["web/web"];
    assert_eq!(observed.running, Running::Exact("abc".to_owned()));
    assert_eq!(observed.health, Health::Progressing);
    assert!(observed
        .reported
        .as_deref()
        .unwrap()
        .contains("1 of 2 updated replicas are available"));
    assert_eq!(reading.proof, Proof::Anchors);
    assert_eq!(reading.anchors[DIRECT_ANCHOR], Visibility::Visible);
}

/// Catches a fully rolled out Deployment not being healthy.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_rolled_out_deployment_is_healthy() {
    let world = World::new();
    world
        .save(revision(
            1,
            ReleaseStatus::Deployed,
            Some(publication("dev", Some("abc"))),
        ))
        .await;
    world.deployment(2);
    let reading = tokio::task::spawn_blocking(move || read(&world.reader(None), false))
        .await
        .unwrap();
    assert_eq!(reading.deployments["web/web"].health, Health::Healthy);
}

/// Catches a partly failed apply standing for a revision: the failed latest
/// revision is a mix, and names the commit of the last complete one.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_failed_latest_revision_is_a_mix() {
    let world = World::new();
    world
        .save(revision(
            1,
            ReleaseStatus::Deployed,
            Some(publication("dev", Some("abc"))),
        ))
        .await;
    world
        .save(revision(
            2,
            ReleaseStatus::Failed,
            Some(publication("dev", Some("def"))),
        ))
        .await;
    let reading = tokio::task::spawn_blocking(move || read(&world.reader(None), false))
        .await
        .unwrap();
    let Running::Mixed { last, reason } = &reading.deployments["web/web"].running else {
        panic!("a failed revision is a mix");
    };
    assert_eq!(last.as_deref(), Some("abc"));
    assert!(reason.contains("revision 2 failed"), "{reason}");
}

/// Catches another publication's release state, or a manual apply's, being
/// read as this target's: neither is a revision of this publication, so the
/// release is unreadable and, without a verified server, shows no anchor.
#[tokio::test(flavor = "multi_thread")]
async fn test_state_of_another_publication_is_unreadable_and_no_anchor() {
    for other in [Some(publication("prod", Some("abc"))), None] {
        let world = World::new();
        world.save(revision(1, ReleaseStatus::Deployed, other)).await;
        let reading = tokio::task::spawn_blocking(move || read(&world.reader(None), false))
            .await
            .unwrap();
        assert!(matches!(reading.deployments["web/web"].running, Running::Unreadable(_)));
        assert!(matches!(reading.anchors[DIRECT_ANCHOR], Visibility::Hidden(_)));
    }
}

/// Catches a manual `nyl apply` of this publication's target counting as
/// running a publication it never made: it records the destination but no commit.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_manual_apply_of_the_target_records_no_commit_and_is_unreadable() {
    let world = World::new();
    world
        .save(revision(1, ReleaseStatus::Deployed, Some(publication("dev", None))))
        .await;
    let reading = tokio::task::spawn_blocking(move || read(&world.reader(None), false))
        .await
        .unwrap();
    assert!(matches!(reading.deployments["web/web"].running, Running::Unreadable(_)));
    assert_eq!(reading.anchors[DIRECT_ANCHOR], Visibility::Visible);
}

/// Catches an empty cluster being read as proof: with no state and no
/// verified server there is no anchor, and the release is absent, visible; a
/// server verified against the declared destination proves the cluster itself.
#[tokio::test(flavor = "multi_thread")]
async fn test_without_state_the_anchor_needs_a_verified_server() {
    let world = World::new();
    let reading = tokio::task::spawn_blocking({
        let reader = world.reader(None);
        move || read(&reader, false)
    })
    .await
    .unwrap();
    assert!(reading.deployments.is_empty());
    assert_eq!(reading.absent["web/web"], Visibility::Visible);
    assert!(matches!(reading.anchors[DIRECT_ANCHOR], Visibility::Hidden(_)));
    let verified = tokio::task::spawn_blocking(move || read(&world.reader(Some("https://127.0.0.1:6443")), false))
        .await
        .unwrap();
    assert_eq!(verified.anchors[DIRECT_ANCHOR], Visibility::Visible);
}

/// Catches a removal reading that ends before the workloads are gone, and one
/// that waits on the Namespace that holds the state it reads: present while
/// the Deployment remains, absent once only the state Namespace is left.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_removal_reading_waits_for_workloads_but_not_for_the_state_namespace() {
    let world = World::new();
    world
        .save(revision(
            1,
            ReleaseStatus::Deployed,
            Some(publication("dev", Some("abc"))),
        ))
        .await;
    world.deployment(2);
    world.add(
        key("v1", "Namespace", None, "web"),
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "web"}}),
    );
    let world = Arc::new(world);
    let present = tokio::task::spawn_blocking({
        let world = world.clone();
        move || read(&world.reader(None), true)
    })
    .await
    .unwrap();
    assert!(present.deployments.contains_key("web/web"));

    let gvk = GroupVersionKind::from_api_version_kind("apps/v1", "Deployment").unwrap();
    use nyl_render::kubernetes::KubeClient;
    world.client.delete_resource(&gvk, Some("web"), "web").await.unwrap();
    let gone = tokio::task::spawn_blocking({
        let world = world.clone();
        move || read(&world.reader(None), true)
    })
    .await
    .unwrap();
    assert!(gone.deployments.is_empty(), "only the state Namespace remains");
    assert_eq!(gone.absent["web/web"], Visibility::Visible);
    assert_eq!(
        gone.anchors[DIRECT_ANCHOR],
        Visibility::Visible,
        "the state is still there"
    );
}

async fn removal_reading(world: &Arc<World>) -> nyl_units::kubernetes_publication::reconciler::Reading {
    tokio::task::spawn_blocking({
        let world = world.clone();
        move || read(&world.reader(None), true)
    })
    .await
    .unwrap()
}

/// Catches a removal reading that waits on a Release another publication took
/// over: its latest revision is not this publication's, a teardown leaves it,
/// so it counts as gone however much of it is running.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_removal_reading_counts_a_release_taken_over_by_another_publication_as_gone() {
    let world = World::new();
    world
        .save(revision(
            1,
            ReleaseStatus::Deployed,
            Some(publication("dev", Some("abc"))),
        ))
        .await;
    world
        .save(revision(
            2,
            ReleaseStatus::Deployed,
            Some(publication("other", Some("def"))),
        ))
        .await;
    world.deployment(2);
    let reading = removal_reading(&Arc::new(world)).await;

    assert!(reading.deployments.is_empty());
    assert_eq!(reading.absent["web/web"], Visibility::Visible);
}

/// Catches a removal reading that waits on a Namespace object, which a
/// teardown's first phase defers whenever it holds the state of any Release it
/// purges (this one's, or a Release that left the tree): no Namespace can end
/// the wait, and the second phase deletes them.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_removal_reading_never_waits_on_a_namespace() {
    let world = World::new();
    let mut state = revision(1, ReleaseStatus::Deployed, Some(publication("dev", Some("abc"))));
    state.resource_keys = vec![key("v1", "Namespace", None, "shared")];
    world.save(state).await;
    world.add(
        key("v1", "Namespace", None, "shared"),
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "shared"}}),
    );
    let reading = removal_reading(&Arc::new(world)).await;

    assert!(reading.deployments.is_empty(), "only a Namespace remains");
    assert_eq!(reading.absent["web/web"], Visibility::Visible);
}

/// Catches a removal reading that waits on what a purge leaves: an object an
/// earlier revision tracked and the latest `Deployed` one dropped (as
/// `Prune=false` does) stays in the cluster by design, and a purge does not
/// touch it, so it must not keep the wait open.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_removal_reading_ignores_what_only_an_older_revision_tracked() {
    let world = World::new();
    let mut older = revision(1, ReleaseStatus::Superseded, Some(publication("dev", Some("abc"))));
    older.resource_keys.push(key("v1", "ConfigMap", Some("web"), "kept"));
    world.save(older).await;
    let mut latest = revision(2, ReleaseStatus::Deployed, Some(publication("dev", Some("abc"))));
    latest.resource_keys = vec![key("apps/v1", "Deployment", Some("web"), "web")];
    world.save(latest).await;
    world.add(
        key("v1", "ConfigMap", Some("web"), "kept"),
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "kept", "namespace": "web"}}),
    );
    let reading = removal_reading(&Arc::new(world)).await;

    assert!(reading.deployments.is_empty());
    assert_eq!(reading.absent["web/web"], Visibility::Visible);
}

/// Catches the teardown wait waiting on a Release the teardown leaves: a Release
/// another unit owns was applied for this publication, so only its owner tells it
/// from this unit's. With the unit known, the removal reading counts it as gone,
/// by the rule the teardown's own classification uses; an earlier incarnation of
/// this very unit is this unit's.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_removal_reading_counts_a_release_another_unit_owns_as_gone() {
    use nyl_render::gitops::reconcile::RenderIndexOwner;
    let own = RenderIndexOwner::unit("dev", "kubernetes", "uid-own");
    for (owner, present) in [
        (RenderIndexOwner::unit("dev", "other", "uid-other"), false),
        (RenderIndexOwner::unit("dev", "kubernetes", "uid-earlier"), true),
        (own.clone(), true),
    ] {
        let world = World::new();
        let mut state = revision(1, ReleaseStatus::Deployed, Some(publication("dev", Some("abc"))));
        state.meta.owner = Some(owner.clone());
        world.save(state).await;
        world.deployment(2);
        let world = Arc::new(world);
        let reading = tokio::task::spawn_blocking({
            let world = world.clone();
            let own = own.clone();
            move || read(&world.reader(None).with_owner(Some(own)), true)
        })
        .await
        .unwrap();

        assert_eq!(
            reading.deployments.contains_key("web/web"),
            present,
            "owned by {owner}: {reading:?}"
        );
    }
}
