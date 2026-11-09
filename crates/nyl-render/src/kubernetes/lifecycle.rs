//! The Kubernetes release lifecycle: applying a Release's rendered resources
//! to a cluster, recording the revision, pruning what it no longer holds, and
//! purging a release. `nyl apply`, `nyl release rollback`, `nyl release
//! delete`, and the direct reconciler of the publication driver share this
//! one implementation, so every entry point applies, prunes, and records the
//! same way.
//!
//! Contract: [Direct reconciler](../../../../design/orchestration-core.md#direct-reconciler),
//! applying. The functions take a connected [`KubeClient`] and a
//! [`ReleaseStorage`]; how a connection is made (ambient kubeconfig for the
//! command, the unit's admitted `KUBECONFIG` and the Cluster's declared
//! context for the driver) belongs to each entry point.
//!
//! - [`prepare_desired_manifests`] resolves namespaces and collapses
//!   duplicate objects, [`apply_sorted_manifests`] applies in Argo CD's kind
//!   order, and [`ReleaseRecorder`] reserves a revision before the cluster is
//!   touched, completes it, prunes by release state, and supersedes the
//!   previous revision. An apply that changed nothing keeps the latest revision
//!   instead ([`RevisionPolicy`]).
//! - [`purge_release`] deletes a release's resources by their `Delete` sync
//!   option, in reverse apply order with waits for termination.
//! - [`ApplyEvent`] reports progress as it happens; the caller renders it.

use chrono::Utc;
use kube::api::DynamicObject;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use super::namespace_resolution::{adjust_duplicate_keys_for_namespace_resolution, resolve_manifest_namespaces};
use super::{
    delete_in_prune_order, enforce_release_history_limit,
    fence::{admit, ReleaseClaim},
    field_manager::FieldOwnership,
    prune::{delete_decision, prune_decision, PruneDecision},
    run_concurrently, ApplyOutcome, DeleteOptions, GroupVersionKind, KubeClient, ObjectIdentity, PruneEvent,
    ReleaseState, ReleaseStatus, ReleaseStorage, ResourceKey, ResourceOrdering,
};
use crate::{NylError, Result};

/// Something that happened during an apply, reported as it happens.
pub enum ApplyEvent<'a> {
    /// One resource finished applying.
    Applied {
        key: &'a ResourceKey,
        result: &'a Result<ApplyOutcome>,
    },
    /// After a resource applied, a handoff released the fields `manager` owned
    /// of it, or failed to.
    OwnershipReleased {
        key: &'a ResourceKey,
        manager: &'a str,
        result: &'a Result<()>,
    },
    /// Pruning of `count` resources that are no longer desired begins.
    PruneStarted { count: usize },
    /// One pruned resource finished deleting.
    Pruned {
        key: &'a ResourceKey,
        result: &'a Result<()>,
    },
    /// `count` deleted resources are still terminating (finalizers); the resources
    /// they may depend on are deleted once they are gone.
    PruneWaiting { count: usize },
    /// Deleted resources still had not terminated when the wait timed out; pruning
    /// continues without them.
    PruneWaitTimedOut { keys: &'a [&'a ResourceKey] },
    /// Pruning finished.
    PruneFinished,
    /// A resource no longer desired is kept by its Argo CD `Prune` sync option,
    /// or, for a purge, by its `Delete` sync option (`option` names which).
    PruneKept {
        key: &'a ResourceKey,
        decision: PruneDecision,
        option: &'static str,
    },
}

/// Receiver of [`ApplyEvent`]s.
pub type ApplyEventSink<'s> = dyn FnMut(ApplyEvent<'_>) + Send + 's;

pub struct ApplyExecutionResult {
    pub outcomes: Vec<ApplyOutcome>,
    pub failed_count: usize,
    pub resource_keys: Vec<ResourceKey>,
    /// The manager whose fields a handoff released, if the apply handed off.
    pub released_manager: Option<String>,
    /// Applied resources whose released manager owned fields that are now released.
    pub released_count: usize,
    /// Applied resources whose released manager's fields could not be released.
    pub release_failed_count: usize,
}

impl ApplyExecutionResult {
    /// The command's error when anything failed to apply or to release.
    pub fn error(&self, command: &str) -> Option<NylError> {
        apply_error(command, self, 0)
    }
}

/// The error of a command whose apply, handoff release, or prune failed.
fn apply_error(command: &str, apply: &ApplyExecutionResult, prune_failed: usize) -> Option<NylError> {
    let mut failures = Vec::new();
    if apply.failed_count > 0 {
        failures.push(format!("{} resource(s) failed to apply", apply.failed_count));
    }
    if apply.release_failed_count > 0 {
        failures.push(format!(
            "{} resource(s) failed to release field ownership",
            apply.release_failed_count
        ));
    }
    if prune_failed > 0 {
        failures.push(format!("{prune_failed} failed to prune"));
    }
    (!failures.is_empty()).then(|| NylError::Other(format!("{command} completed with {}", failures.join(" and "))))
}

/// Whether each kind is namespaced, so keys can be compared by [`ObjectIdentity`].
///
/// Kinds the cluster does not know keep the namespace their manifest names.
#[derive(Default)]
pub struct ObjectScopes {
    /// `None` when the lookup failed, so it is not repeated.
    namespaced: HashMap<(String, String), Option<bool>>,
}

impl ObjectScopes {
    /// Look up the scope of every kind among `keys` not yet known.
    pub async fn load(&mut self, client: &dyn KubeClient, keys: &[ResourceKey]) {
        for key in keys {
            let group_kind = (key.gvk.group.clone(), key.gvk.kind.clone());
            if self.namespaced.contains_key(&group_kind) {
                continue;
            }
            let namespaced = match client.is_namespaced(&key.gvk).await {
                Ok(namespaced) => Some(namespaced),
                Err(err) => {
                    tracing::debug!(resource = %key, error = %err, "Kind scope unknown; keeping its namespace");
                    None
                }
            };
            self.namespaced.insert(group_kind, namespaced);
        }
    }

    pub fn identity(&self, key: &ResourceKey) -> ObjectIdentity {
        let namespaced = self
            .namespaced
            .get(&(key.gvk.group.clone(), key.gvk.kind.clone()))
            .copied()
            .flatten()
            .unwrap_or(true);
        key.object_identity(namespaced)
    }
}

fn manifest_keys(manifests: &[serde_json::Value]) -> Result<Vec<ResourceKey>> {
    manifests.iter().map(ResourceKey::from_json_value).collect()
}

/// Determine which previously-live resources are no longer present in `current_keys`
/// and should therefore be pruned from the cluster.
///
/// Keys are compared by [`ObjectIdentity`], so an object whose manifest moved to
/// another API version is not pruned.
pub fn keys_to_prune<'a>(
    live_keys: &'a HashSet<ResourceKey>,
    current_keys: &[ResourceKey],
    scopes: &ObjectScopes,
) -> Vec<&'a ResourceKey> {
    let current: HashSet<ObjectIdentity> = current_keys.iter().map(|key| scopes.identity(key)).collect();
    let mut to_prune: Vec<&ResourceKey> = live_keys
        .iter()
        .filter(|key| !current.contains(&scopes.identity(key)))
        .collect();
    // Two recorded versions of one removed object are one delete.
    let mut seen = HashSet::new();
    to_prune.retain(|key| seen.insert(scopes.identity(key)));
    to_prune
}

/// Determine the resources currently live on the cluster for a release, and which
/// revision a new deployment supersedes.
///
/// The live state is the most recent `Deployed` revision's resources, plus any
/// resources partially applied by `Failed` revisions after it (a Failed revision
/// never prunes, so its applied resources remain on the cluster). Returns the
/// revision to mark `Superseded` (the most recent Deployed one, if any) together
/// with the union of live resource keys to reconcile against.
///
/// `revisions` are the stored revision numbers, as listed before recording the new one.
pub async fn collect_live_state(
    storage: &dyn ReleaseStorage,
    release_name: &str,
    release_namespace: &str,
    revisions: &[u32],
    next_revision: u32,
) -> Result<(Option<ReleaseState>, HashSet<ResourceKey>)> {
    let mut prev_revisions: Vec<u32> = revisions.iter().copied().filter(|r| *r < next_revision).collect();
    prev_revisions.sort_unstable();

    let mut live_keys: HashSet<ResourceKey> = HashSet::new();
    // Walk newest to oldest, unioning keys until (and including) the most recent
    // Deployed revision — that revision captures the full live state.
    for &rev in prev_revisions.iter().rev() {
        if let Some(prev) = storage.get_release(release_name, release_namespace, rev).await? {
            live_keys.extend(prev.resource_keys.iter().cloned());
            if prev.status == ReleaseStatus::Deployed {
                return Ok((Some(prev), live_keys));
            }
        }
    }

    Ok((None, live_keys))
}

/// Mark a revision superseded. Best-effort, like the rest of superseding: the new
/// revision is already recorded.
async fn mark_superseded(storage: &dyn ReleaseStorage, previous: &ReleaseState) {
    if let Err(err) = storage
        .update_release_status(
            &previous.release_name,
            &previous.release_namespace,
            previous.revision,
            ReleaseStatus::Superseded,
            None,
        )
        .await
    {
        tracing::warn!(
            "Failed to mark release {} revision {} superseded: {}",
            previous.release_name,
            previous.revision,
            err
        );
    }
}

/// Delete resources no longer desired, at most `concurrency` at a time, and return
/// the keys that failed to delete, through the shared [`delete_in_prune_order`]:
/// reverse apply order, waiting for each kind to terminate.
async fn prune_resources<'k>(
    client: &dyn KubeClient,
    keys: Vec<&'k ResourceKey>,
    concurrency: usize,
    on_event: &mut ApplyEventSink<'_>,
) -> Vec<&'k ResourceKey> {
    delete_in_prune_order(client, keys, &DeleteOptions::default(), concurrency, &mut |event| {
        on_event(match event {
            PruneEvent::Started { count } => ApplyEvent::PruneStarted { count },
            PruneEvent::Deleted { key, result } => ApplyEvent::Pruned { key, result },
            PruneEvent::Waiting { count } => ApplyEvent::PruneWaiting { count },
            PruneEvent::WaitTimedOut { keys } => ApplyEvent::PruneWaitTimedOut { keys },
            PruneEvent::Finished => ApplyEvent::PruneFinished,
        });
    })
    .await
}

/// Resolve missing namespaces and collapse documents describing one object, the
/// shared preparation of rendered manifests for `apply` and `diff`, so a diff shows
/// what apply applies.
///
/// Namespace resolution and API versions can make distinct documents describe one
/// object; the last is kept, so the applied and recorded manifests agree.
pub async fn prepare_desired_manifests(
    client: &dyn KubeClient,
    mut manifests: Vec<serde_json::Value>,
    duplicates: &mut HashMap<ResourceKey, usize>,
    destination_namespace: Option<&str>,
) -> Result<(Vec<serde_json::Value>, ObjectScopes)> {
    resolve_manifest_namespaces(client, &mut manifests, destination_namespace).await?;
    *duplicates = adjust_duplicate_keys_for_namespace_resolution(client, duplicates, destination_namespace).await?;
    collapse_rendered_duplicates(client, manifests, duplicates).await
}

/// Load the scope of every rendered kind and collapse documents describing one
/// object ([`collapse_duplicate_objects`]). Returns the collapsed manifests and the
/// loaded scopes, which later identity comparisons extend.
pub async fn collapse_rendered_duplicates(
    client: &dyn KubeClient,
    manifests: Vec<serde_json::Value>,
    duplicates: &mut HashMap<ResourceKey, usize>,
) -> Result<(Vec<serde_json::Value>, ObjectScopes)> {
    let mut scopes = ObjectScopes::default();
    scopes.load(client, &manifest_keys(&manifests)?).await;
    let manifests = collapse_duplicate_objects(manifests, duplicates, &scopes)?;
    Ok((manifests, scopes))
}

/// Collapse manifests that describe the same Kubernetes object, keeping the last.
///
/// Objects are compared by [`ObjectIdentity`]: two versions of one object, or a
/// cluster-scoped object written with and without a namespace, are one object, and
/// applying both concurrently would race. The surviving document takes the position
/// of the first occurrence, as in render-time deduplication, and `duplicates` is
/// updated with the total occurrence count under the surviving document's key.
pub fn collapse_duplicate_objects(
    manifests: Vec<serde_json::Value>,
    duplicates: &mut HashMap<ResourceKey, usize>,
    scopes: &ObjectScopes,
) -> Result<Vec<serde_json::Value>> {
    let mut position: HashMap<ObjectIdentity, usize> = HashMap::new();
    let mut collapsed: Vec<(serde_json::Value, Vec<ResourceKey>)> = Vec::new();
    for manifest in manifests {
        let key = ResourceKey::from_json_value(&manifest)?;
        let identity = scopes.identity(&key);
        if let Some(&index) = position.get(&identity) {
            collapsed[index].0 = manifest;
            collapsed[index].1.push(key);
        } else {
            position.insert(identity, collapsed.len());
            collapsed.push((manifest, vec![key]));
        }
    }

    Ok(collapsed
        .into_iter()
        .map(|(manifest, keys)| {
            if keys.len() > 1 {
                let total = keys.iter().map(|key| duplicates.remove(key).unwrap_or(1)).sum();
                let kept = keys.last().expect("a collapsed object has a key").clone();
                duplicates.insert(kept, total);
            }
            manifest
        })
        .collect())
}

