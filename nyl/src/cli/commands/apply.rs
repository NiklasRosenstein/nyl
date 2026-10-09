use chrono::Utc;
use clap::Args;
use futures::stream::{self, StreamExt};
use kube::api::DynamicObject;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::{
    cli::{
        apply_output::{ApplyEvent, ApplyEventSink, ApplyRenderer, ApplyReport, PhaseTiming},
        commands::render::{run_render_preflight, ClusterClientRequirement, RenderOptions, RenderPreflightOptions},
        namespace_resolution::{adjust_duplicate_keys_for_namespace_resolution, resolve_manifest_namespaces},
    },
    kubernetes::{
        ApplyOutcome, GroupVersionKind, KubeClient, KubernetesReleaseStorage, ReleaseState, ReleaseStatus,
        ReleaseStorage, ResourceKey, ResourceOrdering,
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
        resolve_namespaces: false,
        release_namespace_hint: None,
        adjust_duplicate_keys: false,
        discovery_progress: crate::cli::commands::render::DiscoveryProgress::Announce,
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
    let mut timings = vec![PhaseTiming {
        phase: format!("discovery ({discovery_mode})"),
        elapsed: discovery_elapsed,
    }];

    if desired_manifests.is_empty() {
        tracing::info!("No manifests to apply");
        return Ok(());
    }

    let release_namespace_hint = release
        .as_ref()
        .map(|release| release.metadata.namespace.as_str())
        .or(args.namespace.as_deref());

    // Resolve missing namespaces for namespaced resources.
    resolve_manifest_namespaces(&kube_client, &mut desired_manifests, release_namespace_hint).await?;
    duplicates =
        adjust_duplicate_keys_for_namespace_resolution(&kube_client, &duplicates, release_namespace_hint).await?;
    // Namespace resolution and API versions can make distinct documents describe one
    // object; keep the last, so the applied and recorded manifests agree.
    desired_manifests = collapse_duplicate_objects(desired_manifests, &mut duplicates)?;

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
    timings.push(PhaseTiming {
        phase: "validation".to_string(),
        elapsed: validation_started.elapsed(),
    });

    // 4. Resolve the release identity before touching the cluster, so a missing
    //    --name/--namespace fails before anything is applied.
    let release_identity = if args.no_release {
        None
    } else {
        Some(resolve_release_identity(release.as_ref(), args.name, args.namespace)?)
    };

    // 5. Create the release namespace before the resources that live in it.
    //    Nyl creates this namespace either way to store release state; creating
    //    it after the apply left the first run failing for every namespaced
    //    resource and succeeding only on a retry.
    if let Some((_, release_namespace)) = &release_identity {
        ensure_namespace_exists(&kube_client, release_namespace).await?;
    }

    // 6. Apply manifests, printing each outcome as it completes.
    let renderer = ApplyRenderer::new(&duplicates);
    let apply_started = Instant::now();
    let apply_result = apply_sorted_manifests(&kube_client, &desired_manifests, concurrency, &mut |event| {
        renderer.event(event);
    })
    .await?;
    timings.push(PhaseTiming {
        phase: "apply".to_string(),
        elapsed: apply_started.elapsed(),
    });

    if args.no_release {
        renderer.summary(&ApplyReport {
            outcomes: &apply_result.outcomes,
            failed_count: apply_result.failed_count,
            release: None,
            timings: &timings,
        });
        if apply_result.failed_count > 0 {
            return Err(NylError::Other(format!(
                "Apply completed with {} error(s)",
                apply_result.failed_count
            )));
        }
        return Ok(());
    }

    let (release_name, release_namespace) =
        release_identity.expect("a release identity is resolved unless --no-release is set");

    // 7. Initialize release storage
    let storage = KubernetesReleaseStorage::new(client);

    // 8-12. Record the new revision, mark the previous one superseded, and prune.
    tracing::info!("Recording release {release_name} in namespace {release_namespace}");
    let release_started = Instant::now();
    let release = apply_and_record_release(
        &storage,
        &kube_client,
        &desired_manifests,
        &apply_result,
        &release_name,
        &release_namespace,
        args.append_release,
        concurrency,
        &mut |event| renderer.event(event),
    )
    .await?;
    timings.push(PhaseTiming {
        phase: "release".to_string(),
        elapsed: release_started.elapsed(),
    });

    // 13. Print summary
    renderer.summary(&ApplyReport {
        outcomes: &apply_result.outcomes,
        failed_count: apply_result.failed_count,
        release: Some(&release),
        timings: &timings,
    });

    if apply_result.failed_count > 0 {
        return Err(NylError::Other(format!(
            "Apply completed with {} error(s)",
            apply_result.failed_count
        )));
    }

    Ok(())
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

/// Determine which previously-live resources are no longer present in `current_keys`
/// and should therefore be pruned from the cluster.
pub(crate) fn keys_to_prune<'a>(
    live_keys: &'a HashSet<ResourceKey>,
    current_keys: &HashSet<&ResourceKey>,
) -> Vec<&'a ResourceKey> {
    live_keys.iter().filter(|k| !current_keys.contains(k)).collect()
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

/// Delete resources no longer desired, at most `concurrency` at a time.
///
/// Like Argo CD, pruned resources are deleted concurrently without kind order,
/// except that APIServices and admission webhook configurations
/// ([`ResourceOrdering::is_prune_last`]) are deleted after all others, so they keep
/// serving and admitting requests while the resources related to them are deleted.
async fn prune_resources(
    client: &dyn KubeClient,
    keys: Vec<&ResourceKey>,
    concurrency: usize,
    on_event: &mut ApplyEventSink<'_>,
) {
    if keys.is_empty() {
        return;
    }
    on_event(ApplyEvent::PruneStarted { count: keys.len() });
    let (last, first): (Vec<&ResourceKey>, Vec<&ResourceKey>) = keys
        .into_iter()
        .partition(|key| ResourceOrdering::is_prune_last(&key.gvk));
    for step in [first, last] {
        let mut deletions = stream::iter(step)
            .map(|key| async move {
                let result = client
                    .delete_resource(&key.gvk, key.namespace.as_deref(), &key.name)
                    .await;
                (key, result)
            })
            .buffer_unordered(concurrency);
        while let Some((key, result)) = deletions.next().await {
            on_event(ApplyEvent::Pruned { key, result: &result });
        }
    }
    on_event(ApplyEvent::PruneFinished);
}

/// Collapse manifests that describe the same Kubernetes object, keeping the last.
///
/// Objects are identified by API group, kind, namespace, and name, not API version:
/// two versions of one object are one object, and applying both concurrently would
/// race. The surviving document takes the position of the first occurrence, as in
/// render-time deduplication, and `duplicates` is updated with the total occurrence
/// count under the surviving document's key.
pub(crate) fn collapse_duplicate_objects(
    manifests: Vec<serde_json::Value>,
    duplicates: &mut HashMap<ResourceKey, usize>,
) -> Result<Vec<serde_json::Value>> {
    type ObjectIdentity = (String, String, Option<String>, String);

    let mut position: HashMap<ObjectIdentity, usize> = HashMap::new();
    let mut collapsed: Vec<(serde_json::Value, Vec<ResourceKey>)> = Vec::new();
    for manifest in manifests {
        let key = ResourceKey::from_json_value(&manifest)?;
        let identity = (
            key.gvk.group.clone(),
            key.gvk.kind.clone(),
            key.namespace.clone(),
            key.name.clone(),
        );
        if let Some(&index) = position.get(&identity) {
            tracing::warn!("Duplicate resource: {} (keeping last occurrence)", key);
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
/// manifest documents for resources that are not part of the current set, so the
/// stored manifest matches the merged `resource_keys`. This keeps `release rollback`
/// faithful: rolling back to an appended revision re-applies the complete desired
/// state rather than only the newly-rendered resources.
fn merge_append_manifest(desired_manifests: &[serde_json::Value], previous_manifest: &str) -> Result<String> {
    // Dedup against the keys present in the current manifest (all rendered docs),
    // not just successfully-applied ones — otherwise a current doc that failed to
    // apply would be carried over again from the previous manifest, producing a
    // duplicate document for the same resource.
    let current_keys: HashSet<ResourceKey> = desired_manifests
        .iter()
        .map(ResourceKey::from_json_value)
        .collect::<Result<_>>()?;

    let prev_docs = crate::yaml::parse_yaml_documents_k8s_compatible(previous_manifest)
        .map_err(|e| NylError::Config(format!("Failed to parse previous release manifest: {}", e)))?;
    let mut merged_docs: Vec<serde_json::Value> = desired_manifests.to_vec();
    for doc in prev_docs {
        let doc_key = ResourceKey::from_json_value(&doc)?;
        if !current_keys.contains(&doc_key) {
            merged_docs.push(doc);
        }
    }
    ResourceOrdering::sort_by_priority(&mut merged_docs)?;
    manifests_to_yaml(&merged_docs)
}

/// Record a freshly applied set of manifests as a new release revision.
///
/// This is the shared apply+record path used by both `apply` and `release rollback`:
/// it computes the next revision number, builds and saves the [`ReleaseState`]
/// (carrying the full rendered manifest), optionally merges with the previous
/// revision when `append_release` is set, marks the previous revision
/// [`ReleaseStatus::Superseded`], and prunes resources that existed in the previous
/// revision but not the new one. Returns the recorded [`ReleaseState`] so callers
/// can print a summary.
///
/// The release namespace must already exist; callers create it before applying
/// so namespaced resources in it succeed on the first run.
// Shared by `apply` and `release rollback`, which each pass their own release
// identity, mode, and concurrency; a parameter struct would only restate them.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) async fn apply_and_record_release(
    storage: &KubernetesReleaseStorage,
    kube_client: &dyn KubeClient,
    desired_manifests: &[serde_json::Value],
    apply_result: &ApplyExecutionResult,
    release_name: &str,
    release_namespace: &str,
    append_release: bool,
    concurrency: usize,
    on_event: &mut ApplyEventSink<'_>,
) -> Result<ReleaseState> {
    // Determine next revision number
    let revisions = storage.list_revisions(release_name, release_namespace).await?;
    let next_revision = revisions.iter().max().map_or(1, |r| r + 1);

    // Create initial release state
    let mut release = ReleaseState {
        release_name: release_name.to_string(),
        release_namespace: release_namespace.to_string(),
        revision: next_revision,
        resource_keys: apply_result.resource_keys.clone(),
        manifest: manifests_to_yaml(desired_manifests)?,
        status: ReleaseStatus::Rendered,
        rendered_at: Utc::now(),
        applied_at: None,
        error: None,
    };

    // Append-release mode: merge with previous release
    let mut appended_to = None;
    if append_release && next_revision > 1 {
        // Fetch previous release
        if let Ok(Some(previous_release)) = storage
            .get_release(release_name, release_namespace, next_revision - 1)
            .await
        {
            // Validate that previous release was successfully deployed
            // Only Deployed releases have complete resource sets safe to merge from
            if previous_release.status != ReleaseStatus::Deployed {
                return Err(NylError::Config(format!(
                    "Cannot use --append-release when previous release (revision {}) is in {:?} state. \
                     The previous release must be in Deployed state to safely merge resources.",
                    previous_release.revision, previous_release.status
                )));
            }

            // Use HashSet for deduplication
            let current_keys: HashSet<_> = release.resource_keys.iter().cloned().collect();

            // Add previous resources not in current set
            let mut merged_keys = Vec::new();
            let mut added_from_previous = 0;
            for prev_key in &previous_release.resource_keys {
                if !current_keys.contains(prev_key) {
                    merged_keys.push(prev_key.clone());
                    added_from_previous += 1;
                }
            }

            // Add all current resources (current wins on duplicates)
            merged_keys.extend(release.resource_keys.clone());

            // Merge the stored manifest too, so the recorded manifest matches the
            // merged resource set. Without this, the manifest would contain only the
            // newly-rendered resources while resource_keys tracks the union — which
            // breaks `release rollback` (it would re-apply only the new resources and
            // prune the carried-over ones).
            release.manifest = merge_append_manifest(desired_manifests, &previous_release.manifest)?;

            // Calculate overlap for better logging
            let overlap = previous_release.resource_keys.len() - added_from_previous;
            if overlap > 0 {
                tracing::info!(
                    "Append-release mode: merged {} from previous + {} current ({} overlap, {} total)",
                    added_from_previous,
                    release.resource_keys.len(),
                    overlap,
                    merged_keys.len()
                );
            } else {
                tracing::info!(
                    "Append-release mode: merged {} from previous + {} current ({} total)",
                    added_from_previous,
                    release.resource_keys.len(),
                    merged_keys.len()
                );
            }

            release.resource_keys = merged_keys;
            appended_to = Some(previous_release);
        } else {
            tracing::warn!(
                "Append-release mode: no previous release found (revision {}), treating as initial apply",
                next_revision - 1
            );
        }
    }

    // Update release status based on apply outcome
    if apply_result.failed_count == 0 {
        release.status = ReleaseStatus::Deployed;
        release.applied_at = Some(Utc::now());
    } else {
        release.status = ReleaseStatus::Failed;
        release.error = Some(format!("{} resource(s) failed to apply", apply_result.failed_count));
    }

    storage.save_release(&release).await?;

    // Supersede the previous revision and prune resources no longer desired.
    if release.status == ReleaseStatus::Deployed && next_revision > 1 {
        if append_release {
            // Append mode validated that the immediately previous revision is Deployed
            // and does not prune; just mark it superseded.
            if let Some(previous) = &appended_to {
                mark_superseded(storage, previous).await;
            }
        } else {
            // Reconcile against the resources currently live on the cluster, not just
            // the numerically previous secret. The live state is the most recent
            // Deployed revision's resources plus anything partially applied by Failed
            // revisions after it (Failed revisions never prune). Pruning against only
            // `next_revision - 1` would orphan resources from an older Deployed revision
            // when the immediately previous revision Failed.
            let (superseded, live_keys) =
                collect_live_state(storage, release_name, release_namespace, &revisions, next_revision).await?;

            if let Some(previous) = &superseded {
                mark_superseded(storage, previous).await;
            }

            let current_keys: HashSet<&ResourceKey> = release.resource_keys.iter().collect();
            prune_resources(
                kube_client,
                keys_to_prune(&live_keys, &current_keys),
                concurrency,
                on_event,
            )
            .await;
        }
    }

    Ok(release)
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
    let keys: Vec<ResourceKey> = manifests
        .iter()
        .map(ResourceKey::from_json_value)
        .collect::<Result<_>>()?;
    let batches = ResourceOrdering::apply_batches(manifests);
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

        let mut applies = stream::iter(batch)
            .map(|index| async move { (index, apply_manifest(client, &manifests[index]).await) })
            .buffer_unordered(concurrency);
        while let Some((index, result)) = applies.next().await {
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
        }
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
    let registers_kinds = matches!(
        (key.gvk.group.as_str(), key.gvk.kind.as_str()),
        ("apiextensions.k8s.io", "CustomResourceDefinition") | ("apiregistration.k8s.io", "APIService")
    );
    if !registers_kinds {
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
fn print_duplicate_warning(duplicates: &HashMap<ResourceKey, usize>) {
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
    use crate::kubernetes::GroupVersionKind;
    use kube::api::DynamicObject;
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
        let current_keys: HashSet<&ResourceKey> = current.iter().collect();

        let to_prune = keys_to_prune(&live, &current_keys);
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

        let merged = merge_append_manifest(&current, &previous).unwrap();
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
            self.inner.delete_resource(gvk, namespace, name).await
        }

        async fn get_normalized_resource(&self, resource: &DynamicObject, manager: &str) -> Result<DynamicObject> {
            self.inner.get_normalized_resource(resource, manager).await
        }

        async fn refresh_discovery_until_available(&self, required: &[GroupVersionKind]) -> Result<()> {
            tokio::time::sleep(self.latency).await;
            let kinds: Vec<&str> = required.iter().map(|gvk| gvk.kind.as_str()).collect();
            self.events.lock().unwrap().push(format!("refresh {}", kinds.join(",")));
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

    /// Pruning deletes concurrently, but APIServices and admission webhooks are
    /// deleted only after the resources related to them are gone.
    #[tokio::test(start_paused = true)]
    async fn test_prune_resources_deletes_api_services_and_webhooks_last() {
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
        ];

        let started = tokio::time::Instant::now();
        let mut pruned = Vec::new();
        prune_resources(&client, keys.iter().collect(), 8, &mut |event| {
            if let ApplyEvent::Pruned { key, result: Ok(()) } = event {
                pruned.push(key.name.clone());
            }
        })
        .await;

        assert_eq!(started.elapsed(), client.latency * 2);
        assert_eq!(pruned.len(), 4);
        for last in ["hook", "api"] {
            for first in ["backend", "svc"] {
                assert!(
                    client.position(&format!("delete-end {first}")) < client.position(&format!("delete-start {last}"))
                );
            }
        }
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

        let collapsed = collapse_duplicate_objects(manifests, &mut duplicates).unwrap();

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
        let current_keys: HashSet<&ResourceKey> = current.iter().collect();

        assert!(keys_to_prune(&live, &current_keys).is_empty());
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
        let appended = merge_append_manifest(&[updated.clone()], &stored).unwrap();
        let restored = crate::yaml::parse_yaml_documents_k8s_compatible(&appended).unwrap();
        assert_eq!(restored.len(), manifests.len());
        assert!(restored.contains(&updated));
        for manifest in &manifests[1..] {
            assert!(restored.contains(manifest));
        }
    }
}
