//! Reading a direct target: the release state Nyl recorded in the cluster, and
//! the rollout status of the workloads it applied, mapped into the neutral
//! [`Reading`] the shared evaluation judges.
//!
//! Contract: [Reading a direct target](../../../../../design/promotion.md#reading-a-direct-target).
//! The deployments are the target's Releases; each is identified by its
//! release state, the `nyl.release.v1.<name>.<revision>` Secrets of its
//! namespace. Only a revision applied for this publication (repository,
//! branch, prefix, as the facts record them) can stand for the target's
//! Release: the state of another target, or of a manual apply that recorded
//! another destination or none, is `Unreadable` and never an anchor.

use std::collections::BTreeMap;
use std::time::Duration;

use nyl_render::gitops::observation::{DirectLocation, DirectRelease, ExpectedDeployment, ObservedPublication};
use nyl_render::kubernetes::lifecycle::remaining_after_purge;
use nyl_render::kubernetes::{ReleasePublication, ReleaseState, ReleaseStatus, ResourceKey};
use nyl_render::{KubeErrorKind, NylError};
use serde_json::Value;

use nyl_render::gitops::reconcile::RenderIndexOwner;

use super::health::{fold, is_workload, Found};
use super::{destination_of, not_removable, ClusterAccess, ClusterHandle};
use crate::kubernetes_publication::reconciler::{
    block_on, Health, Observed, Proof, ReadError, ReadErrorKind, ReadRequest, Reading, ReconcilerReader, Running,
    Visibility,
};

/// Reads a direct target's Releases from the cluster they were applied to.
pub struct DirectReader {
    location: DirectLocation,
    publication: ObservedPublication,
    kubeconfig: Option<String>,
    budget: Duration,
    access: ClusterAccess,
    /// The unit whose Releases these are, so a removal reading counts only what
    /// its teardown removes ([`super::not_removable`]); without it, every Release
    /// of the publication.
    owner: Option<RenderIndexOwner>,
}

impl DirectReader {
    /// Judge Releases by whether `owner`'s teardown removes them.
    #[must_use]
    pub fn with_owner(mut self, owner: Option<RenderIndexOwner>) -> Self {
        self.owner = owner;
        self
    }

    pub fn new(
        location: DirectLocation,
        publication: ObservedPublication,
        kubeconfig: Option<String>,
        budget: Duration,
        access: ClusterAccess,
    ) -> Self {
        Self {
            location,
            publication,
            kubeconfig,
            budget,
            access,
            owner: None,
        }
    }
}

/// A read error of the Kubernetes API, as the reader reports it.
pub(super) fn api_error(error: &NylError, what: &str) -> ReadError {
    let text = error.to_string();
    let kind = match error.kube_kind() {
        Some(KubeErrorKind::Forbidden) => ReadErrorKind::Forbidden,
        Some(KubeErrorKind::Unauthorized) => ReadErrorKind::Rejected,
        _ => ReadErrorKind::Unreachable,
    };
    let first = text.lines().next().unwrap_or_default();
    let message = match kind {
        ReadErrorKind::Forbidden => format!(
            "the context may not {what} (HTTP 403): grant its user `get` and `list` on Secrets of the Release namespaces \
             and `get` on the workloads the Releases apply. {first}"
        ),
        ReadErrorKind::Rejected => format!(
            "the Kubernetes API rejected the kubeconfig's credentials (HTTP 401): they are invalid or have expired; \
             log in again or refresh the admitted kubeconfig. {first}"
        ),
        _ => format!("cannot {what}: {first}"),
    };
    ReadError::new(kind, message)
}

fn locator(deployment: &ExpectedDeployment) -> Result<DirectRelease, ReadError> {
    serde_json::from_value(deployment.locator.clone()).map_err(|error| {
        ReadError::new(
            ReadErrorKind::Malformed,
            format!("deployment {} records no Release locator: {error}", deployment.id),
        )
    })
}

/// The recorded revisions of one Release, newest first.
async fn revisions(cluster: &ClusterHandle, release: &DirectRelease) -> Result<Vec<ReleaseState>, ReadError> {
    let what = format!("list the release state of {}/{}", release.namespace, release.name);
    let numbers = cluster
        .storage
        .list_revisions(&release.name, &release.namespace)
        .await
        .map_err(|error| api_error(&error, &what))?;
    let mut states = Vec::new();
    for number in numbers.into_iter().rev() {
        if let Some(state) = cluster
            .storage
            .get_release(&release.name, &release.namespace, number)
            .await
            .map_err(|error| api_error(&error, &what))?
        {
            states.push(state);
        }
    }
    Ok(states)
}