/// Build the manifest to store for an `--append-release` revision.
///
/// Combines the current (newly-rendered) manifests with the previous revision's
/// manifest documents for objects that are not part of the current set, so the
/// stored manifest matches the merged `resource_keys`. This keeps `release rollback`
/// faithful: rolling back to an appended revision re-applies the complete desired
/// state rather than only the newly-rendered resources. Objects are compared by
/// [`ObjectIdentity`], so a current document replaces the previous one even when
/// its API version changed.
fn merge_append_manifest(
    desired_manifests: &[serde_json::Value],
    previous_docs: Vec<serde_json::Value>,
    scopes: &ObjectScopes,
) -> Result<String> {
    // Dedup against the objects in the current manifest (all rendered docs), not
    // just successfully-applied ones — otherwise a current doc that failed to apply
    // would be carried over again from the previous manifest, producing a duplicate
    // document for the same object.
    let current: HashSet<ObjectIdentity> = manifest_keys(desired_manifests)?
        .iter()
        .map(|key| scopes.identity(key))
        .collect();

    let mut merged_docs: Vec<serde_json::Value> = desired_manifests.to_vec();
    for doc in previous_docs {
        if !current.contains(&scopes.identity(&ResourceKey::from_json_value(&doc)?)) {
            merged_docs.push(doc);
        }
    }
    ResourceOrdering::sort_by_priority(&mut merged_docs)?;
    manifests_to_yaml(&merged_docs)
}

/// Records one apply as a release revision.
///
/// [`Self::reserve`] records the revision before the cluster is touched, so a
/// concurrent apply of the same release fails before changing anything; it is
/// recorded `Rendered` with the keys the apply intends to write, which later applies
/// treat like a partial revision if this one is interrupted. [`Self::complete`] then
/// records the outcome, prunes resources no longer desired, and supersedes the
/// previous revision. `apply` and `release rollback` share this path.
pub struct ReleaseRecorder<'s> {
    storage: &'s dyn ReleaseStorage,
    release: ReleaseState,
    /// Revisions stored before this one.
    revisions: Vec<u32>,
    /// The previous revision merged into this one (`--append-release`).
    appended_to: Option<ReleaseState>,
}

impl<'s> ReleaseRecorder<'s> {
    /// Record the next revision of a release as `Rendered`.
    ///
    /// With `append_release`, the previous revision must be `Deployed`; its objects
    /// that the current manifests do not contain are carried into this revision.
    #[allow(clippy::too_many_arguments)] // The reservation's inputs are independent facts of one call; a struct would only rename them.
    pub async fn reserve(
        storage: &'s dyn ReleaseStorage,
        release_name: &str,
        release_namespace: &str,
        desired_manifests: &[serde_json::Value],
        append_release: bool,
        client: &dyn KubeClient,
        scopes: &mut ObjectScopes,
        claim: &ReleaseClaim<'_>,
    ) -> Result<Self> {
        // The fence comes first: a refused claim changes nothing. The revision
        // create below is the compare-and-swap that makes the reservation the
        // claim, so a concurrent claimant fails there instead.
        let admitted = admit(storage, release_namespace, release_name, claim).await?;
        if let Some(warning) = &admitted.warning {
            tracing::warn!("{warning}");
        }
        // The revisions the fence was decided on, not a second listing: a revision
        // another claimant created since makes this create fail.
        let revisions = admitted.revisions;
        let next_revision = revisions.iter().max().map_or(1, |r| r + 1);
        let desired_keys = manifest_keys(desired_manifests)?;

        let mut release = ReleaseState {
            release_name: release_name.to_string(),
            release_namespace: release_namespace.to_string(),
            revision: next_revision,
            resource_keys: desired_keys.clone(),
            manifest: manifests_to_yaml(desired_manifests)?,
            status: ReleaseStatus::Rendered,
            rendered_at: Utc::now(),
            applied_at: None,
            error: None,
            meta: claim.meta(),
        };

        let mut appended_to = None;
        if append_release && next_revision > 1 {
            let previous_revision = next_revision - 1;
            if let Some(previous) = storage
                .get_release(release_name, release_namespace, previous_revision)
                .await?
            {
                // Only Deployed releases have complete resource sets safe to merge from.
                if previous.status != ReleaseStatus::Deployed {
                    return Err(NylError::Config(format!(
                        "Cannot use --append-release when previous release (revision {}) is in {:?} state. \
                         The previous release must be in Deployed state to safely merge resources.",
                        previous.revision, previous.status
                    )));
                }
                let previous_docs = crate::yaml::parse_yaml_documents_k8s_compatible(&previous.manifest)
                    .map_err(|e| NylError::Config(format!("Failed to parse previous release manifest: {}", e)))?;
                scopes.load(client, &previous.resource_keys).await;
                scopes.load(client, &manifest_keys(&previous_docs)?).await;
                // Merge the stored manifest too, so the recorded manifest matches
                // the merged resource set and `release rollback` re-applies all of it.
                release.manifest = merge_append_manifest(desired_manifests, previous_docs, scopes)?;
                release.resource_keys = append_keys(&previous.resource_keys, &desired_keys, scopes);
                tracing::info!(
                    "Append-release mode: carried over {} resources from revision {} ({} total)",
                    release.resource_keys.len() - desired_keys.len(),
                    previous.revision,
                    release.resource_keys.len()
                );
                appended_to = Some(previous);
            } else {
                tracing::warn!(
                    "Append-release mode: no previous release found (revision {}), treating as initial apply",
                    previous_revision
                );
            }
        }

        storage.save_release(&release).await?;
        Ok(Self {
            storage,
            release,
            revisions,
            appended_to,
        })
    }

    /// Apply `manifests` and record the outcome.
    ///
    /// If anything fails or `interrupt` completes (Ctrl-C for the command, the
    /// unit's deadline for the driver) before the revision is completed, the
    /// reserved revision is marked `Failed`, so it never stays `Rendered` and
    /// later applies treat its keys as partially applied. While that is being
    /// recorded, `escalate` completing abandons the recording: the command
    /// passes a second Ctrl-C there, and the driver a future that never completes.
    pub async fn run(
        self,
        client: &dyn KubeClient,
        manifests: &[serde_json::Value],
        scopes: &mut ObjectScopes,
        settings: ApplySettings<'_>,
        interruption: Interruption<impl Future<Output = ()>, impl Future<Output = ()>>,
        on_event: &mut ApplyEventSink<'_>,
    ) -> Result<ReleaseRun> {
        let storage = self.storage;
        let reserved = self.release.clone();
        let completed = AtomicBool::new(false);
        // Keys applied so far, recorded if the apply does not complete.
        let mut applied_keys: Vec<ResourceKey> = Vec::new();
        let mut track = |event: ApplyEvent<'_>| {
            if let ApplyEvent::Applied { key, result: Ok(_) } = &event {
                applied_keys.push((*key).clone());
            }
            on_event(event);
        };
        let work = async {
            let apply_started = Instant::now();
            let (applicable, left): (Vec<_>, Vec<_>) = match settings.unapplied {
                Some(unapplied) => {
                    let mut applicable = Vec::new();
                    let mut left = Vec::new();
                    for manifest in manifests {
                        let key = ResourceKey::from_json_value(manifest)?;
                        if unapplied.contains(&key) {
                            left.push(key);
                        } else {
                            applicable.push(manifest.clone());
                        }
                    }
                    (applicable, left)
                }
                None => (manifests.to_vec(), Vec::new()),
            };
            let mut apply = apply_sorted_manifests(
                client,
                &applicable,
                settings.ownership,
                settings.concurrency,
                &mut track,
            )
            .await?;
            apply.resource_keys.extend(left);
            let apply_elapsed = apply_started.elapsed();

            let release_started = Instant::now();
            let Completed {
                release,
                prune_failed_count,
                unchanged,
            } = self
                .complete(client, &apply, scopes, settings, &mut track, &completed)
                .await?;
            Ok(ReleaseRun {
                apply,
                release,
                unchanged,
                prune_failed_count,
                apply_elapsed,
                release_elapsed: release_started.elapsed(),
            })
        };
        let result = tokio::select! {
            result = work => result,
            () = interruption.interrupt => Err(NylError::Interrupted("Interrupted".to_string())),
        };

        if let Err(err) = &result {
            if !completed.load(AtomicOrdering::SeqCst) {
                // Record only what this apply wrote, so a later prune never deletes
                // objects it did not create.
                let failed = Self::failed_revision(reserved, applied_keys, err);
                tokio::select! {
                    () = Self::record_failed(storage, &failed) => {}
                    () = interruption.escalate => {}
                }
            }
        }
        result
    }

    /// The reserved revision as recorded when its apply did not complete.
    fn failed_revision(mut release: ReleaseState, applied_keys: Vec<ResourceKey>, err: &NylError) -> ReleaseState {
        release.status = ReleaseStatus::Failed;
        release.error = Some(format!("apply did not complete: {err}"));
        release.resource_keys = applied_keys;
        release
    }

    async fn record_failed(storage: &dyn ReleaseStorage, failed: &ReleaseState) {
        if let Err(err) = storage.complete_release(failed).await {
            // An apply that completed unchanged has released its reservation already,
            // so there is no revision left to mark.
            if matches!(
                storage
                    .get_release(&failed.release_name, &failed.release_namespace, failed.revision)
                    .await,
                Ok(None)
            ) {
                tracing::info!(
                    "Release {} revision {} was released before it could be marked failed",
                    failed.release_name,
                    failed.revision
                );
                return;
            }
            tracing::warn!(
                "Failed to mark release {} revision {} failed: {}",
                failed.release_name,
                failed.revision,
                err
            );
        }
    }

    /// Record the apply's outcome, prune, and supersede the previous revision.
    /// Returns the recorded revision and how many resources failed to prune.
    ///
    /// Keys that fail to prune stay in the new revision, so the next apply retries
    /// deleting them. Once the revision deployed, revisions beyond the history limit
    /// are deleted. `completed` is set once the revision is recorded.
    ///
    /// Under [`RevisionPolicy::KeepUnchanged`], a deployed apply that would record
    /// what the latest revision records releases its reservation instead and
    /// settles on the latest revision, which the history limit still applies to.
    async fn complete(
        self,
        client: &dyn KubeClient,
        apply_result: &ApplyExecutionResult,
        scopes: &mut ObjectScopes,
        settings: ApplySettings<'_>,
        on_event: &mut ApplyEventSink<'_>,
        completed: &AtomicBool,
    ) -> Result<Completed> {
        let Self {
            storage,
            mut release,
            revisions,
            appended_to,
        } = self;

        release.resource_keys = match &appended_to {
            Some(previous) => append_keys(&previous.resource_keys, &apply_result.resource_keys, scopes),
            None => apply_result.resource_keys.clone(),
        };
        if apply_result.failed_count == 0 {
            release.status = ReleaseStatus::Deployed;
            release.applied_at = Some(Utc::now());
        } else {
            release.status = ReleaseStatus::Failed;
            release.error = Some(format!("{} resource(s) failed to apply", apply_result.failed_count));
        }

        // Only a fully applied revision supersedes and prunes; a Failed one leaves
        // the previous live state in place.
        let mut superseded = None;
        let mut prune_failed_count = 0;
        if release.status == ReleaseStatus::Deployed {
            if let Some(previous) = appended_to {
                // Append mode does not prune; it only supersedes the revision it merged.
                superseded = Some(previous);
            } else if release.revision > 1 {
                // Reconcile against the resources currently live on the cluster, not
                // just the numerically previous revision: the most recent Deployed
                // revision plus anything partially applied after it.
                let (previous, live_keys) = collect_live_state(
                    storage,
                    &release.release_name,
                    &release.release_namespace,
                    &revisions,
                    release.revision,
                )
                .await?;
                superseded = previous;

                let live_keys_list: Vec<ResourceKey> = live_keys.iter().cloned().collect();
                scopes.load(client, &live_keys_list).await;
                let mut to_prune: Vec<&ResourceKey> = Vec::new();
                for key in keys_to_prune(&live_keys, &release.resource_keys, scopes) {
                    let identity = scopes.identity(key);
                    match settings.protected {
                        // Another Release applied it already: it is theirs now.
                        Some(protected) if protected.handed_over.contains(&identity) => {}
                        // Another Release will render it but has not applied: the object stays in
                        // the cluster, and stays tracked here until that Release applies it.
                        Some(protected) if protected.pending.contains(&identity) => {
                            release.resource_keys.push(key.clone());
                        }
                        _ => to_prune.push(key),
                    }
                }
                let (to_prune, awaiting, unreadable) =
                    filter_by_prune_option(client, to_prune, settings.confirm_prune, on_event).await;
                // Resources awaiting confirmation stay tracked, so a confirmed apply prunes them.
                release.resource_keys.extend(awaiting.into_iter().cloned());
                // A resource whose live state cannot be read is never deleted on a guess:
                // it stays tracked and counts as a failed prune, so the next apply retries.
                prune_failed_count = unreadable.len();
                release.resource_keys.extend(unreadable.into_iter().cloned());
                let failed = prune_resources(client, to_prune, settings.concurrency, on_event).await;
                prune_failed_count += failed.len();
                release.resource_keys.extend(failed.into_iter().cloned());
            }
        }

        if let RevisionPolicy::KeepUnchanged { publication_stands } = settings.revisions {
            // A pruned object leaves the key set, so it always differs from the latest
            // revision's; a failed prune or release keeps the keys but is not settled.
            let settled = release.status == ReleaseStatus::Deployed
                && prune_failed_count == 0
                && apply_result.release_failed_count == 0;
            if settled {
                if let Some(latest) = keep_latest(storage, &release, &revisions, publication_stands).await {
                    completed.store(true, AtomicOrdering::SeqCst);
                    // The limit may have been lowered since the latest revision deployed.
                    prune_release_history(storage, &latest, settings.history_limit).await;
                    return Ok(Completed {
                        release: latest,
                        prune_failed_count,
                        unchanged: true,
                    });
                }
            }
        }

        storage.complete_release(&release).await?;
        completed.store(true, AtomicOrdering::SeqCst);
        if let Some(previous) = &superseded {
            mark_superseded(storage, previous).await;
        }
        prune_release_history(storage, &release, settings.history_limit).await;
        Ok(Completed {
            release,
            prune_failed_count,
            unchanged: false,
        })
    }
}

