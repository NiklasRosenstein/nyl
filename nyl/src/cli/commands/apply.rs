use chrono::Utc;
use clap::Args;
use futures::stream::{self, StreamExt};
use kube::api::DynamicObject;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use crate::{
    cli::{
        apply_output::{ApplyEvent, ApplyEventSink, ApplyRenderer, ApplyReport, Phase, PhaseTiming},
        commands::render::{
            run_render_preflight, ClusterClientRequirement, DiscoveryProgress, RenderOptions, RenderPreflightOptions,
        },
        namespace_resolution::{adjust_duplicate_keys_for_namespace_resolution, resolve_manifest_namespaces},
    },
    kubernetes::{
        enforce_release_history_limit,
        prune::{prune_decision, PruneDecision},
        ApplyOutcome, GroupVersionKind, KubeClient, KubernetesReleaseStorage, ObjectIdentity, ReleaseState,
        ReleaseStatus, ReleaseStorage, ResourceKey, ResourceOrdering,
    },
    NylError, Result,
};

/// Apply rendered manifests to the cluster
#[derive(Args, Debug)]
pub struct ApplyArgs {
    #[command(flatten)]
    pub common: RenderOptions,

    /// Release name (required if no Release in file)
    #[arg(long)]
    pub name: Option<String>,

    /// Release namespace (required if no Release in file)
    #[arg(long)]
    pub namespace: Option<String>,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,

    /// Append to previous release instead of replacing it.
    /// Merges current resources with previous release (union, current wins on duplicates).
    /// Skips pruning to preserve resources from previous releases.
    #[arg(long)]
    pub append_release: bool,

    /// Prune resources whose Argo CD sync option is `Prune=confirm`, such as managed Namespaces.
    /// Without it they are kept and stay tracked until a confirmed apply.
    #[arg(long, conflicts_with = "no_release")]
    pub confirm_prune: bool,

    /// Apply resources without creating release revisions or pruning.
    #[arg(long, conflicts_with_all = ["append_release", "name", "namespace"])]
    pub no_release: bool,

    /// Maximum number of resources applied or pruned at the same time.
    ///
    /// Resources are applied in Argo CD's kind order, one batch per kind; the
    /// resources of a batch are applied concurrently and a batch completes before
    /// the next one starts. Use 1 to apply serially.
    #[arg(long, default_value_t = DEFAULT_APPLY_CONCURRENCY, value_parser = clap::value_parser!(u16).range(1..))]
    pub concurrency: u16,

    /// Maximum number of release revisions to keep; older revisions are deleted
    /// after this apply. 0 keeps every revision. Overrides `[release] history_limit`
    /// in nyl.toml.
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "no_release",
        value_parser = crate::config::parse_release_history_limit
    )]
    pub history_limit: Option<u32>,
}

/// Default bound on concurrent apply and prune requests.
pub(crate) const DEFAULT_APPLY_CONCURRENCY: u16 = 8;

