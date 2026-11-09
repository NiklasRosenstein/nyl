//! The direct reconciler: Nyl applies a target's Releases itself, and reads
//! what it recorded back.
//!
//! Contract: [Direct reconciler](../../../../../design/orchestration-core.md#direct-reconciler)
//! and [Reading a direct target](../../../../../design/promotion.md#reading-a-direct-target).
//!
//! - [`ClusterAccess`] opens the one explicit connection both halves use: the
//!   kubeconfig the unit admits as `KUBECONFIG` and the Cluster's declared
//!   `spec.live.context`, whose server must be the declared destination's.
//!   Nothing falls back to the current context, `~/.kube/config`, or
//!   in-cluster configuration.
//! - [`DirectReader`] implements [`ReconcilerReader`](super::reconciler::ReconcilerReader)
//!   over the release state the apply recorded and the rollout status of the
//!   workloads ([`health`]), so the shared evaluation judges a direct target
//!   exactly as it judges Argo CD.
//! - [`drift`] compares the cluster with what the unit last applied, for
//!   `nyl detect-drift`, from the release state the apply recorded.
//! - [`apply`] holds the driver's operations: preflight, publish, apply in
//!   waves, purge, and the two teardown phases, over the lifecycle code
//!   `nyl apply` shares.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use nyl_render::gitops::observation::{DirectLocation, ObservedPublication};
use nyl_render::gitops::reconcile::RenderIndexOwner;
use nyl_render::kubernetes::fence::{OwnerProbe, OwnerState};
use nyl_render::kubernetes::{
    KubeClient, KubeRsClient, KubernetesReleaseStorage, ReleasePublication, ReleaseState, ReleaseStorage,
};

use super::argocd::Connection;
use super::reconciler::{ReadError, ReadErrorKind, ReconcilerReader, KUBECONFIG_ENV};

pub mod apply;
pub(crate) mod drift;
pub mod health;
mod reader;

pub use reader::DirectReader;

/// Why this unit may not remove a Release whose latest revision is `latest`,
/// or `None` when it may. The one rule a teardown, the purge of Releases that
/// left the tree, and the removal reading share, so the first-phase wait never
/// waits on a Release the teardown leaves.
///
/// A Release is this unit's to remove when its latest revision was applied for
/// `destination` and either names this unit (or an earlier incarnation of it
/// that is gone) as owner, or names none and `adoptable` (an apply by hand of
/// this target took it over: a teardown removes it, the purge of a vanished
/// Release leaves it). `probe` tells whether an earlier incarnation is gone; a
/// reader without one takes it to be, and without `own` takes any owner to be.
pub(crate) fn not_removable(
    latest: &ReleaseState,
    destination: &ReleasePublication,
    own: Option<&RenderIndexOwner>,
    adoptable: bool,
    probe: Option<OwnerProbe<'_>>,
) -> Option<String> {
    if !latest
        .meta
        .publication
        .as_ref()
        .is_some_and(|publication| publication.same_destination(destination))
    {
        return Some("it was applied for another publication".to_owned());
    }
    let Some(recorded) = &latest.meta.owner else {
        return (!adoptable).then(|| "no unit owns it".to_owned());
    };
    let own = own?;
    if recorded == own {
        return None;
    }
    let same_unit = matches!(
        (recorded, own),
        (
            RenderIndexOwner::Unit { environment, unit, .. },
            RenderIndexOwner::Unit { environment: own_environment, unit: own_unit, .. },
        ) if environment == own_environment && unit == own_unit
    );
    let gone = match probe {
        Some(probe) => probe(recorded) == OwnerState::Gone,
        None => true,
    };
    (!(same_unit && gone)).then(|| format!("it is owned by {recorded}"))
}

/// The destination a target's Releases are applied for, without a commit: the
/// repository of its tree and the recorded branch and prefix. What the reader
/// and the drift check judge a revision's recorded publication by.
pub(crate) fn destination_of(repository: String, publication: &ObservedPublication) -> ReleasePublication {
    ReleasePublication {
        repository,
        branch: publication.branch.clone(),
        path_prefix: publication.path_prefix.clone(),
        commit: None,
    }
}

/// A connection to the target's Cluster: what applies resources and what
/// records releases there.
#[derive(Clone)]
pub struct ClusterHandle {
    pub client: Arc<dyn KubeClient>,
    pub storage: Arc<dyn ReleaseStorage>,
}

/// How a direct target's Cluster is reached.
#[derive(Clone, Default)]
pub enum ClusterAccess {
    /// Through the admitted kubeconfig and the declared context.
    #[default]
    Live,
    /// A fixed client and storage, for tests that run no cluster.
    Fixed(ClusterHandle),
}

impl std::fmt::Debug for ClusterAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Live => formatter.write_str("ClusterAccess::Live"),
            Self::Fixed(_) => formatter.write_str("ClusterAccess::Fixed"),
        }
    }
}

impl ClusterAccess {
    /// Open the connection. The client must be used on the runtime that
    /// opened it, so callers open and use it inside one `block_on`.
    pub async fn open(
        &self,
        location: &DirectLocation,
        kubeconfig: Option<&str>,
        budget: Duration,
    ) -> Result<ClusterHandle, ReadError> {
        match self {
            Self::Fixed(handle) => Ok(handle.clone()),
            Self::Live => {
                let connection = Connection::new(
                    &location.cluster,
                    location.context.as_deref(),
                    kubeconfig,
                    budget,
                    ": a direct target is applied and read through it",
                )?;
                let client = connection.client_for(location.server.as_deref()).await?;
                let kube = KubeRsClient::from_client(client.clone()).await.map_err(|error| {
                    ReadError::new(
                        ReadErrorKind::Unreachable,
                        format!(
                            "cannot discover the Kubernetes API through context {:?}: {error}",
                            connection.context()
                        ),
                    )
                })?;
                Ok(ClusterHandle {
                    client: Arc::new(kube),
                    storage: Arc::new(KubernetesReleaseStorage::new(client)),
                })
            }
        }
    }
}

/// The reader the recorded location asks for, built from the unit's admitted
/// environment, finishing each reading within `budget`.
pub fn reader_with(
    location: &DirectLocation,
    publication: &ObservedPublication,
    env: &BTreeMap<String, String>,
    budget: Duration,
    access: ClusterAccess,
    owner: Option<&RenderIndexOwner>,
) -> Result<Box<dyn ReconcilerReader>, ReadError> {
    Ok(Box::new(
        DirectReader::new(
            location.clone(),
            publication.clone(),
            env.get(KUBECONFIG_ENV).cloned(),
            budget,
            access,
        )
        .with_owner(owner.cloned()),
    ))
}