/// What [`ReleaseRecorder::complete`] settled.
struct Completed {
    release: ReleaseState,
    prune_failed_count: usize,
    unchanged: bool,
}

/// Whether the publication commit a revision recorded, which differs from the
/// one an apply would record, still stands for it.
pub type PublicationStands<'a> = &'a (dyn Fn(&str) -> bool + Sync);

/// Whether an apply records a revision when it changed nothing.
///
/// Contract: [Direct reconciler](../../../../design/orchestration-core.md#direct-reconciler),
/// applying. The apply itself always runs, because server-side apply is
/// idempotent and repairs drift; the policy decides only whether the
/// revision it would record is written.
#[derive(Clone, Copy)]
pub enum RevisionPolicy<'a> {
    /// Every apply records a revision, as `helm upgrade` does; `release
    /// rollback` records the rollback this way.
    Always,
    /// An apply that changed nothing keeps the latest revision as the current
    /// one. It changed nothing when the latest revision is `Deployed`, records the
    /// same manifest, resource keys, owner, and publication destination, and the
    /// apply pruned nothing and failed nothing.
    ///
    /// The publication commit changes whenever any other part of the target's
    /// tree does, so the recorded one is accepted when it is the same, or when
    /// `publication_stands` says it still stands for the new one. Without it,
    /// only the same commit (none, for `nyl apply`) is accepted.
    KeepUnchanged {
        publication_stands: Option<PublicationStands<'a>>,
    },
}

impl std::fmt::Debug for RevisionPolicy<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Always => formatter.write_str("Always"),
            Self::KeepUnchanged { .. } => formatter.write_str("KeepUnchanged"),
        }
    }
}

/// Release the reservation of `release` and return the latest revision, when
/// `release` records nothing that differs from it ([`unchanged_latest`]).
///
/// The reservation was only the fence of the apply; releasing it makes the
/// latest revision the current one again. If it cannot be released, the
/// revision is recorded like any other (`None`).
async fn keep_latest(
    storage: &dyn ReleaseStorage,
    release: &ReleaseState,
    revisions: &[u32],
    publication_stands: Option<PublicationStands<'_>>,
) -> Option<ReleaseState> {
    let latest = unchanged_latest(storage, release, revisions, publication_stands).await?;
    if let Err(error) = storage
        .delete_release(&release.release_name, &release.release_namespace, release.revision)
        .await
    {
        // A failed request may still have deleted it: read what is stored. The
        // reservation gone, or a revision another claimant recorded since, means
        // ours is released; only our own reservation still there is recorded.
        let reserved = match storage
            .get_release(&release.release_name, &release.release_namespace, release.revision)
            .await
        {
            Ok(stored) => stored.is_some_and(|stored| is_reservation(&stored, release)),
            Err(_) => true,
        };
        if reserved {
            tracing::warn!(
                "Failed to release the reservation of release {} revision {}; recording it instead: {}",
                release.release_name,
                release.revision,
                error
            );
            return None;
        }
    }
    tracing::info!(
        "Release {}/{} is unchanged at revision {}; no new revision recorded",
        latest.release_namespace,
        latest.release_name,
        latest.revision
    );
    Some(latest)
}

/// Whether `stored` is the reservation `release` made: still `Rendered`, from the
/// same moment. The storage API has no precondition on the object's identity, so
/// this tells our reservation from one another claimant created in its place.
fn is_reservation(stored: &ReleaseState, release: &ReleaseState) -> bool {
    stored.status == ReleaseStatus::Rendered && stored.rendered_at == release.rendered_at
}

/// The latest revision stored before `release`, when `release` records nothing
/// that differs from it. A revision that cannot be read counts as changed.
async fn unchanged_latest(
    storage: &dyn ReleaseStorage,
    release: &ReleaseState,
    revisions: &[u32],
    publication_stands: Option<PublicationStands<'_>>,
) -> Option<ReleaseState> {
    let latest_revision = *revisions.iter().max()?;
    let latest = match storage
        .get_release(&release.release_name, &release.release_namespace, latest_revision)
        .await
    {
        Ok(latest) => latest?,
        Err(error) => {
            tracing::warn!(
                "Cannot read release {} revision {} to compare it: {}",
                release.release_name,
                latest_revision,
                error
            );
            return None;
        }
    };
    records_same(&latest, release, publication_stands).then_some(latest)
}

/// Whether `new` would record what `latest` records. Timestamps are not
/// compared: they say when, not what.
fn records_same(latest: &ReleaseState, new: &ReleaseState, publication_stands: Option<PublicationStands<'_>>) -> bool {
    if latest.status != ReleaseStatus::Deployed
        || latest.manifest != new.manifest
        || latest.meta.owner != new.meta.owner
        || latest.resource_keys.len() != new.resource_keys.len()
    {
        return false;
    }
    let recorded: HashSet<&ResourceKey> = latest.resource_keys.iter().collect();
    if !new.resource_keys.iter().all(|key| recorded.contains(key)) {
        return false;
    }
    match (&latest.meta.publication, &new.meta.publication) {
        (None, None) => true,
        (Some(old), Some(new)) if old.same_destination(new) => {
            old.commit == new.commit
                || old
                    .commit
                    .as_deref()
                    .zip(publication_stands)
                    .is_some_and(|(commit, stands)| stands(commit))
        }
        _ => false,
    }
}

/// How a release is applied and recorded.
#[derive(Clone, Copy, Debug)]
pub struct ApplySettings<'a> {
    /// Maximum number of resources applied or pruned at the same time.
    pub concurrency: usize,
    /// Revisions to keep after a successful apply; 0 keeps every revision.
    pub history_limit: u32,
    /// Whether an apply that changed nothing records a revision.
    pub revisions: RevisionPolicy<'a>,
    /// `--confirm-prune`: prune resources whose sync option is `Prune=confirm`.
    pub confirm_prune: bool,
    /// The field manager resources are applied as, and the one a handoff releases.
    pub ownership: &'a FieldOwnership,
    /// Objects some other release renders, which this release's prune must not
    /// delete: an object that moved between releases is not removed from the one
    /// that took it over.
    pub protected: Option<Protected<'a>>,
    /// Objects of the manifests that this apply leaves alone and still records
    /// as applied, so the revision stays the one it would be had they been
    /// applied: the driver names an object whose absence is expected and whose
    /// document did not change, such as a Job a TTL removed after it finished.
    pub unapplied: Option<&'a HashSet<ResourceKey>>,
}

/// The objects other releases of one run render, by whether they applied yet.
#[derive(Clone, Copy, Debug)]
pub struct Protected<'a> {
    /// Rendered by a release that applied already: dropped from this release's
    /// tracking without being deleted, since the other release tracks it.
    pub handed_over: &'a HashSet<ObjectIdentity>,
    /// Rendered by a release that has not applied (and may not, when a later
    /// wave aborts or the deadline passes): left in the cluster and kept
    /// tracked here, so no release forgets an object nothing else tracks.
    pub pending: &'a HashSet<ObjectIdentity>,
}

/// Split the resources to prune by their live Argo CD `Prune` sync option
/// ([`prune_decision`]): returns those to delete and those awaiting
/// confirmation, and reports every resource kept. A resource whose live state
/// cannot be read fails closed: its option is unknown, so it is returned as
/// unreadable, never to be deleted.
async fn filter_by_prune_option<'k>(
    client: &dyn KubeClient,
    keys: Vec<&'k ResourceKey>,
    confirmed: bool,
    on_event: &mut ApplyEventSink<'_>,
) -> (Vec<&'k ResourceKey>, Vec<&'k ResourceKey>, Vec<&'k ResourceKey>) {
    let filtered = filter_by_sync_option(client, keys, confirmed, ("Prune", prune_decision), on_event).await;
    (filtered.delete, filtered.awaiting, filtered.unreadable)
}

/// What a sync option decided for a set of resources.
struct SyncFiltered<'k> {
    /// To delete.
    delete: Vec<&'k ResourceKey>,
    /// Kept until confirmed.
    awaiting: Vec<&'k ResourceKey>,
    /// Kept for good (`=false`).
    retained: Vec<&'k ResourceKey>,
    /// Whose live state could not be read, so the option is unknown.
    unreadable: Vec<&'k ResourceKey>,
}

/// A sync option's name and how it decides from a live object's annotations.
type SyncOption = (
    &'static str,
    fn(Option<&std::collections::BTreeMap<String, String>>, bool) -> PruneDecision,
);

/// [`filter_by_prune_option`] for either sync option: `option` names it and
/// decides from the live annotations.
async fn filter_by_sync_option<'k>(
    client: &dyn KubeClient,
    keys: Vec<&'k ResourceKey>,
    confirmed: bool,
    (option, decide): SyncOption,
    on_event: &mut ApplyEventSink<'_>,
) -> SyncFiltered<'k> {
    let mut prune = Vec::new();
    let mut awaiting = Vec::new();
    let mut retained = Vec::new();
    let mut unreadable = Vec::new();
    for key in keys {
        let live = match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
            Ok(live) => live,
            // A kind the cluster no longer serves has no live object to read; deleting
            // it is the executor's to settle (a removed kind's objects are gone).
            Err(error) if error.is_api_resource_not_found_error() => None,
            Err(error) => {
                tracing::warn!(resource = %key, %error, "Cannot read a resource's sync option; it is not deleted");
                unreadable.push(key);
                continue;
            }
        };
        let decision = decide(
            live.as_ref().and_then(|object| object.metadata.annotations.as_ref()),
            confirmed,
        );
        match decision {
            PruneDecision::Prune => prune.push(key),
            PruneDecision::Retain => {
                on_event(ApplyEvent::PruneKept { key, decision, option });
                retained.push(key);
            }
            PruneDecision::AwaitConfirmation => {
                on_event(ApplyEvent::PruneKept { key, decision, option });
                awaiting.push(key);
            }
        }
    }
    SyncFiltered {
        delete: prune,
        awaiting,
        retained,
        unreadable,
    }
}

/// Delete release revisions beyond `history_limit` once `release` deployed.
///
/// A failed revision leaves older revisions in place: they may still describe live
/// resources and hold the last known-good state to roll back to. Deletion errors
/// only warn, because the resources are already applied and recorded and the next
/// successful apply retries.
async fn prune_release_history(storage: &dyn ReleaseStorage, release: &ReleaseState, history_limit: u32) {
    if history_limit == 0 {
        return;
    }
    if release.status != ReleaseStatus::Deployed {
        tracing::warn!(
            "Release history limit of {} not applied: revision {} did not deploy successfully",
            history_limit,
            release.revision
        );
        return;
    }
    match enforce_release_history_limit(
        storage,
        &release.release_name,
        &release.release_namespace,
        release.revision,
        history_limit,
    )
    .await
    {
        Ok(deleted) if !deleted.is_empty() => {
            tracing::info!(
                "Deleted {} release revision(s) beyond the history limit of {}: {:?}",
                deleted.len(),
                history_limit,
                deleted
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                "Failed to delete release revisions beyond the history limit of {}: {}",
                history_limit,
                e
            );
        }
    }
}

/// What applying and recording a release produced.
pub struct ReleaseRun {
    pub apply: ApplyExecutionResult,
    /// The current revision: the one this apply recorded, or, when
    /// `unchanged`, the latest one it kept.
    pub release: ReleaseState,
    /// The apply changed nothing ([`RevisionPolicy::KeepUnchanged`]), so it
    /// recorded no revision and `release` is the latest one.
    pub unchanged: bool,
    /// Resources that failed to prune; they stay recorded for the next apply.
    pub prune_failed_count: usize,
    /// How long applying took.
    pub apply_elapsed: Duration,
    /// How long recording, pruning, and superseding took.
    pub release_elapsed: Duration,
}