#[allow(clippy::too_many_lines)]
pub async fn execute(args: ApplyArgs) -> Result<()> {
    args.common.validation.check_complete(args.append_release)?;
    let preflight = run_render_preflight(RenderPreflightOptions {
        common: &args.common,
        offline: false,
        kube_version: None,
        kube_api_versions: &[],
        context_override: args.context.as_deref(),
        cluster_client_requirement: ClusterClientRequirement::Required,
        discovery_progress: DiscoveryProgress::Announce,
    })
    .await?;

    let mut desired_manifests = preflight.manifests;
    let release = preflight.release;
    let mut duplicates = preflight.duplicates;
    let kube_client = preflight
        .kube_client
        .ok_or_else(|| NylError::Config("Kubernetes client unavailable in online mode".to_string()))?;
    let client = preflight
        .raw_client
        .ok_or_else(|| NylError::Config("Raw Kubernetes client unavailable in online mode".to_string()))?;
    let concurrency = usize::from(args.concurrency);
    let (discovery_mode, discovery_elapsed) = kube_client.initial_discovery();
    let mut timings = vec![PhaseTiming::new(Phase::Discovery(discovery_mode), discovery_elapsed)];

    // A release that renders nothing is still recorded, so the resources of its
    // previous revision are pruned. Without a release identity there is nothing to do.
    let has_release_identity = release.is_some() || args.name.is_some() || args.namespace.is_some();
    if desired_manifests.is_empty() && (args.no_release || !has_release_identity) {
        tracing::info!("No manifests to apply");
        return Ok(());
    }

    let destination_namespace = preflight.destination_namespace.as_deref().or(args.namespace.as_deref());

    let (collapsed, mut scopes) =
        prepare_desired_manifests(&kube_client, desired_manifests, &mut duplicates, destination_namespace).await?;
    desired_manifests = collapsed;

    // Display duplicate resources warning if any
    if !duplicates.is_empty() {
        print_duplicate_warning(&duplicates);
    }

    // 3. Sort resources into apply order (Argo CD's kind order).
    // Sort in place so the recorded release manifest is stored in the same order it
    // is applied (and consistent with `release rollback`, which also stores sorted).
    ResourceOrdering::sort_by_priority(&mut desired_manifests)?;

    tracing::info!("Validating {} manifests", desired_manifests.len());
    let validation_started = Instant::now();
    crate::validation::validate_manifest_input(
        &args.common.validation,
        &preflight.project_config,
        &preflight.project_root,
        preflight
            .resolved_target
            .as_ref()
            .map(|target| target.cluster.metadata.name.as_str()),
        None,
        crate::validation::ManifestValidationInput {
            manifests: &desired_manifests,
            source: &args.common.path,
            provenance: &preflight.provenance,
        },
    )
    .await?;
    timings.push(PhaseTiming::new(Phase::Validation, validation_started.elapsed()));

    // 4. Resolve the release identity before touching the cluster, so a missing
    //    --name/--namespace fails before anything is applied.
    let release_identity = if args.no_release {
        None
    } else {
        Some(resolve_release_identity(release.as_ref(), args.name, args.namespace)?)
    };

    // 5. Create the release namespace before the resources that live in it, and
    //    record the new revision before touching anything else, so an apply that
    //    computed the same revision concurrently fails here rather than after both
    //    changed the cluster. Nyl creates this namespace either way to store release
    //    state.
    let storage = KubernetesReleaseStorage::new(client);
    let renderer = ApplyRenderer::new(&duplicates);
    let recorder = match &release_identity {
        Some((release_name, release_namespace)) => {
            ensure_namespace_exists(&kube_client, release_namespace).await?;
            tracing::info!("Recording release {release_name} in namespace {release_namespace}");
            Some(
                ReleaseRecorder::reserve(
                    &storage,
                    release_name,
                    release_namespace,
                    &desired_manifests,
                    args.append_release,
                    &kube_client,
                    &mut scopes,
                )
                .await?,
            )
        }
        None => None,
    };

    // 6. Apply manifests, printing each outcome as it completes, then complete the
    //    revision, prune resources no longer desired, and supersede the previous one.
    let Some(recorder) = recorder else {
        let apply_started = Instant::now();
        let apply_result = apply_sorted_manifests(&kube_client, &desired_manifests, concurrency, &mut |event| {
            renderer.event(event);
        })
        .await?;
        timings.push(PhaseTiming::new(Phase::Apply, apply_started.elapsed()));
        renderer.summary(&ApplyReport {
            outcomes: &apply_result.outcomes,
            failed_count: apply_result.failed_count,
            prune_failed_count: 0,
            release: None,
            timings: &timings,
        });
        if apply_result.failed_count > 0 {
            return Err(NylError::Other(format!(
                "Apply completed with {} resource(s) failed to apply",
                apply_result.failed_count
            )));
        }
        return Ok(());
    };

    let history_limit = args
        .history_limit
        .unwrap_or_else(|| preflight.project_config.release_history_limit());
    let run = recorder
        .run(
            &kube_client,
            &desired_manifests,
            &mut scopes,
            ApplySettings {
                concurrency,
                history_limit,
                confirm_prune: args.confirm_prune,
            },
            &mut timings,
            &mut |event| renderer.event(event),
        )
        .await?;
    renderer.summary(&run.report(&timings));
    run.error("Apply").map_or(Ok(()), Err)
}