pub(super) fn applied_for(state: &ReleaseState, destination: &ReleasePublication) -> bool {
    state
        .meta
        .publication
        .as_ref()
        .is_some_and(|publication| publication.same_destination(destination))
}

/// The commit a revision applied, when it was applied for `destination`.
fn commit_of<'a>(state: &'a ReleaseState, destination: &ReleasePublication) -> Option<&'a str> {
    state
        .meta
        .publication
        .as_ref()
        .filter(|publication| publication.same_destination(destination))
        .and_then(|publication| publication.commit.as_deref())
}

/// What one Release's revisions say about what runs, for `destination`.
async fn observe_release(
    cluster: &ClusterHandle,
    states: &[ReleaseState],
    destination: &ReleasePublication,
) -> Result<Observed, ReadError> {
    let latest = &states[0];
    let last_commit = states
        .iter()
        .find(|state| state.status == ReleaseStatus::Deployed)
        .and_then(|state| commit_of(state, destination))
        .map(str::to_owned);
    let mut observed = Observed {
        running: Running::None,
        health: Health::Unknown,
        reported: None,
        deleting: false,
        terminal: false,
        source: None,
        synced_source: None,
    };
    if !applied_for(latest, destination) {
        observed.running = Running::Unreadable(format!(
            "its latest revision {} was applied outside this publication ({}); reconcile to apply the publication again",
            latest.revision,
            if latest.meta.publication.is_some() {
                "for another destination"
            } else {
                "without a target"
            }
        ));
        return Ok(observed);
    }
    match latest.status {
        ReleaseStatus::Failed | ReleaseStatus::Rendered => {
            observed.running = Running::Mixed {
                last: last_commit,
                reason: format!(
                    "revision {} {} after applying some of its resources, so the cluster runs a mix of revisions: \
                     reconcile again so a full apply completes",
                    latest.revision,
                    if latest.status == ReleaseStatus::Failed {
                        "failed"
                    } else {
                        "was interrupted"
                    }
                ),
            };
        }
        ReleaseStatus::Deployed | ReleaseStatus::Superseded => match commit_of(latest, destination) {
            Some(commit) => {
                observed.running = Running::Exact(commit.to_owned());
                let (health, reported, terminal) = health_of(cluster, latest).await?;
                observed.health = health;
                observed.terminal = terminal;
                observed.reported = reported;
            }
            None => {
                observed.running = Running::Unreadable(format!(
                    "its latest revision {} was applied by `nyl apply` and records no publication commit; reconcile to \
                     apply the publication again",
                    latest.revision
                ));
            }
        },
    }
    Ok(observed)
}