/// How an interrupted apply is stopped.
pub struct Interruption<I, E> {
    /// Completes when the apply should stop (Ctrl-C, a deadline).
    pub interrupt: I,
    /// Completes when the recording of the failed revision should be abandoned.
    pub escalate: E,
}

impl Interruption<std::future::Pending<()>, std::future::Pending<()>> {
    /// An apply that is never interrupted from outside.
    pub fn never() -> Self {
        Self {
            interrupt: std::future::pending(),
            escalate: std::future::pending(),
        }
    }
}

impl ReleaseRun {
    /// The command's error when anything failed to apply or prune.
    pub fn error(&self, command: &str) -> Option<NylError> {
        apply_error(command, &self.apply, self.prune_failed_count)
    }
}

/// The keys of an appended revision: the previous revision's objects that `current`
/// does not contain, then `current` (current wins on duplicates).
fn append_keys(previous: &[ResourceKey], current: &[ResourceKey], scopes: &ObjectScopes) -> Vec<ResourceKey> {
    let current_identities: HashSet<ObjectIdentity> = current.iter().map(|key| scopes.identity(key)).collect();
    previous
        .iter()
        .filter(|key| !current_identities.contains(&scopes.identity(key)))
        .chain(current)
        .cloned()
        .collect()
}

/// Apply manifests in [`ResourceOrdering::apply_batches`], at most `concurrency` at a time.
///
/// As in Argo CD, the resources of one kind form a batch that is applied
/// concurrently, and every batch finishes before the next starts, so Namespaces
/// precede the resources in them and CustomResourceDefinitions and APIServices
/// precede the kinds they register. Before the first batch containing a kind of an
/// API group registered earlier in this apply, discovery is refreshed until the
/// registered kinds are served. A failed resource does not stop the others, matching
/// serial apply. Callers collapse duplicate objects first
/// ([`collapse_duplicate_objects`]); concurrent applies of one object would race.
///
/// `on_event` receives each result as it completes so progress is visible before the
/// whole apply ends; the returned outcomes and keys keep manifest order regardless of
/// completion order.
pub async fn apply_sorted_manifests(
    client: &dyn KubeClient,
    manifests: &[serde_json::Value],
    ownership: &FieldOwnership,
    concurrency: usize,
    on_event: &mut ApplyEventSink<'_>,
) -> Result<ApplyExecutionResult> {
    let keys = manifest_keys(manifests)?;
    let batches = ResourceOrdering::apply_batches(keys.iter().map(|key| &key.gvk));
    tracing::info!(
        "Applying {} resources in {} batches (up to {} at a time)",
        manifests.len(),
        batches.len(),
        concurrency
    );

    let mut results: Vec<Option<Result<ApplyOutcome>>> = (0..manifests.len()).map(|_| None).collect();
    let mut released_count = 0;
    let mut release_failed_count = 0;
    // API groups registered by CRDs or APIServices applied so far and not yet
    // confirmed in discovery.
    let mut registered_groups: HashSet<String> = HashSet::new();

    for batch in batches {
        // Kinds of a newly registered group are not resolvable until discovery is
        // refreshed (otherwise apply fails with ApiResourceNotFound), and a fresh CRD
        // or APIService may not be served the instant its apply returns.
        if keys[batch.clone()]
            .iter()
            .any(|key| registered_groups.contains(&key.gvk.group))
        {
            let mut needed: Vec<GroupVersionKind> = keys[batch.start..]
                .iter()
                .filter(|key| registered_groups.contains(&key.gvk.group))
                .map(|key| key.gvk.clone())
                .collect();
            needed.sort_by(|a, b| (&a.group, &a.version, &a.kind).cmp(&(&b.group, &b.version, &b.kind)));
            needed.dedup();
            client.refresh_discovery_until_available(&needed).await?;
            registered_groups.clear();
        }

        run_concurrently(
            batch,
            concurrency,
            |index| apply_manifest(client, &manifests[index], ownership),
            |index, (result, release)| {
                on_event(ApplyEvent::Applied {
                    key: &keys[index],
                    result: &result,
                });
                if let (Some(release), Some(manager)) = (release, ownership.released_manager()) {
                    match release {
                        Ok(false) => {}
                        Ok(true) => {
                            released_count += 1;
                            on_event(ApplyEvent::OwnershipReleased {
                                key: &keys[index],
                                manager,
                                result: &Ok(()),
                            });
                        }
                        Err(err) => {
                            release_failed_count += 1;
                            on_event(ApplyEvent::OwnershipReleased {
                                key: &keys[index],
                                manager,
                                result: &Err(err),
                            });
                        }
                    }
                }
                if result.is_ok() {
                    if let Some(group) = registered_api_group(&keys[index], &manifests[index]) {
                        registered_groups.insert(group);
                    }
                }
                results[index] = Some(result);
            },
        )
        .await;
    }

    let mut outcomes = Vec::new();
    let mut failed_count = 0;
    let mut resource_keys = Vec::new();
    for (key, result) in keys.into_iter().zip(results) {
        match result.expect("every manifest belongs to exactly one apply batch") {
            Ok(outcome) => {
                outcomes.push(outcome);
                resource_keys.push(key);
            }
            Err(_) => failed_count += 1,
        }
    }

    Ok(ApplyExecutionResult {
        outcomes,
        failed_count,
        resource_keys,
        released_manager: ownership.released_manager().map(str::to_string),
        released_count,
        release_failed_count,
    })
}

/// The API group whose kinds this manifest registers: a CustomResourceDefinition's or
/// an APIService's `spec.group`.
fn registered_api_group(key: &ResourceKey, manifest: &serde_json::Value) -> Option<String> {
    if !ResourceOrdering::registers_api_group(&key.gvk) {
        return None;
    }
    manifest["spec"]["group"]
        .as_str()
        .filter(|group| !group.is_empty())
        .map(str::to_string)
}

/// Convert manifests to YAML string
pub fn manifests_to_yaml(manifests: &[serde_json::Value]) -> Result<String> {
    let mut yaml_parts = Vec::new();

    for manifest in manifests {
        let yaml = crate::yaml::serialize_yaml_value(manifest).map_err(NylError::YamlEmit)?;
        yaml_parts.push(yaml);
    }

    Ok(yaml_parts.join("---\n"))
}

/// Apply a single manifest as the ownership's manager, then release what the
/// manager a handoff replaces owns. The release result is `None` unless the
/// apply succeeded and the ownership hands off.
async fn apply_manifest(
    client: &dyn KubeClient,
    manifest: &serde_json::Value,
    ownership: &FieldOwnership,
) -> (Result<ApplyOutcome>, Option<Result<bool>>) {
    let resource: DynamicObject = match serde_json::from_value(manifest.clone()) {
        Ok(resource) => resource,
        Err(err) => return (Err(err.into()), None),
    };
    let outcome = client.apply_resource(&resource, ownership.manager(), false).await;
    let release = match (&outcome, ownership.released_manager()) {
        (Ok(_), Some(released)) => Some(client.release_field_manager(&resource, released).await),
        _ => None,
    };
    (outcome, release)
}

/// Ensure a namespace exists, creating it if necessary
pub async fn ensure_namespace_exists(
    client: &dyn KubeClient,
    namespace: &str,
    ownership: &FieldOwnership,
) -> Result<()> {
    use serde_json::json;

    // Build namespace GVK
    let ns_gvk = GroupVersionKind::from_api_version_kind("v1", "Namespace")?;

    // Check if namespace exists
    if let Some(_ns) = client.get_resource(&ns_gvk, None, namespace).await? {
        // Namespace exists, nothing to do
        Ok(())
    } else {
        // Namespace doesn't exist, create it
        tracing::warn!("Namespace '{}' does not exist. Creating it for the release.", namespace);

        // Create bare namespace resource
        let ns_resource: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": namespace
            }
        }))?;

        // Apply the namespace
        client.apply_resource(&ns_resource, ownership.manager(), false).await?;

        tracing::info!("Created namespace '{}'", namespace);

        Ok(())
    }
}

/// What a purge did.
#[derive(Debug, Default)]
pub struct Purged {
    /// Resources deleted.
    pub deleted: Vec<ResourceKey>,
    /// Resources a `Delete` sync option kept (`Delete=false`, or `Delete=confirm`
    /// without confirmation).
    pub kept: Vec<ResourceKey>,
    /// Resources that failed to delete.
    pub failed: Vec<ResourceKey>,
}

/// Delete `keys` from the cluster: the resources of a release being removed.
///
/// Each resource's live `Delete` sync option decides ([`delete_decision`]):
/// `Delete=false` keeps it and `Delete=confirm` keeps it unless `confirmed`.
/// A key `excluded` is never deleted, whatever its option: a caller passes the
/// objects another release still tracks and the Namespaces it must keep.
/// Resources are deleted in reverse apply order, with waits for termination,
/// by the executor that prunes ([`prune_resources`]). Duplicate keys that name
/// one object are one delete.
pub async fn purge_resources(
    client: &dyn KubeClient,
    keys: &[ResourceKey],
    excluded: &(dyn Fn(&ResourceKey) -> bool + Sync),
    concurrency: usize,
    confirmed: bool,
    on_event: &mut ApplyEventSink<'_>,
) -> Purged {
    let mut scopes = ObjectScopes::default();
    scopes.load(client, keys).await;
    let mut seen = HashSet::new();
    let candidates: Vec<&ResourceKey> = keys
        .iter()
        .filter(|key| !excluded(key))
        .filter(|key| seen.insert(scopes.identity(key)))
        .collect();
    let SyncFiltered {
        delete: deleting,
        awaiting,
        retained,
        unreadable,
    } = filter_by_sync_option(client, candidates, confirmed, ("Delete", delete_decision), on_event).await;
    let mut purged = Purged {
        kept: awaiting.into_iter().chain(retained).cloned().collect(),
        // Never deleted on a guess: a resource whose option cannot be read is failed.
        failed: unreadable.into_iter().cloned().collect(),
        ..Purged::default()
    };
    let failed = prune_resources(client, deleting.clone(), concurrency, on_event).await;
    let failed_identities: HashSet<ObjectIdentity> = failed.iter().map(|key| scopes.identity(key)).collect();
    for key in deleting {
        if failed_identities.contains(&scopes.identity(key)) {
            purged.failed.push(key.clone());
        } else {
            purged.deleted.push(key.clone());
        }
    }
    purged
}