/// Resolve the release name and namespace from the rendered Release or the CLI flags.
fn resolve_release_identity(
    release: Option<&crate::resources::Release>,
    name: Option<String>,
    namespace: Option<String>,
) -> Result<(String, String)> {
    if let Some(release) = release {
        return Ok((release.metadata.name.clone(), release.metadata.namespace.clone()));
    }
    let name =
        name.ok_or_else(|| NylError::Config("No Release resource found. Specify --name and --namespace".to_string()))?;
    let namespace = namespace
        .ok_or_else(|| NylError::Config("No Release resource found. Specify --name and --namespace".to_string()))?;
    Ok((name, namespace))
}

pub(crate) struct ApplyExecutionResult {
    pub(crate) outcomes: Vec<ApplyOutcome>,
    pub(crate) failed_count: usize,
    pub(crate) resource_keys: Vec<ResourceKey>,
}

/// Whether each kind is namespaced, so keys can be compared by [`ObjectIdentity`].
///
/// Kinds the cluster does not know keep the namespace their manifest names.
#[derive(Default)]
pub(crate) struct ObjectScopes {
    /// `None` when the lookup failed, so it is not repeated.
    namespaced: HashMap<(String, String), Option<bool>>,
}

impl ObjectScopes {
    /// Look up the scope of every kind among `keys` not yet known.
    pub(crate) async fn load(&mut self, client: &dyn KubeClient, keys: &[ResourceKey]) {
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

    pub(crate) fn identity(&self, key: &ResourceKey) -> ObjectIdentity {
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
pub(crate) fn keys_to_prune<'a>(
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
pub(crate) async fn collect_live_state(
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

/// Run `op` for every item, at most `concurrency` at a time, reporting each result
/// to `on_done` as it completes. The one bounded executor behind apply and prune.
async fn run_concurrently<T, R, Fut>(
    items: impl IntoIterator<Item = T>,
    concurrency: usize,
    op: impl Fn(T) -> Fut,
    mut on_done: impl FnMut(T, R),
) where
    T: Copy,
    Fut: Future<Output = R>,
{
    let op = &op;
    let mut running = stream::iter(items)
        .map(|item| async move { (item, op(item).await) })
        .buffer_unordered(concurrency);
    while let Some((item, result)) = running.next().await {
        on_done(item, result);
    }
}

/// How long one prune waits in total for deleted resources to terminate before
/// deleting the resources they may depend on.
const PRUNE_TERMINATION_TIMEOUT: Duration = Duration::from_secs(120);
const PRUNE_TERMINATION_POLL: Duration = Duration::from_secs(1);

/// Delete resources no longer desired, at most `concurrency` at a time, and return
/// the keys that failed to delete.
///
/// Resources are deleted in reverse apply order ([`ResourceOrdering::prune_batches`]),
/// dependents first; the resources of one kind are deleted concurrently. Before the
/// next batch, pruning waits until the deleted resources are gone, so a resource
/// whose finalizer needs its controller, CRD, or webhook is finalized before those
/// are deleted. Once [`PRUNE_TERMINATION_TIMEOUT`] has passed, resources still
/// terminating are reported and pruning continues without waiting. A resource whose
/// version is no longer served is deleted through another served version; one whose
/// whole kind is gone ([`KubeClient::is_kind_removed`]), such as a custom resource
/// whose CRD was deleted, is already deleted.
async fn prune_resources<'k>(
    client: &dyn KubeClient,
    mut keys: Vec<&'k ResourceKey>,
    concurrency: usize,
    on_event: &mut ApplyEventSink<'_>,
) -> Vec<&'k ResourceKey> {
    let mut failed = Vec::new();
    if keys.is_empty() {
        return failed;
    }
    on_event(ApplyEvent::PruneStarted { count: keys.len() });
    // One deadline for the whole prune, however many kinds have slow finalizers.
    let deadline = tokio::time::Instant::now() + PRUNE_TERMINATION_TIMEOUT;
    let batches = ResourceOrdering::prune_batches(&mut keys);
    let last = batches.len() - 1;
    for (position, batch) in batches.into_iter().enumerate() {
        let mut deleted = Vec::new();
        run_concurrently(
            keys[batch].iter().copied(),
            concurrency,
            |key: &ResourceKey| async move {
                match client
                    .delete_resource(&key.gvk, key.namespace.as_deref(), &key.name)
                    .await
                {
                    // Objects of a removed kind are gone with it; anything else, such
                    // as an aggregated API that is down, stays recorded and is retried.
                    Err(err) if err.is_api_resource_not_found_error() => match client.is_kind_removed(&key.gvk).await {
                        Ok(true) => Ok(()),
                        _ => Err(err),
                    },
                    result => result,
                }
            },
            |key, result| {
                on_event(ApplyEvent::Pruned { key, result: &result });
                if result.is_ok() {
                    deleted.push(key);
                } else {
                    failed.push(key);
                }
            },
        )
        .await;
        if position < last {
            wait_until_terminated(client, deleted, concurrency, deadline, on_event).await;
        }
    }
    on_event(ApplyEvent::PruneFinished);
    failed
}

/// Wait until every deleted resource is gone from the cluster, or the timeout passes.
async fn wait_until_terminated(
    client: &dyn KubeClient,
    mut remaining: Vec<&ResourceKey>,
    concurrency: usize,
    deadline: tokio::time::Instant,
    on_event: &mut ApplyEventSink<'_>,
) {
    let mut announced = false;
    loop {
        let mut terminating = Vec::new();
        run_concurrently(
            remaining.iter().copied(),
            concurrency,
            |key: &ResourceKey| async move {
                match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
                    Ok(found) => Ok(found.is_some()),
                    // Gone with its kind, or unreachable while its aggregated API is down.
                    Err(err) if err.is_api_resource_not_found_error() => {
                        client.is_kind_removed(&key.gvk).await.map(|removed| !removed)
                    }
                    Err(err) => Err(err),
                }
            },
            |key, result| match result {
                Ok(true) => terminating.push(key),
                Ok(false) => {}
                // Keep waiting on transient errors; the deadline bounds the wait.
                Err(err) => {
                    tracing::debug!(resource = %key, error = %err, "Checking pruned resource failed");
                    terminating.push(key);
                }
            },
        )
        .await;
        remaining = terminating;
        if remaining.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            on_event(ApplyEvent::PruneWaitTimedOut { keys: &remaining });
            return;
        }
        if !announced {
            on_event(ApplyEvent::PruneWaiting { count: remaining.len() });
            announced = true;
        }
        tokio::time::sleep(PRUNE_TERMINATION_POLL).await;
    }
}

