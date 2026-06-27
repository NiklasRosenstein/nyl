use chrono::Utc;
use clap::Args;
use kube::api::DynamicObject;
use std::collections::{HashMap, HashSet};

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

    /// Release name (required if no NylRelease in file)
    #[arg(long)]
    pub name: Option<String>,

    /// Release namespace (required if no NylRelease in file)
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
}

#[allow(clippy::too_many_lines)]
pub async fn execute(args: ApplyArgs) -> Result<()> {
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
    let nyl_release = preflight.nyl_release;
    let mut duplicates = preflight.duplicates;
    let kube_client = preflight
        .kube_client
        .ok_or_else(|| NylError::Config("Kubernetes client unavailable in online mode".to_string()))?;
    let client = preflight
        .raw_client
        .ok_or_else(|| NylError::Config("Raw Kubernetes client unavailable in online mode".to_string()))?;

    if desired_manifests.is_empty() {
        tracing::info!("No manifests to apply");
        return Ok(());
    }

    let release_namespace_hint = nyl_release
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

    // 4. Apply manifests
    let apply_result = apply_sorted_manifests(&kube_client, &desired_manifests).await?;

    if args.no_release {
        print_apply_summary(&apply_result.outcomes, None, &duplicates, apply_result.failed_count);
        if apply_result.failed_count > 0 {
            return Err(NylError::Other(format!(
                "Apply completed with {} error(s)",
                apply_result.failed_count
            )));
        }
        return Ok(());
    }

    // 5. Determine release name and namespace
    let (release_name, release_namespace) = if let Some(ref release) = nyl_release {
        (release.metadata.name.clone(), release.metadata.namespace.clone())
    } else {
        // Require CLI flags if no NylRelease
        let name = args.name.ok_or_else(|| {
            NylError::Config("No NylRelease resource found. Specify --name and --namespace".to_string())
        })?;
        let namespace = args.namespace.ok_or_else(|| {
            NylError::Config("No NylRelease resource found. Specify --name and --namespace".to_string())
        })?;
        (name, namespace)
    };

    // 6. Initialize release storage
    let storage = KubernetesReleaseStorage::new(client);

    // 7-12. Record the new revision, mark the previous one superseded, and prune.
    let release = apply_and_record_release(
        &storage,
        &kube_client,
        &desired_manifests,
        &apply_result,
        &release_name,
        &release_namespace,
        args.append_release,
    )
    .await?;

    // 13. Print summary
    print_apply_summary(
        &apply_result.outcomes,
        Some(&release),
        &duplicates,
        apply_result.failed_count,
    );

    if apply_result.failed_count > 0 {
        return Err(NylError::Other(format!(
            "Apply completed with {} error(s)",
            apply_result.failed_count
        )));
    }

    Ok(())
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
pub(crate) async fn collect_live_state(
    storage: &dyn ReleaseStorage,
    release_name: &str,
    release_namespace: &str,
    next_revision: u32,
) -> Result<(Option<u32>, HashSet<ResourceKey>)> {
    let mut prev_revisions = storage.list_revisions(release_name, release_namespace).await?;
    prev_revisions.retain(|r| *r < next_revision);
    prev_revisions.sort_unstable();

    let mut live_keys: HashSet<ResourceKey> = HashSet::new();
    let mut superseded_revision = None;
    // Walk newest to oldest, unioning keys until (and including) the most recent
    // Deployed revision — that revision captures the full live state.
    for &rev in prev_revisions.iter().rev() {
        if let Some(prev) = storage.get_release(release_name, release_namespace, rev).await? {
            live_keys.extend(prev.resource_keys.iter().cloned());
            if prev.status == ReleaseStatus::Deployed {
                superseded_revision = Some(rev);
                break;
            }
        }
    }

    Ok((superseded_revision, live_keys))
}

/// Build the manifest to store for an `--append-release` revision.
///
/// Combines the current (newly-rendered) manifests with the previous revision's
/// manifest documents for resources that are not part of the current set, so the
/// stored manifest matches the merged `resource_keys`. This keeps `release rollback`
/// faithful: rolling back to an appended revision re-applies the complete desired
/// state rather than only the newly-rendered resources.
fn merge_append_manifest(
    desired_manifests: &[serde_json::Value],
    current_keys: &HashSet<ResourceKey>,
    previous_manifest: &str,
) -> Result<String> {
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
#[allow(clippy::too_many_lines)]
pub(crate) async fn apply_and_record_release(
    storage: &KubernetesReleaseStorage,
    kube_client: &KubeRsClient,
    desired_manifests: &[serde_json::Value],
    apply_result: &ApplyExecutionResult,
    release_name: &str,
    release_namespace: &str,
    append_release: bool,
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
            release.manifest = merge_append_manifest(desired_manifests, &current_keys, &previous_release.manifest)?;

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

    // Save release state. Ensure the release namespace exists first.
    ensure_namespace_exists(kube_client, release_namespace).await?;
    storage.save_release(&release).await?;

    // Supersede the previous revision and prune resources no longer desired.
    if release.status == ReleaseStatus::Deployed && next_revision > 1 {
        if append_release {
            // Append mode validated that the immediately previous revision is Deployed
            // and does not prune; just mark it superseded.
            storage
                .update_release_status(
                    release_name,
                    release_namespace,
                    next_revision - 1,
                    ReleaseStatus::Superseded,
                    None,
                )
                .await
                .ok();
        } else {
            // Reconcile against the resources currently live on the cluster, not just
            // the numerically previous secret. The live state is the most recent
            // Deployed revision's resources plus anything partially applied by Failed
            // revisions after it (Failed revisions never prune). Pruning against only
            // `next_revision - 1` would orphan resources from an older Deployed revision
            // when the immediately previous revision Failed.
            let (superseded_revision, live_keys) =
                collect_live_state(storage, release_name, release_namespace, next_revision).await?;

            if let Some(rev) = superseded_revision {
                storage
                    .update_release_status(release_name, release_namespace, rev, ReleaseStatus::Superseded, None)
                    .await
                    .ok();
            }

            let current_keys: HashSet<&ResourceKey> = release.resource_keys.iter().collect();
            let to_prune = keys_to_prune(&live_keys, &current_keys);
            if !to_prune.is_empty() {
                println!("\nPruning {} resources...", to_prune.len());
                for key in to_prune {
                    match kube_client
                        .delete_resource(&key.gvk, key.namespace.as_deref(), &key.name)
                        .await
                    {
                        Ok(()) => {
                            println!("  ✓ Deleted {}", key);
                        }
                        Err(e) => {
                            println!("  ✗ Failed to delete {}: {}", key, e);
                        }
                    }
                }
                println!();
            }
        }
    }

    Ok(release)
}

pub(crate) async fn apply_sorted_manifests(
    client: &KubeRsClient,
    manifests: &[serde_json::Value],
) -> Result<ApplyExecutionResult> {
    let mut outcomes = Vec::new();
    let mut failed_count = 0;
    let mut resource_keys = Vec::new();
    let mut crd_applied = false;
    let mut discovery_refreshed = false;

    for (i, manifest) in manifests.iter().enumerate() {
        let key = ResourceKey::from_json_value(manifest)?;
        let is_crd = key.gvk.kind == "CustomResourceDefinition";

        // If a CRD was applied earlier in this batch, refresh the discovery cache
        // before applying the first resource of a CRD-defined kind, so the newly
        // registered kind is resolvable (otherwise apply fails with ApiResourceNotFound).
        // Retry until the kinds of the remaining resources are served, since a freshly
        // applied CRD may not be Established the instant its apply returns.
        if !is_crd && crd_applied && !discovery_refreshed {
            let needed: Vec<GroupVersionKind> = manifests[i..]
                .iter()
                .filter_map(|m| ResourceKey::from_json_value(m).ok())
                .map(|k| k.gvk)
                .filter(|gvk| gvk.kind != "CustomResourceDefinition")
                .collect();
            client.refresh_discovery_until_available(&needed).await?;
            discovery_refreshed = true;
        }

        match apply_manifest(client, manifest).await {
            Ok(outcome) => {
                outcomes.push(outcome);
                resource_keys.push(key);
                if is_crd {
                    crd_applied = true;
                }
            }
            Err(e) => {
                let error_msg = format!("(failed to apply resource: {})", e);
                println!("{} {} {}", "✗".red().bold(), key, error_msg.red());
                failed_count += 1;
            }
        }
    }

    Ok(ApplyExecutionResult {
        outcomes,
        failed_count,
        resource_keys,
    })
}

/// Convert manifests to YAML string
pub(crate) fn manifests_to_yaml(manifests: &[serde_json::Value]) -> Result<String> {
    let mut yaml_parts = Vec::new();

    for manifest in manifests {
        let yaml = crate::yaml::serialize_yaml_document(manifest).map_err(NylError::YamlEmit)?;
        yaml_parts.push(yaml);
    }

    Ok(yaml_parts.join("---\n"))
}

/// Apply a single manifest
async fn apply_manifest(client: &KubeRsClient, manifest: &serde_json::Value) -> Result<ApplyOutcome> {
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
    for outcome in outcomes {
        match outcome {
            ApplyOutcome::Created { resource_key } => {
                let ns_name = format_namespace_name(outcome.namespace(), outcome.name());
                let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
                println!(
                    "{} {} {}{}",
                    "+".green().bold(),
                    outcome.kind(),
                    ns_name,
                    dup_annotation
                );
            }
            ApplyOutcome::Updated { resource_key } => {
                let ns_name = format_namespace_name(outcome.namespace(), outcome.name());
                let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
                println!(
                    "{} {} {}{}",
                    "~".yellow().bold(),
                    outcome.kind(),
                    ns_name,
                    dup_annotation
                );
            }
            ApplyOutcome::Unchanged { resource_key } => {
                let ns_name = format_namespace_name(outcome.namespace(), outcome.name());
                let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
                println!(
                    "{} {} {}{}",
                    "=".bright_black().bold(),
                    outcome.kind(),
                    ns_name,
                    dup_annotation
                );
            }
            ApplyOutcome::DryRun { would_be } => {
                // This shouldn't happen anymore since we removed --dry-run
                // But handle it anyway by unwrapping
                print_single_outcome(would_be, duplicates);
            }
        }
    }

    println!();

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

/// Print a single outcome
fn print_single_outcome(outcome: &ApplyOutcome, duplicates: &HashMap<ResourceKey, usize>) {
    match outcome {
        ApplyOutcome::Created { resource_key } => {
            let ns_name = format_namespace_name(outcome.namespace(), outcome.name());
            let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
            println!(
                "{} {} {}{}",
                "+".green().bold(),
                outcome.kind(),
                ns_name,
                dup_annotation
            );
        }
        ApplyOutcome::Updated { resource_key } => {
            let ns_name = format_namespace_name(outcome.namespace(), outcome.name());
            let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
            println!(
                "{} {} {}{}",
                "~".yellow().bold(),
                outcome.kind(),
                ns_name,
                dup_annotation
            );
        }
        ApplyOutcome::Unchanged { resource_key } => {
            let ns_name = format_namespace_name(outcome.namespace(), outcome.name());
            let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
            println!(
                "{} {} {}{}",
                "=".bright_black().bold(),
                outcome.kind(),
                ns_name,
                dup_annotation
            );
        }
        ApplyOutcome::DryRun { would_be } => {
            print_single_outcome(would_be, duplicates);
        }
    }
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
async fn ensure_namespace_exists(client: &KubeRsClient, namespace: &str) -> Result<()> {
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
        tracing::warn!(
            "Namespace '{}' does not exist. Creating it to store release state.",
            namespace
        );

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
        let current_keys: HashSet<ResourceKey> = current
            .iter()
            .map(|m| ResourceKey::from_json_value(m).unwrap())
            .collect();

        let merged = merge_append_manifest(&current, &current_keys, &previous).unwrap();
        let docs = crate::yaml::parse_yaml_documents_k8s_compatible(&merged).unwrap();

        // The stored manifest carries over A (not in the current set) plus B.
        assert_eq!(docs.len(), 2);
        let names: Vec<&str> = docs
            .iter()
            .filter_map(|d| d.get("metadata").and_then(|m| m.get("name")).and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn test_keys_to_prune_nothing_when_superset() {
        let live: HashSet<ResourceKey> = [resource_key("a"), resource_key("b")].into_iter().collect();
        let current = [resource_key("a"), resource_key("b"), resource_key("c")];
        let current_keys: HashSet<&ResourceKey> = current.iter().collect();

        assert!(keys_to_prune(&live, &current_keys).is_empty());
    }
}