/// The rollout health of the workloads the revision's manifest declares.
async fn health_of(cluster: &ClusterHandle, state: &ReleaseState) -> Result<(Health, Option<String>, bool), ReadError> {
    let documents = nyl_render::yaml::parse_yaml_documents_k8s_compatible(&state.manifest).map_err(|error| {
        ReadError::new(
            ReadErrorKind::Malformed,
            format!(
                "the manifest of revision {} of {} cannot be read: {error}",
                state.revision, state.release_name
            ),
        )
    })?;
    let mut found = Vec::new();
    for document in &documents {
        let (Some(api_version), Some(kind)) = (
            document.get("apiVersion").and_then(Value::as_str),
            document.get("kind").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !is_workload(api_version, kind) {
            continue;
        }
        let key = ResourceKey::from_json_value(document).map_err(|error| api_error(&error, "read a workload"))?;
        let live = cluster
            .client
            .get_resource(&key.gvk, key.namespace.as_deref(), &key.name)
            .await
            .map_err(|error| api_error(&error, &format!("read {key}")))?;
        found.push((
            key.clone(),
            document,
            live.map(|object| serde_json::to_value(&object).unwrap_or(Value::Null)),
        ));
    }
    let items = found
        .iter()
        .map(|(key, document, live)| Found {
            label: format!(
                "{} {}",
                key.gvk.kind,
                key.namespace
                    .as_deref()
                    .map_or_else(|| key.name.clone(), |namespace| format!("{namespace}/{}", key.name))
            ),
            kind: key.gvk.kind.as_str(),
            manifest: document,
            live: live.clone(),
        })
        .collect::<Vec<_>>();
    Ok(fold(&items))
}

/// Every key any recorded revision tracks, for the removal reading.
fn tracked(states: &[ReleaseState]) -> Vec<ResourceKey> {
    // What a purge deletes: the keys of the latest Deployed revision and of the
    // Failed revisions after it (`collect_live_state`), never of older
    // revisions, so an object a later revision untracked, for instance with
    // `Prune=false`, does not keep a removal reading waiting. `states` run from
    // the newest revision down.
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::new();
    for state in states {
        keys.extend(
            state
                .resource_keys
                .iter()
                .filter(|key| seen.insert((*key).clone()))
                .cloned(),
        );
        if state.status == ReleaseStatus::Deployed {
            break;
        }
    }
    keys
}

const NO_ANCHOR: &str = "no release state of this target's Releases is visible, and the context's server is not one the \
    Cluster declares, so nothing shows it reaches the cluster Nyl applied to: the target was never applied, or the context is \
    another cluster, or phase 2 of a teardown already removed the state. Check the context (or declare the Cluster's \
    destination server), reconcile, or, when the state is known gone, end a teardown with `teardownWait: manual` and \
    `nyl teardown --confirm-removed`, or release the unit with `nyl state forget`";

impl DirectReader {
    async fn read_cluster(&self, request: &ReadRequest<'_>) -> Result<Reading, ReadError> {
        let cluster = self
            .access
            .open(&self.location, self.kubeconfig.as_deref(), self.budget)
            .await?;
        let repository = request
            .deployments
            .first()
            .map(|deployment| deployment.source.repository.clone())
            .unwrap_or_default();
        let destination = destination_of(repository, &self.publication);
        // A removal reading waits for no Namespace at all: the first phase of a
        // teardown defers every Namespace that holds the state of any Release it
        // purges, those of Releases that left the tree and of foreign ones
        // included, and the second phase deletes them once the state is gone.
        // No Namespace is gone before that, so none can end the wait.
        let excluded = |key: &ResourceKey| key.gvk.group.is_empty() && key.gvk.kind == "Namespace";
        let mut reading = Reading {
            deployments: BTreeMap::new(),
            anchors: BTreeMap::new(),
            absent: BTreeMap::new(),
            proof: Proof::Anchors,
            unprovable: None,
        };
        let mut with_state = false;
        for deployment in request.deployments {
            let release = locator(deployment)?;
            let states = revisions(&cluster, &release).await?;
            let own = states.iter().any(|state| applied_for(state, &destination));
            with_state |= own;
            if states.is_empty() {
                reading.absent.insert(deployment.id.clone(), Visibility::Visible);
                continue;
            }
            if request.concluding_absence {
                // A removal reading: present while anything the release applied
                // that a purge deletes remains, other than the Namespaces that
                // hold the state the wait itself reads. A Release whose latest
                // revision was applied for another publication, or by no
                // target, is not this publication's, and a teardown leaves it:
                // it is gone as far as this unit is concerned.
                if not_removable(&states[0], &destination, self.owner.as_ref(), true, None).is_some() {
                    reading.absent.insert(deployment.id.clone(), Visibility::Visible);
                    continue;
                }
                let keys = tracked(&states);
                let remaining = remaining_after_purge(cluster.client.as_ref(), &keys, &excluded)
                    .await
                    .map_err(|error| api_error(&error, "check what remains of a release"))?;
                if remaining.is_empty() {
                    reading.absent.insert(deployment.id.clone(), Visibility::Visible);
                    continue;
                }
            }
            reading.deployments.insert(
                deployment.id.clone(),
                observe_release(&cluster, &states, &destination).await?,
            );
        }
        let visible = self.location.server.is_some() || with_state;
        for anchor in request.anchors {
            reading.anchors.insert(
                anchor.id.clone(),
                if visible {
                    Visibility::Visible
                } else {
                    Visibility::Hidden(NO_ANCHOR.to_owned())
                },
            );
        }
        Ok(reading)
    }
}

impl ReconcilerReader for DirectReader {
    fn read(&self, request: &ReadRequest<'_>) -> Result<Reading, ReadError> {
        let budget = self.budget;
        block_on(async {
            tokio::time::timeout(budget, self.read_cluster(request))
                .await
                .unwrap_or_else(|_| {
                    Err(ReadError::new(
                        ReadErrorKind::Unreachable,
                        format!(
                            "reading Cluster {} through context {:?} took longer than {}s",
                            self.location.cluster,
                            self.location.context,
                            budget.as_secs()
                        ),
                    ))
                })
        })?
    }
}