/// The resources among `keys` that a purge would still delete: they exist, and
/// their live `Delete` option deletes them (a resource kept by its option is not
/// remaining, because the purge leaves it on purpose), except `excluded` ones.
pub async fn remaining_after_purge(
    client: &dyn KubeClient,
    keys: &[ResourceKey],
    excluded: &(dyn Fn(&ResourceKey) -> bool + Sync),
) -> Result<Vec<ResourceKey>> {
    let mut remaining = Vec::new();
    for key in keys.iter().filter(|key| !excluded(key)) {
        let live = match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
            Ok(live) => live,
            // The whole kind is gone, so are its objects.
            Err(error) if error.is_api_resource_not_found_error() => match client.is_kind_removed(&key.gvk).await {
                Ok(true) => None,
                _ => return Err(error),
            },
            Err(error) => return Err(error),
        };
        if let Some(live) = live {
            if delete_decision(live.metadata.annotations.as_ref(), false) == PruneDecision::Prune {
                remaining.push(key.clone());
            }
        }
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn test_manifests_to_yaml() {
        let manifests = vec![
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "test1"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": "test2"}
            }),
        ];

        let yaml = manifests_to_yaml(&manifests).unwrap();
        assert!(yaml.contains("test1"));
        assert!(yaml.contains("test2"));
        assert!(yaml.contains("---"));
    }

    fn resource_key(name: &str) -> ResourceKey {
        ResourceKey {
            gvk: crate::kubernetes::GroupVersionKind {
                group: String::new(),
                version: "v1".to_string(),
                kind: "ConfigMap".to_string(),
            },
            namespace: Some("default".to_string()),
            name: name.to_string(),
        }
    }

    #[test]
    fn test_keys_to_prune_removes_orphans() {
        let live: HashSet<ResourceKey> = [resource_key("a"), resource_key("b"), resource_key("c")]
            .into_iter()
            .collect();
        let current = [resource_key("a"), resource_key("c")];

        let to_prune = keys_to_prune(&live, &current, &ObjectScopes::default());
        assert_eq!(to_prune.len(), 1);
        assert_eq!(to_prune[0].name, "b");
    }

    #[test]
    fn test_merge_append_manifest_carries_over_previous_resources() {
        // Previous revision stored ConfigMaps A and B.
        let previous = manifests_to_yaml(&[
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a", "namespace": "default"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "b", "namespace": "default"}}),
        ])
        .unwrap();

        // Current append-release apply renders only B (an update).
        let current =
            vec![json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "b", "namespace": "default"}})];

        let previous = crate::yaml::parse_yaml_documents_k8s_compatible(&previous).unwrap();
        let merged = merge_append_manifest(&current, previous, &ObjectScopes::default()).unwrap();
        let docs = crate::yaml::parse_yaml_documents_k8s_compatible(&merged).unwrap();

        // The stored manifest carries over A (not in the current set) plus B, with no
        // duplicate B (B is present in both current and previous).
        assert_eq!(docs.len(), 2);
        let names: Vec<&str> = docs
            .iter()
            .filter_map(|d| d.get("metadata").and_then(|m| m.get("name")).and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    /// A cluster with a fixed per-request latency that records apply order and
    /// how many applies were in flight at once.
    struct LatencyClient {
        inner: crate::kubernetes::MockKubeClient,
        latency: Duration,
        failing: &'static str,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        events: std::sync::Mutex<Vec<String>>,
        /// Discovery refreshes fail.
        refresh_fails: bool,
        /// Deleted resources that stay terminating for this many reads, by name.
        terminating_reads: std::sync::Mutex<HashMap<String, usize>>,
        /// A kind whose recorded version the cluster no longer serves.
        removed_kind: &'static str,
        /// Whether `removed_kind` is gone in every version.
        kind_gone: bool,
        /// Reading this resource fails with an API error.
        unreadable: &'static str,
    }

    impl LatencyClient {
        fn new(failing: &'static str) -> Self {
            Self {
                inner: crate::kubernetes::MockKubeClient::new(),
                latency: Duration::from_millis(250),
                failing,
                in_flight: AtomicUsize::new(0),
                max_in_flight: AtomicUsize::new(0),
                events: std::sync::Mutex::default(),
                refresh_fails: false,
                terminating_reads: std::sync::Mutex::default(),
                removed_kind: "",
                kind_gone: true,
                unreadable: "",
            }
        }

        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn position(&self, event: &str) -> usize {
            self.events()
                .iter()
                .position(|e| e == event)
                .unwrap_or_else(|| panic!("missing event {event}"))
        }
    }

    #[async_trait::async_trait]
    impl KubeClient for LatencyClient {
        async fn get_resource(
            &self,
            gvk: &GroupVersionKind,
            namespace: Option<&str>,
            name: &str,
        ) -> Result<Option<DynamicObject>> {
            if name == self.unreadable {
                return Err(NylError::Kubernetes("the API server is unavailable".to_string()));
            }
            if let Some(reads) = self.terminating_reads.lock().unwrap().get_mut(name) {
                if *reads > 0 {
                    *reads -= 1;
                    let terminating = json!({"apiVersion": "v1", "kind": gvk.kind, "metadata": {"name": name}});
                    return Ok(Some(serde_json::from_value(terminating).unwrap()));
                }
                self.events.lock().unwrap().push(format!("gone {name}"));
            }
            self.inner.get_resource(gvk, namespace, name).await
        }

        async fn apply_resource(&self, resource: &DynamicObject, manager: &str, dry_run: bool) -> Result<ApplyOutcome> {
            use kube::ResourceExt;
            use std::sync::atomic::Ordering;
            let name = resource.name_any();
            self.events.lock().unwrap().push(format!("start {name}"));
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.latency).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.events.lock().unwrap().push(format!("end {name}"));
            if name == self.failing {
                return Err(NylError::Other("rejected by admission".to_string()));
            }
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
            self.events.lock().unwrap().push(format!("delete-start {name}"));
            tokio::time::sleep(self.latency).await;
            self.events.lock().unwrap().push(format!("delete-end {name}"));
            if name == self.failing {
                return Err(NylError::Other("failed calling webhook".to_string()));
            }
            if gvk.kind == self.removed_kind {
                return Err(NylError::ApiResourceNotFound(gvk.kind.clone()));
            }
            self.inner.delete_resource_with(gvk, namespace, name, options).await
        }

        async fn get_normalized_resource(&self, resource: &DynamicObject, manager: &str) -> Result<DynamicObject> {
            self.inner.get_normalized_resource(resource, manager).await
        }

        async fn release_field_manager(&self, resource: &DynamicObject, manager: &str) -> Result<bool> {
            self.inner.release_field_manager(resource, manager).await
        }

        async fn is_kind_removed(&self, gvk: &GroupVersionKind) -> Result<bool> {
            Ok(gvk.kind == self.removed_kind && self.kind_gone)
        }

        async fn refresh_discovery_until_available(&self, required: &[GroupVersionKind]) -> Result<()> {
            tokio::time::sleep(self.latency).await;
            let kinds: Vec<&str> = required.iter().map(|gvk| gvk.kind.as_str()).collect();
            self.events.lock().unwrap().push(format!("refresh {}", kinds.join(",")));
            if self.refresh_fails {
                return Err(NylError::Kubernetes("connection reset".to_string()));
            }
            Ok(())
        }
    }

    fn config_maps(count: usize) -> Vec<serde_json::Value> {
        (0..count)
            .map(|i| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": format!("cm-{i:02}"), "namespace": "app"}}))
            .collect()
    }

    /// Independent resources share round trips up to the concurrency bound, and each
    /// outcome is reported when it completes rather than after the whole apply.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_overlaps_round_trips_and_streams_outcomes() {
        let client = LatencyClient::new("");
        let mut manifests = vec![json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "app"}})];
        manifests.extend(config_maps(16));

        let started = tokio::time::Instant::now();
        let mut reported_at = Vec::new();
        let result = apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 8, &mut |_| {
            reported_at.push(started.elapsed());
        })
        .await
        .unwrap();
        let elapsed = started.elapsed();

        // One round trip for the Namespace batch, two for 16 ConfigMaps at 8 at a
        // time, instead of 17 sequential round trips.
        assert_eq!(elapsed, client.latency * 3);
        assert_eq!(client.max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 8);
        assert_eq!(reported_at.len(), 17);
        assert_eq!(reported_at[0], client.latency);
        assert!(reported_at.iter().filter(|at| **at < elapsed).count() >= 9);

        let expected_keys: Vec<ResourceKey> = manifests
            .iter()
            .map(|manifest| ResourceKey::from_json_value(manifest).unwrap())
            .collect();
        assert_eq!(result.resource_keys, expected_keys);
        assert_eq!(result.failed_count, 0);
    }

    /// A handoff applies as the receiving manager and releases the fields an
    /// earlier `nyl apply` left, so a later Argo CD sync can remove fields
    /// dropped from the source; objects Nyl never applied are left alone.
    #[tokio::test]
    async fn test_apply_sorted_manifests_handoff_releases_nyl_ownership() {
        let client = crate::kubernetes::MockKubeClient::new();
        let manifests = config_maps(2);
        let bootstrapped = ResourceKey::from_json_value(&manifests[0]).unwrap();
        let fresh = ResourceKey::from_json_value(&manifests[1]).unwrap();
        let mut live = manifests[0].clone();
        live["metadata"]["managedFields"] = json!([{"manager": "nyl", "operation": "Apply"}]);
        client.add_resource(bootstrapped.clone(), serde_json::from_value(live).unwrap());
        let ownership =
            FieldOwnership::from_handoff(Some(crate::kubernetes::field_manager::ARGOCD_FIELD_MANAGER)).unwrap();

        let mut released = Vec::new();
        let result = apply_sorted_manifests(&client, &manifests, &ownership, 8, &mut |event| {
            if let ApplyEvent::OwnershipReleased {
                key,
                manager,
                result: Ok(()),
            } = event
            {
                released.push((key.clone(), manager.to_string()));
            }
        })
        .await
        .unwrap();

        assert_eq!(released, vec![(bootstrapped.clone(), "nyl".to_string())]);
        assert_eq!((result.released_count, result.release_failed_count), (1, 0));
        assert_eq!(
            client.applied_managers(&bootstrapped),
            vec![crate::kubernetes::field_manager::ARGOCD_FIELD_MANAGER]
        );
        assert_eq!(
            client.applied_managers(&fresh),
            vec![crate::kubernetes::field_manager::ARGOCD_FIELD_MANAGER]
        );
        assert!(result.error("Apply").is_none());
    }

    /// Without a handoff Nyl applies as `nyl` and releases nothing.
    #[tokio::test]
    async fn test_apply_sorted_manifests_applies_as_nyl_by_default() {
        let client = crate::kubernetes::MockKubeClient::new();
        let manifests = config_maps(1);
        let key = ResourceKey::from_json_value(&manifests[0]).unwrap();

        let result = apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 8, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(client.applied_managers(&key), vec!["nyl"]);
        assert!(result.released_manager.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_concurrency_one_is_serial() {
        let client = LatencyClient::new("");
        let manifests = config_maps(4);

        let started = tokio::time::Instant::now();
        apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 1, &mut |_| {})
            .await
            .unwrap();

        assert_eq!(started.elapsed(), client.latency * 4);
        assert_eq!(client.max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Namespaces finish before namespaced resources start; a CRD is applied before
    /// its custom resources, and discovery is refreshed for the CRD's group only once
    /// a batch needs it, so built-in batches in between do not wait for it.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_keeps_barriers_and_defers_discovery_refresh() {
        let client = LatencyClient::new("");
        let mut manifests = vec![
            json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "widget", "namespace": "app"}}),
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "deploy", "namespace": "app"}}),
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "widgets.example.com"}, "spec": {"group": "example.com"}}),
            json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "app"}}),
        ];
        ResourceOrdering::sort_by_priority(&mut manifests).unwrap();

        apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 8, &mut |_| {})
            .await
            .unwrap();

        assert!(client.position("end app") < client.position("start widgets.example.com"));
        assert!(client.position("end widgets.example.com") < client.position("start deploy"));
        assert!(client.position("end deploy") < client.position("refresh Widget"));
        assert!(client.position("refresh Widget") < client.position("start widget"));
    }

    /// An APIService is applied, and discovery refreshed for its group, before the
    /// objects it serves.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_registers_api_service_before_served_objects() {
        let client = LatencyClient::new("");
        let mut manifests = vec![
            json!({"apiVersion": "metrics.example.com/v1", "kind": "Metric", "metadata": {"name": "metric", "namespace": "app"}}),
            json!({"apiVersion": "apiregistration.k8s.io/v1", "kind": "APIService", "metadata": {"name": "v1.metrics.example.com"}, "spec": {"group": "metrics.example.com", "version": "v1"}}),
        ];
        ResourceOrdering::sort_by_priority(&mut manifests).unwrap();

        apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 8, &mut |_| {})
            .await
            .unwrap();

        assert!(client.position("end v1.metrics.example.com") < client.position("refresh Metric"));
        assert!(client.position("refresh Metric") < client.position("start metric"));
    }

    /// A failed resource is reported and counted without stopping the rest, and it is
    /// not recorded as part of the release.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_continues_after_partial_failure() {
        let client = LatencyClient::new("cm-01");
        let manifests = config_maps(3);

        let mut failures = Vec::new();
        let result = apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 8, &mut |event| {
            if let ApplyEvent::Applied { key, result: Err(_) } = event {
                failures.push(key.name.clone());
            }
        })
        .await
        .unwrap();

        assert_eq!(failures, vec!["cm-01"]);
        assert_eq!(result.failed_count, 1);
        let names: Vec<&str> = result.resource_keys.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(names, vec!["cm-00", "cm-02"]);
        assert_eq!(client.inner.get_all_resources().len(), 2);
    }

    /// Pruning deletes each kind concurrently and dependents before what they depend
    /// on: the webhook before its backend, the custom resource before its CRD.
    #[tokio::test(start_paused = true)]
    async fn test_prune_resources_deletes_in_reverse_apply_order() {
        let client = LatencyClient::new("");
        let key = |api_version: &str, kind: &str, namespace: Option<&str>, name: &str| ResourceKey {
            gvk: GroupVersionKind::from_api_version_kind(api_version, kind).unwrap(),
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
        };
        let keys = [
            key(
                "admissionregistration.k8s.io/v1",
                "ValidatingWebhookConfiguration",
                None,
                "hook",
            ),
            key("apps/v1", "Deployment", Some("app"), "backend"),
            key("apiregistration.k8s.io/v1", "APIService", None, "api"),
            key("v1", "Service", Some("app"), "svc"),
            key(
                "apiextensions.k8s.io/v1",
                "CustomResourceDefinition",
                None,
                "widgets.example.com",
            ),
            key("example.com/v1", "Widget", Some("app"), "widget"),
        ];

        let started = tokio::time::Instant::now();
        let mut pruned = Vec::new();
        let failed = prune_resources(&client, keys.iter().collect(), 8, &mut |event| {
            if let ApplyEvent::Pruned { key, result: Ok(()) } = event {
                pruned.push(key.name.clone());
            }
        })
        .await;

        // One round trip per kind: six kinds, one resource each.
        assert_eq!(started.elapsed(), client.latency * 6);
        assert_eq!(
            pruned,
            vec!["hook", "widget", "api", "backend", "svc", "widgets.example.com"]
        );
        assert!(failed.is_empty());
    }

    /// Documents for one object collapse to the last, even across API versions and
    /// after namespace resolution, and the duplicate count covers every occurrence.
    #[test]
    fn test_collapse_duplicate_objects_keeps_last_across_api_versions() {
        let manifests = vec![
            json!({"apiVersion": "autoscaling/v1", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web", "namespace": "app"}, "spec": {"v": "first"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "other", "namespace": "app"}}),
            json!({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "web", "namespace": "app"}, "spec": {"v": "last"}}),
        ];
        let first_key = ResourceKey::from_json_value(&manifests[0]).unwrap();
        let last_key = ResourceKey::from_json_value(&manifests[2]).unwrap();
        // Render already collapsed two identical autoscaling/v1 documents.
        let mut duplicates = HashMap::from([(first_key.clone(), 2)]);

        let collapsed = collapse_duplicate_objects(manifests, &mut duplicates, &ObjectScopes::default()).unwrap();

        assert_eq!(collapsed.len(), 2);
        assert_eq!(collapsed[0]["apiVersion"], "autoscaling/v2");
        assert_eq!(collapsed[0]["spec"]["v"], "last");
        assert_eq!(collapsed[1]["metadata"]["name"], "other");
        assert_eq!(duplicates, HashMap::from([(last_key, 3)]));
    }

    #[test]
    fn test_keys_to_prune_nothing_when_superset() {
        let live: HashSet<ResourceKey> = [resource_key("a"), resource_key("b")].into_iter().collect();
        let current = [resource_key("a"), resource_key("b"), resource_key("c")];

        assert!(keys_to_prune(&live, &current, &ObjectScopes::default()).is_empty());
    }

    #[test]
    fn test_large_release_manifest_can_be_read_and_appended() {
        let manifests: Vec<_> = (0..1_025)
            .map(|index| {
                json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": format!("config-{index}"), "namespace": "default"},
                })
            })
            .collect();
        let stored = manifests_to_yaml(&manifests).unwrap();
        assert_eq!(
            crate::yaml::parse_yaml_documents_k8s_compatible(&stored).unwrap(),
            manifests
        );

        let mut updated = manifests[0].clone();
        updated["data"] = json!({"revision": "updated"});
        let previous = crate::yaml::parse_yaml_documents_k8s_compatible(&stored).unwrap();
        let appended = merge_append_manifest(&[updated.clone()], previous, &ObjectScopes::default()).unwrap();
        let restored = crate::yaml::parse_yaml_documents_k8s_compatible(&appended).unwrap();
        assert_eq!(restored.len(), manifests.len());
        assert!(restored.contains(&updated));
        for manifest in &manifests[1..] {
            assert!(restored.contains(manifest));
        }
    }

    fn key(api_version: &str, kind: &str, namespace: Option<&str>, name: &str) -> ResourceKey {
        ResourceKey {
            gvk: GroupVersionKind::from_api_version_kind(api_version, kind).unwrap(),
            namespace: namespace.map(str::to_string),
            name: name.to_string(),
        }
    }

    /// Moving a manifest to another API version changes its key, not its object, so
    /// prune must not delete the object it was just applied as.
    #[test]
    fn test_keys_to_prune_ignores_api_version_changes() {
        let live: HashSet<ResourceKey> = [
            key("example.com/v1beta1", "Widget", Some("app"), "w"),
            key("example.com/v1beta1", "Widget", Some("app"), "gone"),
        ]
        .into_iter()
        .collect();
        let current = [key("example.com/v1", "Widget", Some("app"), "w")];

        let to_prune = keys_to_prune(&live, &current, &ObjectScopes::default());

        let names: Vec<&str> = to_prune.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(names, vec!["gone"]);
    }

    /// A cluster-scoped object written with and without a namespace is one object.
    #[tokio::test]
    async fn test_collapse_duplicate_objects_ignores_namespace_of_cluster_scoped_kinds() {
        let manifests = vec![
            json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "reader", "namespace": "app"}, "rules": ["first"]}),
            json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "reader"}, "rules": ["last"]}),
        ];
        let mut scopes = ObjectScopes::default();
        scopes
            .load(
                &crate::kubernetes::MockKubeClient::new(),
                &manifest_keys(&manifests).unwrap(),
            )
            .await;

        let collapsed = collapse_duplicate_objects(manifests, &mut HashMap::new(), &scopes).unwrap();

        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0]["rules"][0], "last");
    }

    fn release_with_keys(revision: u32, status: ReleaseStatus, keys: Vec<ResourceKey>) -> ReleaseState {
        let mut release = crate::kubernetes::memory::sample_release("app", "app", revision, status);
        release.resource_keys = keys;
        release
    }

    /// The recorder records the revision before the apply, prunes what the previous
    /// revision had and the new one does not, keeps keys that failed to prune so the
    /// next apply retries them, and supersedes the previous revision.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_reserves_prunes_and_keeps_failed_prunes() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("stuck");
        storage
            .save_release(&release_with_keys(
                1,
                ReleaseStatus::Deployed,
                vec![
                    key("v1", "ConfigMap", Some("app"), "kept"),
                    key("v1", "ConfigMap", Some("app"), "removed"),
                    key("v1", "ConfigMap", Some("app"), "stuck"),
                ],
            ))
            .await
            .unwrap();
        let manifests =
            vec![json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "kept", "namespace": "app"}})];
        let mut scopes = ObjectScopes::default();

        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &manifests,
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        let reserved = storage.get_release("app", "app", 2).await.unwrap().unwrap();
        assert_eq!(reserved.status, ReleaseStatus::Rendered);
        assert_eq!(reserved.resource_keys, manifest_keys(&manifests).unwrap());

        let run = recorder
            .run(
                &client,
                &manifests,
                &mut scopes,
                settings(0),
                Interruption::never(),
                &mut |_| {},
            )
            .await
            .unwrap();
        let release = &run.release;

        assert_eq!(release.status, ReleaseStatus::Deployed);
        let names: Vec<&str> = release.resource_keys.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(names, vec!["kept", "stuck"]);
        // A failed prune fails the command although the revision is deployed.
        assert_eq!(run.prune_failed_count, 1);
        assert!(run.error("Apply").is_some());
        let stored = storage.get_release("app", "app", 2).await.unwrap().unwrap();
        assert_eq!(stored.status, ReleaseStatus::Deployed);
        assert_eq!(stored.resource_keys, release.resource_keys);
        let previous = storage.get_release("app", "app", 1).await.unwrap().unwrap();
        assert_eq!(previous.status, ReleaseStatus::Superseded);
    }

    /// Pruning keeps what Argo CD keeps: `Prune=false` resources stay and are no
    /// longer tracked, `Prune=confirm` resources stay tracked until a confirmed apply.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_prunes_by_argocd_prune_sync_option() {
        for confirm_prune in [false, true] {
            let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
            let client = crate::kubernetes::MockKubeClient::new();
            let previous = [
                ("retained", "Prune=false"),
                ("confirmed", "Delete=confirm,Prune=confirm"),
                ("plain", ""),
            ];
            for (name, options) in previous {
                let object: DynamicObject = serde_json::from_value(json!({
                    "apiVersion": "v1", "kind": "Namespace",
                    "metadata": {"name": name, "annotations": {"argocd.argoproj.io/sync-options": options}}
                }))
                .unwrap();
                client.add_resource(key("v1", "Namespace", None, name), object);
            }
            storage
                .save_release(&release_with_keys(
                    1,
                    ReleaseStatus::Deployed,
                    previous
                        .iter()
                        .map(|(name, _)| key("v1", "Namespace", None, name))
                        .collect(),
                ))
                .await
                .unwrap();
            let mut scopes = ObjectScopes::default();
            let recorder = ReleaseRecorder::reserve(
                &storage,
                "app",
                "app",
                &[],
                false,
                &client,
                &mut scopes,
                &ReleaseClaim::manual(),
            )
            .await
            .unwrap();
            let settings = ApplySettings {
                confirm_prune,
                ..settings(0)
            };
            let release = recorder
                .run(&client, &[], &mut scopes, settings, Interruption::never(), &mut |_| {})
                .await
                .unwrap()
                .release;

            let mut live = client
                .get_all_resources()
                .into_keys()
                .map(|key| key.name)
                .collect::<Vec<_>>();
            live.sort();
            let tracked = release
                .resource_keys
                .iter()
                .map(|key| key.name.as_str())
                .collect::<Vec<_>>();
            if confirm_prune {
                assert_eq!(live, ["retained"]);
                assert!(tracked.is_empty());
            } else {
                assert_eq!(live, ["confirmed", "retained"]);
                assert_eq!(tracked, ["confirmed"]);
            }
        }
    }

    /// Catches a purge that deletes an object whose `Delete` option it could not
    /// read: with the read failing, `Delete=false` could be hiding, so the object
    /// is reported failed and stays, while a readable plain object is deleted.
    #[tokio::test]
    async fn test_purge_never_deletes_an_object_whose_option_cannot_be_read() {
        let client = LatencyClient {
            unreadable: "guarded",
            latency: Duration::ZERO,
            ..LatencyClient::new("")
        };
        let keys = vec![
            live(&client.inner, "plain", None),
            live(&client.inner, "guarded", Some("Delete=false")),
        ];

        let purged = purge_resources(&client, &keys, &|_| false, 4, false, &mut |_| {}).await;

        assert_eq!(
            purged.deleted.iter().map(|key| key.name.as_str()).collect::<Vec<_>>(),
            ["plain"]
        );
        assert_eq!(
            purged.failed.iter().map(|key| key.name.as_str()).collect::<Vec<_>>(),
            ["guarded"]
        );
        assert!(client.inner.get_all_resources().keys().any(|key| key.name == "guarded"));
    }

    /// Catches a prune that deletes an object whose `Prune` option it could not
    /// read: the object stays, stays tracked so the next apply retries it, and
    /// the apply reports a failed prune.
    #[tokio::test]
    async fn test_prune_never_deletes_an_object_whose_option_cannot_be_read() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient {
            unreadable: "guarded",
            latency: Duration::ZERO,
            ..LatencyClient::new("")
        };
        let guarded = live(&client.inner, "guarded", Some("Prune=false"));
        let stale = live(&client.inner, "stale", None);
        storage
            .save_release(&release_with_keys(1, ReleaseStatus::Deployed, vec![guarded, stale]))
            .await
            .unwrap();
        let mut scopes = ObjectScopes::default();
        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();

        let run = recorder
            .run(
                &client,
                &[],
                &mut scopes,
                settings(0),
                Interruption::never(),
                &mut |_| {},
            )
            .await
            .unwrap();

        let mut live_names = client
            .inner
            .get_all_resources()
            .into_keys()
            .map(|key| key.name)
            .collect::<Vec<_>>();
        live_names.sort();
        assert_eq!(live_names, ["guarded"]);
        assert_eq!(
            run.release
                .resource_keys
                .iter()
                .map(|key| key.name.as_str())
                .collect::<Vec<_>>(),
            ["guarded"]
        );
        assert_eq!(run.prune_failed_count, 1);
    }

    /// Catches an object that moved from one Release to another being deleted by
    /// the first Release's prune, which takes the second's object (and, for a
    /// CRD, every custom resource) with it: what another current Release now
    /// renders is neither deleted nor tracked by the Release that lost it.
    #[tokio::test]
    async fn test_prune_leaves_an_object_another_release_now_renders() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient {
            latency: Duration::ZERO,
            ..LatencyClient::new("")
        };
        let moved = live(&client.inner, "moved", None);
        let old = live(&client.inner, "old", None);
        storage
            .save_release(&release_with_keys(1, ReleaseStatus::Deployed, vec![moved.clone(), old]))
            .await
            .unwrap();
        let mut scopes = ObjectScopes::default();
        let handed_over = HashSet::from([scopes.identity(&moved)]);
        let nobody = HashSet::new();
        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        let settings = ApplySettings {
            protected: Some(Protected {
                handed_over: &handed_over,
                pending: &nobody,
            }),
            ..settings(0)
        };

        let run = recorder
            .run(&client, &[], &mut scopes, settings, Interruption::never(), &mut |_| {})
            .await
            .unwrap();

        let live_names = client
            .inner
            .get_all_resources()
            .into_keys()
            .map(|key| key.name)
            .collect::<Vec<_>>();
        assert_eq!(live_names, ["moved"]);
        assert!(run.release.resource_keys.is_empty(), "the Release no longer tracks it");
        assert_eq!(run.prune_failed_count, 0);
    }

    /// Catches an object handed to a Release that never applies in this run (a
    /// later wave aborts, or the deadline passes) being forgotten: the old
    /// Release leaves it in the cluster and keeps it tracked, so the next apply
    /// of either Release still finds an owner for it.
    #[tokio::test]
    async fn test_prune_keeps_tracking_an_object_whose_new_release_has_not_applied() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient {
            latency: Duration::ZERO,
            ..LatencyClient::new("")
        };
        let moved = live(&client.inner, "moved", None);
        let old = live(&client.inner, "old", None);
        storage
            .save_release(&release_with_keys(1, ReleaseStatus::Deployed, vec![moved.clone(), old]))
            .await
            .unwrap();
        let mut scopes = ObjectScopes::default();
        let pending = HashSet::from([scopes.identity(&moved)]);
        let nobody = HashSet::new();
        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        let settings = ApplySettings {
            protected: Some(Protected {
                handed_over: &nobody,
                pending: &pending,
            }),
            ..settings(0)
        };

        let run = recorder
            .run(&client, &[], &mut scopes, settings, Interruption::never(), &mut |_| {})
            .await
            .unwrap();

        assert_eq!(
            client
                .inner
                .get_all_resources()
                .into_keys()
                .map(|key| key.name)
                .collect::<Vec<_>>(),
            ["moved"]
        );
        assert_eq!(
            run.release
                .resource_keys
                .iter()
                .map(|key| key.name.as_str())
                .collect::<Vec<_>>(),
            ["moved"],
            "still tracked, so a later apply can hand it over"
        );
        assert_eq!(run.prune_failed_count, 0);
    }

    /// Storage whose listing is followed by a competing claimant's revision,
    /// the interleaving of two applies of one release.
    struct RacingStorage {
        inner: crate::kubernetes::memory::InMemoryReleaseStorage,
        competitor: std::sync::Mutex<Option<ReleaseState>>,
    }

    #[async_trait::async_trait]
    impl ReleaseStorage for RacingStorage {
        async fn save_release(&self, release: &ReleaseState) -> Result<()> {
            self.inner.save_release(release).await
        }
        async fn complete_release(&self, release: &ReleaseState) -> Result<()> {
            self.inner.complete_release(release).await
        }
        async fn get_latest_release(&self, name: &str, namespace: &str) -> Result<Option<ReleaseState>> {
            self.inner.get_latest_release(name, namespace).await
        }
        async fn get_release(&self, name: &str, namespace: &str, revision: u32) -> Result<Option<ReleaseState>> {
            self.inner.get_release(name, namespace, revision).await
        }
        async fn list_revisions(&self, name: &str, namespace: &str) -> Result<Vec<u32>> {
            let listed = self.inner.list_revisions(name, namespace).await?;
            let competitor = self.competitor.lock().unwrap().take();
            if let Some(competitor) = competitor {
                self.inner.save_release(&competitor).await?;
            }
            Ok(listed)
        }
        async fn update_release_status(
            &self,
            name: &str,
            namespace: &str,
            revision: u32,
            status: ReleaseStatus,
            error: Option<String>,
        ) -> Result<()> {
            self.inner
                .update_release_status(name, namespace, revision, status, error)
                .await
        }
        async fn list_releases(&self, namespace: Option<&str>) -> Result<Vec<crate::kubernetes::ReleaseInfo>> {
            self.inner.list_releases(namespace).await
        }
        async fn delete_release(&self, name: &str, namespace: &str, revision: u32) -> Result<()> {
            self.inner.delete_release(name, namespace, revision).await
        }
        async fn delete_all_revisions(&self, name: &str, namespace: &str) -> Result<u32> {
            self.inner.delete_all_revisions(name, namespace).await
        }
    }

    /// Catches a reservation that lists the revisions a second time after the
    /// fence decided: a claimant that created the next revision in between would
    /// be skipped over and both would apply. The reservation numbers its
    /// revision from the list the fence read, so the create is the compare-and-swap.
    #[tokio::test]
    async fn test_a_reservation_loses_to_a_claimant_that_created_the_next_revision_first() {
        let storage = RacingStorage {
            inner: crate::kubernetes::memory::InMemoryReleaseStorage::new(),
            competitor: std::sync::Mutex::new(Some(release_with_keys(2, ReleaseStatus::Rendered, Vec::new()))),
        };
        storage
            .inner
            .save_release(&release_with_keys(1, ReleaseStatus::Deployed, Vec::new()))
            .await
            .unwrap();
        let client = crate::kubernetes::MockKubeClient::new();
        let mut scopes = ObjectScopes::default();

        let error = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .err()
        .expect("the reservation must fail");

        assert!(error.to_string().contains("revision 2 already exists"), "{error}");
        assert_eq!(storage.inner.list_revisions("app", "app").await.unwrap(), [1, 2]);
    }

    /// A release that renders nothing is still recorded and prunes everything the
    /// previous revision deployed.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_empty_release_prunes_previous_resources() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        storage
            .save_release(&release_with_keys(
                1,
                ReleaseStatus::Deployed,
                vec![key("v1", "ConfigMap", Some("app"), "old")],
            ))
            .await
            .unwrap();
        let mut scopes = ObjectScopes::default();

        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        let release = recorder
            .run(
                &client,
                &[],
                &mut scopes,
                settings(0),
                Interruption::never(),
                &mut |_| {},
            )
            .await
            .unwrap()
            .release;

        assert_eq!(release.status, ReleaseStatus::Deployed);
        assert!(release.resource_keys.is_empty());
        assert!(client.events().contains(&"delete-end old".to_string()));
    }

    /// `--append-release` requires a Deployed previous revision and fails before the
    /// cluster is touched otherwise.
    #[tokio::test]
    async fn test_release_recorder_append_rejects_undeployed_previous_revision() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        storage
            .save_release(&release_with_keys(1, ReleaseStatus::Failed, vec![]))
            .await
            .unwrap();

        let result = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            true,
            &crate::kubernetes::MockKubeClient::new(),
            &mut ObjectScopes::default(),
            &ReleaseClaim::manual(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1]);
    }

    /// An apply that fails before its revision is completed marks the reserved
    /// revision Failed instead of leaving it Rendered.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_marks_revision_failed_when_apply_errors() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient {
            refresh_fails: true,
            ..LatencyClient::new("")
        };
        // The refresh after the CRD batch fails, aborting the apply.
        let manifests = vec![
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "widgets.example.com"}, "spec": {"group": "example.com"}}),
            json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "w", "namespace": "app"}}),
        ];
        let mut scopes = ObjectScopes::default();

        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &manifests,
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        let result = recorder
            .run(
                &client,
                &manifests,
                &mut scopes,
                settings(0),
                Interruption::never(),
                &mut |_| {},
            )
            .await;

        assert!(result.is_err());
        let revision = storage.get_release("app", "app", 1).await.unwrap().unwrap();
        assert_eq!(revision.status, ReleaseStatus::Failed);
        assert!(revision.error.unwrap().contains("connection reset"));
        // Only the CRD applied; the Widget was never written, so it is not recorded
        // as partially applied, and a later prune cannot delete an object it never made.
        assert_eq!(
            revision.resource_keys,
            vec![manifest_keys(&manifests).unwrap()[0].clone()]
        );
    }

    /// Pruning deletes what a terminating resource may depend on only once it is gone.
    #[tokio::test(start_paused = true)]
    async fn test_prune_resources_waits_for_terminating_dependents() {
        let client = LatencyClient::new("");
        client.terminating_reads.lock().unwrap().insert("widget".to_string(), 3);
        let keys = [
            key("apps/v1", "Deployment", Some("app"), "controller"),
            key("example.com/v1", "Widget", Some("app"), "widget"),
        ];

        let mut waiting = 0;
        let failed = prune_resources(&client, keys.iter().collect(), 8, &mut |event| {
            if let ApplyEvent::PruneWaiting { count } = event {
                waiting = count;
            }
        })
        .await;

        assert!(failed.is_empty());
        assert_eq!(waiting, 1);
        assert!(client.position("gone widget") < client.position("delete-start controller"));
    }

    /// A resource whose kind the cluster no longer serves, such as a custom resource
    /// whose CRD was pruned, is already deleted, so it is not retried forever.
    #[tokio::test(start_paused = true)]
    async fn test_prune_resources_treats_removed_kinds_as_deleted() {
        let client = LatencyClient {
            removed_kind: "Widget",
            ..LatencyClient::new("")
        };
        let keys = [key("example.com/v1", "Widget", Some("app"), "widget")];

        let failed = prune_resources(&client, keys.iter().collect(), 8, &mut |_| {}).await;

        assert!(failed.is_empty());
    }

    fn settings(history_limit: u32) -> ApplySettings<'static> {
        ApplySettings {
            concurrency: 8,
            history_limit,
            revisions: RevisionPolicy::Always,
            confirm_prune: false,
            ownership: &NYL_OWNERSHIP,
            protected: None,
            unapplied: None,
        }
    }

    static NYL_OWNERSHIP: FieldOwnership = FieldOwnership::Nyl;

    /// A deployed apply deletes the oldest revisions beyond the history limit.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_enforces_history_limit_after_deploy() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        for revision in 1..=3 {
            let status = if revision == 3 {
                ReleaseStatus::Deployed
            } else {
                ReleaseStatus::Superseded
            };
            storage
                .save_release(&release_with_keys(revision, status, vec![]))
                .await
                .unwrap();
        }
        let mut scopes = ObjectScopes::default();

        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &[],
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        recorder
            .run(
                &client,
                &[],
                &mut scopes,
                settings(2),
                Interruption::never(),
                &mut |_| {},
            )
            .await
            .unwrap();

        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![3, 4]);
    }

    /// A recorded version that is no longer served, while the kind still exists in
    /// another version, is not "deleted": the key stays recorded for a retry.
    #[tokio::test(start_paused = true)]
    async fn test_prune_resources_keeps_kinds_served_in_other_versions() {
        let client = LatencyClient {
            removed_kind: "HorizontalPodAutoscaler",
            kind_gone: false,
            ..LatencyClient::new("")
        };
        let keys = [key(
            "autoscaling/v2beta2",
            "HorizontalPodAutoscaler",
            Some("app"),
            "web",
        )];

        let failed = prune_resources(&client, keys.iter().collect(), 8, &mut |_| {}).await;

        assert_eq!(failed.len(), 1);
    }

    fn live(client: &crate::kubernetes::MockKubeClient, name: &str, options: Option<&str>) -> ResourceKey {
        let mut object =
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "app"}});
        if let Some(options) = options {
            object["metadata"]["annotations"] = json!({"argocd.argoproj.io/sync-options": options});
        }
        let key = ResourceKey::from_json_value(&object).unwrap();
        client.add_resource(key.clone(), serde_json::from_value(object).unwrap());
        key
    }

    /// Catches a purge that ignores a resource's `Delete` option, deletes what
    /// the caller excluded, or counts what it kept as remaining: `Delete=false`
    /// and unconfirmed `Delete=confirm` stay, an excluded object stays, and what
    /// a purge would still delete is what `remaining_after_purge` lists.
    #[tokio::test]
    async fn test_purge_honors_the_delete_option_and_the_exclusions() {
        let client = crate::kubernetes::MockKubeClient::new();
        let keys = vec![
            live(&client, "plain", None),
            live(&client, "kept", Some("Delete=false")),
            live(&client, "awaiting", Some("Delete=confirm")),
            live(&client, "excluded", None),
        ];
        let excluded = |key: &ResourceKey| key.name == "excluded";
        let before = remaining_after_purge(&client, &keys, &excluded).await.unwrap();
        assert_eq!(
            before.iter().map(|key| key.name.as_str()).collect::<Vec<_>>(),
            ["plain"]
        );

        let purged = purge_resources(&client, &keys, &excluded, 4, false, &mut |_| {}).await;
        assert_eq!(
            purged.deleted.iter().map(|key| key.name.as_str()).collect::<Vec<_>>(),
            ["plain"]
        );
        let mut kept = purged.kept.iter().map(|key| key.name.as_str()).collect::<Vec<_>>();
        kept.sort_unstable();
        assert_eq!(kept, ["awaiting", "kept"]);
        assert!(purged.failed.is_empty());
        assert!(remaining_after_purge(&client, &keys, &excluded)
            .await
            .unwrap()
            .is_empty());
        assert!(client
            .get_resource(&keys[3].gvk, Some("app"), "excluded")
            .await
            .unwrap()
            .is_some());

        let confirmed = purge_resources(&client, &keys, &excluded, 4, true, &mut |_| {}).await;
        assert!(confirmed.deleted.iter().any(|key| key.name == "awaiting"));
        assert!(client
            .get_resource(&keys[2].gvk, Some("app"), "awaiting")
            .await
            .unwrap()
            .is_none());
    }

    fn keep_unchanged() -> ApplySettings<'static> {
        ApplySettings {
            revisions: RevisionPolicy::KeepUnchanged {
                publication_stands: None,
            },
            ..settings(0)
        }
    }

    fn config_map(name: &str) -> serde_json::Value {
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "app"}})
    }

    /// Reserves, applies, and completes one apply of `manifests` as `nyl apply` would.
    async fn apply_with(
        storage: &dyn ReleaseStorage,
        client: &LatencyClient,
        manifests: &[serde_json::Value],
        settings: ApplySettings<'_>,
    ) -> ReleaseRun {
        let mut scopes = ObjectScopes::default();
        let recorder = ReleaseRecorder::reserve(
            storage,
            "app",
            "app",
            manifests,
            false,
            client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        recorder
            .run(
                client,
                manifests,
                &mut scopes,
                settings,
                Interruption::never(),
                &mut |_| {},
            )
            .await
            .unwrap()
    }

    /// Applying again what the latest deployed revision records still applies every
    /// resource, which repairs drift, but records no revision: the history keeps its
    /// useful revisions and the latest one stays current.
    #[tokio::test(start_paused = true)]
    async fn test_unchanged_apply_applies_again_and_keeps_the_latest_revision() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        let manifests = vec![config_map("a"), config_map("b")];

        let first = apply_with(&storage, &client, &manifests, keep_unchanged()).await;
        assert!(!first.unchanged);
        assert_eq!(first.release.revision, 1);

        let second = apply_with(&storage, &client, &manifests, keep_unchanged()).await;
        assert!(second.unchanged);
        assert_eq!(second.release.revision, 1);
        assert_eq!(second.release.status, ReleaseStatus::Deployed);
        // The server-side apply ran for every resource both times.
        assert_eq!(second.apply.outcomes.len(), 2);
        assert_eq!(
            client
                .events()
                .iter()
                .filter(|event| event.starts_with("start "))
                .count(),
            4
        );
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1]);
        assert!(second.error("Apply").is_none());
    }

    /// The same applies record every time under [`RevisionPolicy::Always`], as
    /// `release rollback` needs.
    #[tokio::test(start_paused = true)]
    async fn test_always_policy_records_an_unchanged_apply() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        let manifests = vec![config_map("a")];

        apply_with(&storage, &client, &manifests, settings(0)).await;
        let second = apply_with(&storage, &client, &manifests, settings(0)).await;

        assert!(!second.unchanged);
        assert_eq!(second.release.revision, 2);
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1, 2]);
    }

    /// A different manifest is a new revision and supersedes the one before it.
    #[tokio::test(start_paused = true)]
    async fn test_changed_manifest_records_a_new_revision() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        apply_with(&storage, &client, &[config_map("a")], keep_unchanged()).await;

        let mut changed = config_map("a");
        changed["data"] = json!({"key": "value"});
        let second = apply_with(&storage, &client, &[changed], keep_unchanged()).await;

        assert!(!second.unchanged);
        assert_eq!(second.release.revision, 2);
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1, 2]);
        let first = storage.get_release("app", "app", 1).await.unwrap().unwrap();
        assert_eq!(first.status, ReleaseStatus::Superseded);
    }

    /// A latest revision that failed is never kept, even when the retry records the
    /// same manifest: the retry is what makes the release `Deployed`.
    #[tokio::test(start_paused = true)]
    async fn test_apply_after_a_failed_revision_records_a_new_one() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let manifests = vec![config_map("a"), config_map("broken")];

        let failed = apply_with(&storage, &LatencyClient::new("broken"), &manifests, keep_unchanged()).await;
        assert_eq!(failed.release.status, ReleaseStatus::Failed);

        let retry = apply_with(&storage, &LatencyClient::new(""), &manifests, keep_unchanged()).await;

        assert!(!retry.unchanged);
        assert_eq!(retry.release.revision, 2);
        assert_eq!(retry.release.status, ReleaseStatus::Deployed);
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1, 2]);
    }

    /// An apply that pruned something, or failed to prune, records a revision even
    /// though its manifest equals the latest one: the revision records the pruning
    /// and what stays tracked for a retry.
    #[tokio::test(start_paused = true)]
    async fn test_apply_that_prunes_records_a_new_revision() {
        let manifests = vec![config_map("kept")];
        for (stuck, pruned_name, failed) in [("", "removed", 0), ("stuck", "stuck", 1)] {
            let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
            let client = LatencyClient::new(stuck);
            let mut recorded = release_with_keys(
                1,
                ReleaseStatus::Deployed,
                vec![
                    key("v1", "ConfigMap", Some("app"), "kept"),
                    key("v1", "ConfigMap", Some("app"), pruned_name),
                ],
            );
            recorded.manifest = manifests_to_yaml(&manifests).unwrap();
            storage.save_release(&recorded).await.unwrap();

            let run = apply_with(&storage, &client, &manifests, keep_unchanged()).await;

            assert!(!run.unchanged, "pruning {pruned_name} is a change");
            assert_eq!(run.release.revision, 2);
            assert_eq!(run.prune_failed_count, failed);
            assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1, 2]);
        }
    }

    /// What may differ between the latest revision and a new apply without making
    /// it a change, and what may not.
    #[test]
    fn test_records_same_compares_what_a_revision_records() {
        use crate::gitops::reconcile::RenderIndexOwner;
        use crate::kubernetes::{ReleaseMeta, ReleasePublication};

        let owner = |uid: &str| RenderIndexOwner::unit("env", "unit", uid);
        let publication = |prefix: &str, commit: Option<&str>| ReleasePublication {
            repository: "repo".into(),
            branch: "main".into(),
            path_prefix: prefix.into(),
            commit: commit.map(str::to_owned),
        };
        let revision = |owner: Option<RenderIndexOwner>, publication: Option<ReleasePublication>| {
            let mut release = release_with_keys(
                1,
                ReleaseStatus::Deployed,
                vec![
                    key("v1", "ConfigMap", Some("app"), "a"),
                    key("v1", "ConfigMap", Some("app"), "b"),
                ],
            );
            release.meta = ReleaseMeta { owner, publication };
            release
        };
        let latest = revision(Some(owner("1")), Some(publication("p", Some("old"))));
        let same_as = |new: &ReleaseState, stands: Option<PublicationStands<'_>>| records_same(&latest, new, stands);

        // Timestamps and the order of keys are not what a revision records.
        let mut reordered = revision(Some(owner("1")), Some(publication("p", Some("old"))));
        reordered.resource_keys.reverse();
        reordered.rendered_at += chrono::Duration::hours(1);
        reordered.applied_at = None;
        assert!(same_as(&reordered, None));

        // Another owner, destination, manifest, or key set is a change.
        assert!(!same_as(
            &revision(Some(owner("2")), Some(publication("p", Some("old")))),
            None
        ));
        assert!(!same_as(&revision(None, Some(publication("p", Some("old")))), None));
        assert!(!same_as(
            &revision(Some(owner("1")), Some(publication("q", Some("old")))),
            None
        ));
        assert!(!same_as(&revision(Some(owner("1")), None), None));
        let mut other_manifest = revision(Some(owner("1")), Some(publication("p", Some("old"))));
        other_manifest.manifest.push_str("# changed");
        assert!(!same_as(&other_manifest, None));
        let mut other_keys = revision(Some(owner("1")), Some(publication("p", Some("old"))));
        other_keys.resource_keys.pop();
        assert!(!same_as(&other_keys, None));
        let mut failed = latest.clone();
        failed.status = ReleaseStatus::Failed;
        assert!(!records_same(&failed, &latest, None));

        // A newer publication commit is a change unless the old one still stands for it.
        let newer = revision(Some(owner("1")), Some(publication("p", Some("new"))));
        assert!(!same_as(&newer, None));
        assert!(!same_as(&newer, Some(&|_| false)));
        assert!(same_as(&newer, Some(&|old| old == "old")));
        // A revision that recorded no commit has nothing that can stand for one.
        let uncommitted = revision(Some(owner("1")), Some(publication("p", None)));
        assert!(!records_same(&uncommitted, &newer, Some(&|_| true)));
    }

    /// Storage whose revision deletes fail, as a flaky API server does: with
    /// `deletes_first`, the revision is gone although the request reports failure.
    struct FailingDeletes {
        inner: crate::kubernetes::memory::InMemoryReleaseStorage,
        deletes_first: bool,
    }

    #[async_trait::async_trait]
    impl ReleaseStorage for FailingDeletes {
        async fn save_release(&self, release: &ReleaseState) -> Result<()> {
            self.inner.save_release(release).await
        }
        async fn complete_release(&self, release: &ReleaseState) -> Result<()> {
            self.inner.complete_release(release).await
        }
        async fn get_latest_release(&self, name: &str, namespace: &str) -> Result<Option<ReleaseState>> {
            self.inner.get_latest_release(name, namespace).await
        }
        async fn get_release(&self, name: &str, namespace: &str, revision: u32) -> Result<Option<ReleaseState>> {
            self.inner.get_release(name, namespace, revision).await
        }
        async fn list_revisions(&self, name: &str, namespace: &str) -> Result<Vec<u32>> {
            self.inner.list_revisions(name, namespace).await
        }
        async fn update_release_status(
            &self,
            name: &str,
            namespace: &str,
            revision: u32,
            status: ReleaseStatus,
            error: Option<String>,
        ) -> Result<()> {
            self.inner
                .update_release_status(name, namespace, revision, status, error)
                .await
        }
        async fn list_releases(&self, namespace: Option<&str>) -> Result<Vec<crate::kubernetes::ReleaseInfo>> {
            self.inner.list_releases(namespace).await
        }
        async fn delete_release(&self, name: &str, namespace: &str, revision: u32) -> Result<()> {
            if self.deletes_first {
                self.inner.delete_release(name, namespace, revision).await?;
            }
            Err(NylError::Kubernetes("the API server timed out".to_string()))
        }
        async fn delete_all_revisions(&self, name: &str, namespace: &str) -> Result<u32> {
            self.inner.delete_all_revisions(name, namespace).await
        }
    }

    /// A reservation that cannot be released is recorded like any other revision;
    /// one a failed request deleted anyway is released, and the apply stays a
    /// success that changed nothing.
    #[tokio::test(start_paused = true)]
    async fn test_a_failed_release_of_the_reservation_is_settled_by_what_is_stored() {
        for (deletes_first, unchanged, revisions) in [(false, false, vec![1, 2]), (true, true, vec![1])] {
            let storage = FailingDeletes {
                inner: crate::kubernetes::memory::InMemoryReleaseStorage::new(),
                deletes_first,
            };
            let client = LatencyClient::new("");
            let manifests = vec![config_map("a")];
            apply_with(&storage, &client, &manifests, keep_unchanged()).await;

            let run = apply_with(&storage, &client, &manifests, keep_unchanged()).await;

            assert_eq!(run.unchanged, unchanged, "deletes_first: {deletes_first}");
            assert!(run.error("Apply").is_none());
            assert_eq!(storage.list_revisions("app", "app").await.unwrap(), revisions);
            let latest = storage.get_latest_release("app", "app").await.unwrap().unwrap();
            assert_eq!(latest.status, ReleaseStatus::Deployed);
        }
    }

    /// A kept apply still enforces the history limit, which may have been lowered
    /// since the latest revision deployed.
    #[tokio::test(start_paused = true)]
    async fn test_a_kept_apply_enforces_the_history_limit() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        let manifests = vec![config_map("a")];
        for revision in 1..=3 {
            let mut recorded = release_with_keys(
                revision,
                if revision == 3 {
                    ReleaseStatus::Deployed
                } else {
                    ReleaseStatus::Superseded
                },
                vec![key("v1", "ConfigMap", Some("app"), "a")],
            );
            recorded.manifest = manifests_to_yaml(&manifests).unwrap();
            storage.save_release(&recorded).await.unwrap();
        }

        let limited = ApplySettings {
            history_limit: 2,
            ..keep_unchanged()
        };
        let run = apply_with(&storage, &client, &manifests, limited).await;

        assert!(run.unchanged);
        assert_eq!(run.release.revision, 3);
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![2, 3]);
    }

    /// `--append-release` compares the merged manifest: appending what the latest
    /// revision already holds changes nothing, appending something new does.
    #[tokio::test(start_paused = true)]
    async fn test_append_release_keeps_an_unchanged_revision_and_records_a_change() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        let append = |manifests: Vec<serde_json::Value>| {
            let (storage, client) = (&storage, &client);
            async move {
                let mut scopes = ObjectScopes::default();
                let recorder = ReleaseRecorder::reserve(
                    storage,
                    "app",
                    "app",
                    &manifests,
                    true,
                    client,
                    &mut scopes,
                    &ReleaseClaim::manual(),
                )
                .await
                .unwrap();
                recorder
                    .run(
                        client,
                        &manifests,
                        &mut scopes,
                        keep_unchanged(),
                        Interruption::never(),
                        &mut |_| {},
                    )
                    .await
                    .unwrap()
            }
        };

        assert!(!append(vec![config_map("a")]).await.unchanged);
        let again = append(vec![config_map("a")]).await;
        assert!(again.unchanged);
        assert_eq!(again.release.revision, 1);

        let grown = append(vec![config_map("b")]).await;
        assert!(!grown.unchanged);
        assert_eq!(grown.release.revision, 2);
        assert_eq!(grown.release.resource_keys.len(), 2);
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1, 2]);
    }

    /// A failure to release field ownership is not a settled apply, even though
    /// every key is tracked as before.
    #[tokio::test(start_paused = true)]
    async fn test_a_failed_release_of_field_ownership_is_never_kept() {
        let storage = crate::kubernetes::memory::InMemoryReleaseStorage::new();
        let client = LatencyClient::new("");
        let manifests = vec![config_map("a")];
        apply_with(&storage, &client, &manifests, keep_unchanged()).await;
        let mut scopes = ObjectScopes::default();
        let recorder = ReleaseRecorder::reserve(
            &storage,
            "app",
            "app",
            &manifests,
            false,
            &client,
            &mut scopes,
            &ReleaseClaim::manual(),
        )
        .await
        .unwrap();
        let mut stored_apply = apply_sorted_manifests(&client, &manifests, &FieldOwnership::Nyl, 1, &mut |_| {})
            .await
            .unwrap();
        stored_apply.release_failed_count = 1;
        let completed = AtomicBool::new(false);
        let done = recorder
            .complete(
                &client,
                &stored_apply,
                &mut scopes,
                keep_unchanged(),
                &mut |_| {},
                &completed,
            )
            .await
            .unwrap();

        assert!(!done.unchanged);
        assert_eq!(done.release.revision, 2);
    }
}