/// Resolve missing namespaces and collapse documents describing one object, the
/// shared preparation of rendered manifests for `apply` and `diff`, so a diff shows
/// what apply applies.
///
/// Namespace resolution and API versions can make distinct documents describe one
/// object; the last is kept, so the applied and recorded manifests agree.
pub(crate) async fn prepare_desired_manifests(
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
pub(crate) async fn collapse_rendered_duplicates(
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
pub(crate) fn collapse_duplicate_objects(
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
pub(crate) struct ReleaseRecorder<'s> {
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
    pub(crate) async fn reserve(
        storage: &'s dyn ReleaseStorage,
        release_name: &str,
        release_namespace: &str,
        desired_manifests: &[serde_json::Value],
        append_release: bool,
        client: &dyn KubeClient,
        scopes: &mut ObjectScopes,
    ) -> Result<Self> {
        let revisions = storage.list_revisions(release_name, release_namespace).await?;
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

    /// Apply `manifests` and record the outcome, recording the phase timings.
    ///
    /// If anything fails or the user interrupts (Ctrl-C) before the revision is
    /// completed, the reserved revision is marked `Failed`, so it never stays
    /// `Rendered` and later applies treat its keys as partially applied.
    pub(crate) async fn run(
        self,
        client: &dyn KubeClient,
        manifests: &[serde_json::Value],
        scopes: &mut ObjectScopes,
        settings: ApplySettings,
        timings: &mut Vec<PhaseTiming>,
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
            let apply = apply_sorted_manifests(client, manifests, settings.concurrency, &mut track).await?;
            timings.push(PhaseTiming::new(Phase::Apply, apply_started.elapsed()));

            let release_started = Instant::now();
            let (release, prune_failed_count) = self
                .complete(client, &apply, scopes, settings, &mut track, &completed)
                .await?;
            timings.push(PhaseTiming::new(Phase::Release, release_started.elapsed()));
            Ok(ReleaseRun {
                apply,
                release,
                prune_failed_count,
            })
        };
        let result = tokio::select! {
            result = work => result,
            Ok(()) = tokio::signal::ctrl_c() => Err(NylError::Other("Interrupted".to_string())),
        };

        if let Err(err) = &result {
            if !completed.load(AtomicOrdering::SeqCst) {
                // Record only what this apply wrote, so a later prune never deletes
                // objects it did not create. A second Ctrl-C exits without waiting.
                let failed = Self::failed_revision(reserved, applied_keys, err);
                tokio::select! {
                    () = Self::record_failed(storage, &failed) => {}
                    Ok(()) = tokio::signal::ctrl_c() => std::process::exit(130),
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
    async fn complete(
        self,
        client: &dyn KubeClient,
        apply_result: &ApplyExecutionResult,
        scopes: &mut ObjectScopes,
        settings: ApplySettings,
        on_event: &mut ApplyEventSink<'_>,
        completed: &AtomicBool,
    ) -> Result<(ReleaseState, usize)> {
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
                let to_prune = keys_to_prune(&live_keys, &release.resource_keys, scopes);
                let (to_prune, awaiting) =
                    filter_by_prune_option(client, to_prune, settings.confirm_prune, on_event).await;
                // Resources awaiting confirmation stay tracked, so a confirmed apply prunes them.
                release.resource_keys.extend(awaiting.into_iter().cloned());
                let failed = prune_resources(client, to_prune, settings.concurrency, on_event).await;
                prune_failed_count = failed.len();
                release.resource_keys.extend(failed.into_iter().cloned());
            }
        }

        storage.complete_release(&release).await?;
        completed.store(true, AtomicOrdering::SeqCst);
        if let Some(previous) = &superseded {
            mark_superseded(storage, previous).await;
        }
        prune_release_history(storage, &release, settings.history_limit).await;
        Ok((release, prune_failed_count))
    }
}

/// How a release is applied and recorded.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ApplySettings {
    /// Maximum number of resources applied or pruned at the same time.
    pub(crate) concurrency: usize,
    /// Revisions to keep after a successful apply; 0 keeps every revision.
    pub(crate) history_limit: u32,
    /// `--confirm-prune`: prune resources whose sync option is `Prune=confirm`.
    pub(crate) confirm_prune: bool,
}

/// Split the resources to prune by their live Argo CD `Prune` sync option
/// ([`prune_decision`]): returns those to delete and those awaiting
/// confirmation, and reports every resource kept. A resource that cannot be read
/// is pruned as before; deleting it reports the error.
async fn filter_by_prune_option<'k>(
    client: &dyn KubeClient,
    keys: Vec<&'k ResourceKey>,
    confirmed: bool,
    on_event: &mut ApplyEventSink<'_>,
) -> (Vec<&'k ResourceKey>, Vec<&'k ResourceKey>) {
    let mut prune = Vec::new();
    let mut awaiting = Vec::new();
    for key in keys {
        let live = client
            .get_resource(&key.gvk, key.namespace.as_deref(), &key.name)
            .await
            .ok()
            .flatten();
        let decision = prune_decision(
            live.as_ref().and_then(|object| object.metadata.annotations.as_ref()),
            confirmed,
        );
        match decision {
            PruneDecision::Prune => prune.push(key),
            PruneDecision::Retain => on_event(ApplyEvent::PruneKept { key, decision }),
            PruneDecision::AwaitConfirmation => {
                on_event(ApplyEvent::PruneKept { key, decision });
                awaiting.push(key);
            }
        }
    }
    (prune, awaiting)
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
pub(crate) struct ReleaseRun {
    pub(crate) apply: ApplyExecutionResult,
    pub(crate) release: ReleaseState,
    /// Resources that failed to prune; they stay recorded for the next apply.
    pub(crate) prune_failed_count: usize,
}

impl ReleaseRun {
    /// The summary of this run.
    pub(crate) fn report<'a>(&'a self, timings: &'a [PhaseTiming]) -> ApplyReport<'a> {
        ApplyReport {
            outcomes: &self.apply.outcomes,
            failed_count: self.apply.failed_count,
            prune_failed_count: self.prune_failed_count,
            release: Some(&self.release),
            timings,
        }
    }

    /// The command's error when anything failed to apply or prune.
    pub(crate) fn error(&self, command: &str) -> Option<NylError> {
        let (failed, pruned) = (self.apply.failed_count, self.prune_failed_count);
        (failed > 0 || pruned > 0).then(|| {
            NylError::Other(format!(
                "{command} completed with {failed} resource(s) failed to apply and {pruned} failed to prune"
            ))
        })
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
pub(crate) async fn apply_sorted_manifests(
    client: &dyn KubeClient,
    manifests: &[serde_json::Value],
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
            |index| apply_manifest(client, &manifests[index]),
            |index, result| {
                on_event(ApplyEvent::Applied {
                    key: &keys[index],
                    result: &result,
                });
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
pub(crate) fn manifests_to_yaml(manifests: &[serde_json::Value]) -> Result<String> {
    let mut yaml_parts = Vec::new();

    for manifest in manifests {
        let yaml = crate::yaml::serialize_yaml_value(manifest).map_err(NylError::YamlEmit)?;
        yaml_parts.push(yaml);
    }

    Ok(yaml_parts.join("---\n"))
}

/// Apply a single manifest
async fn apply_manifest(client: &dyn KubeClient, manifest: &serde_json::Value) -> Result<ApplyOutcome> {
    // Convert JSON to DynamicObject
    let resource: DynamicObject = serde_json::from_value(manifest.clone())?;

    // Apply using client
    client.apply_resource(&resource, "nyl", false).await
}

/// Print a warning trace about duplicate resources
pub(crate) fn print_duplicate_warning(duplicates: &HashMap<ResourceKey, usize>) {
    if duplicates.is_empty() {
        return;
    }

    let total_unique = duplicates.len();
    let total_ignored: usize = duplicates.values().map(|count| count - 1).sum();

    tracing::warn!(
        "Found {} unique resources with duplicates ({} total duplicates ignored, keeping last occurrence)",
        total_unique,
        total_ignored
    );
}

/// Ensure a namespace exists, creating it if necessary
pub(crate) async fn ensure_namespace_exists(client: &dyn KubeClient, namespace: &str) -> Result<()> {
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
        client.apply_resource(&ns_resource, "nyl", false).await?;

        tracing::info!("Created namespace '{}'", namespace);

        Ok(())
    }
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

        async fn delete_resource(&self, gvk: &GroupVersionKind, namespace: Option<&str>, name: &str) -> Result<()> {
            self.events.lock().unwrap().push(format!("delete-start {name}"));
            tokio::time::sleep(self.latency).await;
            self.events.lock().unwrap().push(format!("delete-end {name}"));
            if name == self.failing {
                return Err(NylError::Other("failed calling webhook".to_string()));
            }
            if gvk.kind == self.removed_kind {
                return Err(NylError::ApiResourceNotFound(gvk.kind.clone()));
            }
            self.inner.delete_resource(gvk, namespace, name).await
        }

        async fn get_normalized_resource(&self, resource: &DynamicObject, manager: &str) -> Result<DynamicObject> {
            self.inner.get_normalized_resource(resource, manager).await
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
        let result = apply_sorted_manifests(&client, &manifests, 8, &mut |_| {
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

    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_concurrency_one_is_serial() {
        let client = LatencyClient::new("");
        let manifests = config_maps(4);

        let started = tokio::time::Instant::now();
        apply_sorted_manifests(&client, &manifests, 1, &mut |_| {})
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

        apply_sorted_manifests(&client, &manifests, 8, &mut |_| {})
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

        apply_sorted_manifests(&client, &manifests, 8, &mut |_| {})
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
        let result = apply_sorted_manifests(&client, &manifests, 8, &mut |event| {
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
        let mut release = crate::cli::commands::release::rollback::tests::make_release("app", "app", revision, status);
        release.resource_keys = keys;
        release
    }

    /// The recorder records the revision before the apply, prunes what the previous
    /// revision had and the new one does not, keeps keys that failed to prune so the
    /// next apply retries them, and supersedes the previous revision.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_reserves_prunes_and_keeps_failed_prunes() {
        let storage = crate::cli::commands::release::rollback::tests::MockReleaseStorage::new();
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

        let recorder = ReleaseRecorder::reserve(&storage, "app", "app", &manifests, false, &client, &mut scopes)
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
                &mut Vec::new(),
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
            let storage = crate::cli::commands::release::rollback::tests::MockReleaseStorage::new();
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
            let recorder = ReleaseRecorder::reserve(&storage, "app", "app", &[], false, &client, &mut scopes)
                .await
                .unwrap();
            let settings = ApplySettings {
                confirm_prune,
                ..settings(0)
            };
            let release = recorder
                .run(&client, &[], &mut scopes, settings, &mut Vec::new(), &mut |_| {})
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

    /// A release that renders nothing is still recorded and prunes everything the
    /// previous revision deployed.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_empty_release_prunes_previous_resources() {
        let storage = crate::cli::commands::release::rollback::tests::MockReleaseStorage::new();
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

        let recorder = ReleaseRecorder::reserve(&storage, "app", "app", &[], false, &client, &mut scopes)
            .await
            .unwrap();
        let release = recorder
            .run(&client, &[], &mut scopes, settings(0), &mut Vec::new(), &mut |_| {})
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
        let storage = crate::cli::commands::release::rollback::tests::MockReleaseStorage::new();
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
        )
        .await;

        assert!(result.is_err());
        assert_eq!(storage.list_revisions("app", "app").await.unwrap(), vec![1]);
    }

    /// An apply that fails before its revision is completed marks the reserved
    /// revision Failed instead of leaving it Rendered.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_marks_revision_failed_when_apply_errors() {
        let storage = crate::cli::commands::release::rollback::tests::MockReleaseStorage::new();
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

        let recorder = ReleaseRecorder::reserve(&storage, "app", "app", &manifests, false, &client, &mut scopes)
            .await
            .unwrap();
        let result = recorder
            .run(
                &client,
                &manifests,
                &mut scopes,
                settings(0),
                &mut Vec::new(),
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

    fn settings(history_limit: u32) -> ApplySettings {
        ApplySettings {
            concurrency: 8,
            history_limit,
            confirm_prune: false,
        }
    }

    /// A deployed apply deletes the oldest revisions beyond the history limit.
    #[tokio::test(start_paused = true)]
    async fn test_release_recorder_enforces_history_limit_after_deploy() {
        let storage = crate::cli::commands::release::rollback::tests::MockReleaseStorage::new();
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

        let recorder = ReleaseRecorder::reserve(&storage, "app", "app", &[], false, &client, &mut scopes)
            .await
            .unwrap();
        recorder
            .run(&client, &[], &mut scopes, settings(2), &mut Vec::new(), &mut |_| {})
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
}
