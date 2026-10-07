use chrono::Utc;
use clap::Args;
use futures::stream::{self, StreamExt};
use kube::api::DynamicObject;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::{
    cli::{
        commands::render::{run_render_preflight, ClusterClientRequirement, RenderOptions, RenderPreflightOptions},
        namespace_resolution::{adjust_duplicate_keys_for_namespace_resolution, resolve_manifest_namespaces},
    },
    kubernetes::{
        ApplyOutcome, GroupVersionKind, KubeClient, KubeRsClient, KubernetesReleaseStorage, ReleaseState,
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

    /// Apply resources without creating release revisions or pruning.
    #[arg(long, conflicts_with_all = ["append_release", "name", "namespace"])]
    pub no_release: bool,

    /// Maximum number of resources applied or pruned at the same time.
    ///
    /// Resources are applied in waves of equal ordering priority (Namespaces,
    /// CRDs, ServiceAccounts, roles, role bindings, configuration, Services,
    /// workloads, other, APIServices, admission webhooks); a wave completes
    /// before the next one starts. Use 1 to apply serially.
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
    let mut timings = PhaseTimings::default();
    let (discovery_mode, discovery_elapsed) = kube_client.initial_discovery();
    timings.record(format!("discovery ({discovery_mode})"), discovery_elapsed);

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

    // Display duplicate resources warning if any
    if !duplicates.is_empty() {
        print_duplicate_warning(&duplicates);
    }

    // 3. Sort resources by priority (Namespace → CRD → RBAC → Config → Workload).
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
    timings.record("validation", validation_started.elapsed());

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
    let apply_started = Instant::now();
    let apply_result = apply_sorted_manifests(&kube_client, &desired_manifests, concurrency, &mut |key, result| {
        print_apply_result(key, result, &duplicates);
    })
    .await?;
    timings.record("apply", apply_started.elapsed());

    if args.no_release {
        print_apply_summary(&apply_result.outcomes, None, &duplicates, apply_result.failed_count);
        timings.print();
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
    )
    .await?;
    timings.record("release", release_started.elapsed());

    // 13. Print summary
    print_apply_summary(
        &apply_result.outcomes,
        Some(&release),
        &duplicates,
        apply_result.failed_count,
    );
    timings.print();

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

/// Wall-clock duration of each apply phase, printed after the summary so slow
/// phases are attributable.
#[derive(Default)]
struct PhaseTimings(Vec<(String, Duration)>);

impl PhaseTimings {
    fn record(&mut self, phase: impl Into<String>, elapsed: Duration) {
        self.0.push((phase.into(), elapsed));
    }

    fn print(&self) {
        let parts: Vec<String> = self
            .0
            .iter()
            .map(|(phase, elapsed)| format!("{phase} {:.2}s", elapsed.as_secs_f64()))
            .collect();
        println!("{}", format!("Timings: {}", parts.join(", ")).bright_black());
    }
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

/// Mark an already-loaded revision superseded. Best-effort, like the rest of
/// superseding: the new revision is already recorded.
async fn mark_superseded(storage: &dyn ReleaseStorage, mut previous: ReleaseState) {
    previous.status = ReleaseStatus::Superseded;
    previous.error = None;
    if let Err(err) = storage.update_release(&previous).await {
        tracing::warn!(
            "Failed to mark release {} revision {} superseded: {}",
            previous.release_name,
            previous.revision,
            err
        );
    }
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
    kube_client: &KubeRsClient,
    desired_manifests: &[serde_json::Value],
    apply_result: &ApplyExecutionResult,
    release_name: &str,
    release_namespace: &str,
    append_release: bool,
    concurrency: usize,
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
            if let Some(previous) = appended_to {
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

            if let Some(previous) = superseded {
                mark_superseded(storage, previous).await;
            }

            let current_keys: HashSet<&ResourceKey> = release.resource_keys.iter().collect();
            let mut to_prune = keys_to_prune(&live_keys, &current_keys);
            if !to_prune.is_empty() {
                to_prune.sort_by_key(|key| key.to_string());
                println!("\nPruning {} resources...", to_prune.len());
                let mut deletions = stream::iter(to_prune)
                    .map(|key| async move {
                        let result = kube_client
                            .delete_resource(&key.gvk, key.namespace.as_deref(), &key.name)
                            .await;
                        (key, result)
                    })
                    .buffer_unordered(concurrency);
                while let Some((key, result)) = deletions.next().await {
                    match result {
                        Ok(()) => println!("  ✓ Deleted {}", key),
                        Err(e) => println!("  ✗ Failed to delete {}: {}", key, e),
                    }
                }
                println!();
            }
        }
    }

    Ok(release)
}

/// Apply manifests in [`ResourceOrdering::apply_waves`], at most `concurrency` at a time.
///
/// Every wave finishes before the next starts, so Namespaces precede the resources
/// in them and CustomResourceDefinitions precede their custom resources; after a
/// wave that applied a CRD, discovery is refreshed before the next wave. A failed
/// resource does not stop the others, matching serial apply. When several manifests
/// share a resource key, only the last is applied, as the duplicate warning states;
/// concurrent server-side applies of both would race. `on_result` is called
/// as each resource completes so progress is visible before the whole apply ends;
/// the returned outcomes and keys keep manifest order regardless of completion order.
pub(crate) async fn apply_sorted_manifests(
    client: &dyn KubeClient,
    manifests: &[serde_json::Value],
    concurrency: usize,
    on_result: &mut (dyn FnMut(&ResourceKey, &Result<ApplyOutcome>) + Send),
) -> Result<ApplyExecutionResult> {
    let keys: Vec<ResourceKey> = manifests
        .iter()
        .map(ResourceKey::from_json_value)
        .collect::<Result<_>>()?;
    let waves = ResourceOrdering::apply_waves(manifests);
    let last_occurrence: HashMap<&ResourceKey, usize> = keys.iter().enumerate().map(|(i, key)| (key, i)).collect();
    let is_applied = |index: &usize| last_occurrence[&keys[*index]] == *index;
    tracing::info!(
        "Applying {} resources in {} waves (up to {} at a time)",
        manifests.len(),
        waves.len(),
        concurrency
    );

    let mut results: Vec<Option<Result<ApplyOutcome>>> = (0..manifests.len()).map(|_| None).collect();
    let mut crd_applied = false;
    let mut discovery_refreshed = false;

    for wave in waves {
        // If a CRD was applied in an earlier wave, refresh the discovery cache before
        // applying the first wave that may contain CRD-defined kinds, so newly
        // registered kinds are resolvable (otherwise apply fails with
        // ApiResourceNotFound). Retry until the kinds of the remaining resources are
        // served, since a freshly applied CRD may not be Established the instant its
        // apply returns.
        if crd_applied && !discovery_refreshed && !keys[wave.clone()].iter().all(is_crd) {
            let needed: Vec<GroupVersionKind> = keys[wave.start..]
                .iter()
                .filter(|key| key.gvk.kind != "CustomResourceDefinition")
                .map(|key| key.gvk.clone())
                .collect();
            client.refresh_discovery_until_available(&needed).await?;
            discovery_refreshed = true;
        }

        let mut applies = stream::iter(wave.filter(is_applied))
            .map(|index| async move { (index, apply_manifest(client, &manifests[index]).await) })
            .buffer_unordered(concurrency);
        while let Some((index, result)) = applies.next().await {
            on_result(&keys[index], &result);
            if result.is_ok() && is_crd(&keys[index]) {
                crd_applied = true;
            }
            results[index] = Some(result);
        }
    }

    let mut outcomes = Vec::new();
    let mut failed_count = 0;
    let mut resource_keys = Vec::new();
    for (index, (key, result)) in keys.iter().zip(results).enumerate() {
        if !is_applied(&index) {
            continue;
        }
        match result.expect("every applied manifest belongs to exactly one apply wave") {
            Ok(outcome) => {
                outcomes.push(outcome);
                resource_keys.push(key.clone());
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

fn is_crd(key: &ResourceKey) -> bool {
    key.gvk.kind == "CustomResourceDefinition" && key.gvk.group == "apiextensions.k8s.io"
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

/// Print apply summary
#[allow(clippy::too_many_lines)]
pub(crate) fn print_apply_summary(
    outcomes: &[ApplyOutcome],
    release: Option<&ReleaseState>,
    duplicates: &HashMap<ResourceKey, usize>,
    failed_count: usize,
) {
    // Per-resource lines are printed as each apply completes; see `print_apply_result`.
    if !outcomes.is_empty() || failed_count > 0 {
        println!();
    }

    // Print summary counts
    let mut created = 0;
    let mut updated = 0;
    let mut unchanged = 0;

    for outcome in outcomes {
        match outcome {
            ApplyOutcome::Created { .. } => created += 1,
            ApplyOutcome::Updated { .. } => updated += 1,
            ApplyOutcome::Unchanged { .. } => unchanged += 1,
            ApplyOutcome::DryRun { would_be } => match **would_be {
                ApplyOutcome::Created { .. } => created += 1,
                ApplyOutcome::Updated { .. } => updated += 1,
                ApplyOutcome::Unchanged { .. } => unchanged += 1,
                ApplyOutcome::DryRun { .. } => {} // shouldn't happen
            },
        }
    }

    let total_duplicates_ignored: usize = duplicates.values().map(|count| count - 1).sum();

    let mut parts = vec![
        format!("{} created", created.to_string().green()),
        format!("{} updated", updated.to_string().yellow()),
        format!("{} unchanged", unchanged),
    ];

    if total_duplicates_ignored > 0 {
        let plural = if total_duplicates_ignored == 1 {
            "duplicate"
        } else {
            "duplicates"
        };
        parts.push(format!(
            "{} {} ignored",
            total_duplicates_ignored.to_string().bright_black(),
            plural
        ));
    }

    if failed_count > 0 {
        parts.push(format!("{} failed", failed_count.to_string().red()));
    }

    println!("Summary: {}", parts.join(", "));

    if let Some(release) = release {
        println!();
        if release.status == ReleaseStatus::Deployed {
            println!(
                "Release: {} revision {} deployed successfully to namespace {}",
                release.release_name, release.revision, release.release_namespace
            );
        } else {
            println!("Release: {} revision {} failed", release.release_name, release.revision);
        }
    }
}

/// Print the line for one completed apply.
pub(crate) fn print_apply_result(
    key: &ResourceKey,
    result: &Result<ApplyOutcome>,
    duplicates: &HashMap<ResourceKey, usize>,
) {
    match result {
        Ok(outcome) => println!("{}", format_outcome_line(outcome, duplicates)),
        Err(e) => {
            let error_msg = format!("(failed to apply resource: {})", e);
            println!("{} {} {}", "✗".red().bold(), key, error_msg.red());
        }
    }
}

/// Format one successful apply outcome.
fn format_outcome_line(outcome: &ApplyOutcome, duplicates: &HashMap<ResourceKey, usize>) -> String {
    let (marker, resource_key) = match outcome {
        ApplyOutcome::Created { resource_key } => ("+".green().bold(), resource_key),
        ApplyOutcome::Updated { resource_key } => ("~".yellow().bold(), resource_key),
        ApplyOutcome::Unchanged { resource_key } => ("=".bright_black().bold(), resource_key),
        ApplyOutcome::DryRun { would_be } => return format_outcome_line(would_be, duplicates),
    };
    format!(
        "{} {} {}{}",
        marker,
        outcome.kind(),
        format_namespace_name(outcome.namespace(), outcome.name()),
        get_duplicate_annotation(resource_key, duplicates)
    )
}

/// Format namespace and name for display
fn format_namespace_name(namespace: Option<&str>, name: &str) -> String {
    if let Some(ns) = namespace {
        format!("{}/{}", ns, name)
    } else {
        name.to_string()
    }
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

/// Get duplicate annotation for a resource if it's a duplicate
fn get_duplicate_annotation(resource_key: &ResourceKey, duplicates: &HashMap<ResourceKey, usize>) -> String {
    // Use direct HashMap lookup with the full ResourceKey (including apiVersion/kind)
    // to avoid incorrectly matching resources with the same kind from different API versions
    if let Some(count) = duplicates.get(resource_key) {
        let ignored_count = count - 1;
        let plural = if ignored_count == 1 { "duplicate" } else { "duplicates" };
        return format!(" {}", format!("({} {} ignored)", ignored_count, plural).yellow());
    }
    String::new()
}

/// Ensure a namespace exists, creating it if necessary
pub(crate) async fn ensure_namespace_exists(client: &KubeRsClient, namespace: &str) -> Result<()> {
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

    #[test]
    fn test_format_namespace_name_with_namespace() {
        assert_eq!(format_namespace_name(Some("default"), "myapp"), "default/myapp");
    }

    #[test]
    fn test_format_namespace_name_without_namespace() {
        assert_eq!(format_namespace_name(None, "mynamespace"), "mynamespace");
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
        let result = apply_sorted_manifests(&client, &manifests, 8, &mut |_, _| {
            reported_at.push(started.elapsed());
        })
        .await
        .unwrap();
        let elapsed = started.elapsed();

        // One round trip for the Namespace wave, two for 16 ConfigMaps at 8 at a
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

    /// Documents that resolve to the same resource are not applied concurrently;
    /// the last one wins, as with serial apply.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_applies_only_last_duplicate() {
        let client = LatencyClient::new("");
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm", "namespace": "app"}, "data": {"v": "first"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "other", "namespace": "app"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm", "namespace": "app"}, "data": {"v": "last"}}),
        ];

        let result = apply_sorted_manifests(&client, &manifests, 8, &mut |_, _| {})
            .await
            .unwrap();

        let names: Vec<&str> = result.resource_keys.iter().map(|key| key.name.as_str()).collect();
        assert_eq!(names, vec!["other", "cm"]);
        let stored = client.inner.get_all_resources();
        let cm = stored
            .values()
            .find(|object| object.metadata.name.as_deref() == Some("cm"))
            .unwrap();
        assert_eq!(cm.data["data"]["v"], "last");
        assert_eq!(client.events().iter().filter(|e| *e == "start cm").count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_concurrency_one_is_serial() {
        let client = LatencyClient::new("");
        let manifests = config_maps(4);

        let started = tokio::time::Instant::now();
        apply_sorted_manifests(&client, &manifests, 1, &mut |_, _| {})
            .await
            .unwrap();

        assert_eq!(started.elapsed(), client.latency * 4);
        assert_eq!(client.max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Namespaces finish before namespaced resources start, and a CRD is applied and
    /// discovery refreshed before its custom resources are applied.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_keeps_namespace_and_crd_barriers() {
        let client = LatencyClient::new("");
        let mut manifests = vec![
            json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "widget", "namespace": "app"}}),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "config", "namespace": "app"}}),
            json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "widgets.example.com"}}),
            json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "app"}}),
        ];
        ResourceOrdering::sort_by_priority(&mut manifests).unwrap();

        apply_sorted_manifests(&client, &manifests, 8, &mut |_, _| {})
            .await
            .unwrap();

        assert!(client.position("end app") < client.position("start widgets.example.com"));
        assert!(client.position("end widgets.example.com") < client.position("refresh ConfigMap,Widget"));
        assert!(client.position("refresh ConfigMap,Widget") < client.position("start config"));
        assert!(client.position("end config") < client.position("start widget"));
    }

    /// A failed resource is reported and counted without stopping the rest, and it is
    /// not recorded as part of the release.
    #[tokio::test(start_paused = true)]
    async fn test_apply_sorted_manifests_continues_after_partial_failure() {
        let client = LatencyClient::new("cm-01");
        let manifests = config_maps(3);

        let mut failures = Vec::new();
        let result = apply_sorted_manifests(&client, &manifests, 8, &mut |key, result| {
            if result.is_err() {
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
