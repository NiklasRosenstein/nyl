use chrono::Utc;
use clap::Args;
use kube::api::DynamicObject;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::time::{sleep, Instant};

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

    // 3. Sort resources by priority (Namespace → CRD → RBAC → Config → Workload)
    let mut sorted_manifests = desired_manifests.clone();
    ResourceOrdering::sort_by_priority(&mut sorted_manifests)?;

    // 4. Apply manifests (Namespaces/CRDs first, then refresh discovery for remaining resources)
    let apply_result = apply_manifests_with_bootstrap_phase(&kube_client, &client, &sorted_manifests).await?;

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

    // 7. Determine next revision number
    let revisions = storage.list_revisions(&release_name, &release_namespace).await?;
    let next_revision = revisions.iter().max().map_or(1, |r| r + 1);

    // 8. Create initial release state
    let mut release = ReleaseState {
        release_name: release_name.clone(),
        release_namespace: release_namespace.clone(),
        revision: next_revision,
        resource_keys: apply_result.resource_keys.clone(),
        manifest: manifests_to_yaml(&desired_manifests)?,
        status: ReleaseStatus::Rendered,
        rendered_at: Utc::now(),
        applied_at: None,
        error: None,
    };

    // 9. Append-release mode: merge with previous release
    if args.append_release && next_revision > 1 {
        // Fetch previous release
        if let Ok(Some(previous_release)) = storage
            .get_release(&release_name, &release_namespace, next_revision - 1)
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
            let current_keys: std::collections::HashSet<_> = release.resource_keys.iter().cloned().collect();

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

    // 10. Update release status
    if apply_result.failed_count == 0 {
        release.status = ReleaseStatus::Deployed;
        release.applied_at = Some(Utc::now());
    } else {
        release.status = ReleaseStatus::Failed;
        release.error = Some(format!("{} resource(s) failed to apply", apply_result.failed_count));
    }

    // 11. Save release state
    // Ensure the release namespace exists before saving the release state
    ensure_namespace_exists(&kube_client, &release_namespace).await?;

    storage.save_release(&release).await?;

    // Mark previous revision as superseded (if successful)
    if release.status == ReleaseStatus::Deployed && next_revision > 1 {
        let prev_revision = next_revision - 1;
        storage
            .update_release_status(
                &release_name,
                &release_namespace,
                prev_revision,
                ReleaseStatus::Superseded,
                None,
            )
            .await
            .ok(); // Ignore errors if previous revision doesn't exist
    }

    // 12. Prune resources from previous release that are no longer desired
    if !args.append_release && release.status == ReleaseStatus::Deployed && next_revision > 1 {
        // Get previous release's resource keys
        if let Ok(Some(previous_release)) = storage
            .get_release(&release_name, &release_namespace, next_revision - 1)
            .await
        {
            // Find resources to prune (in previous but not in current)
            let current_keys: std::collections::HashSet<_> = release.resource_keys.iter().collect();
            let to_prune: Vec<_> = previous_release
                .resource_keys
                .iter()
                .filter(|k| !current_keys.contains(k))
                .collect();

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

struct ApplyExecutionResult {
    outcomes: Vec<ApplyOutcome>,
    failed_count: usize,
    resource_keys: Vec<ResourceKey>,
}

async fn apply_manifests_with_bootstrap_phase(
    kube_client: &KubeRsClient,
    raw_client: &kube::Client,
    manifests: &[serde_json::Value],
) -> Result<ApplyExecutionResult> {
    let (bootstrap_manifests, remaining_manifests) = partition_bootstrap_manifests(manifests);
    let bootstrap_crd_gvks = collect_crd_gvks_from_manifests(&bootstrap_manifests);

    let mut combined = ApplyExecutionResult {
        outcomes: Vec::new(),
        failed_count: 0,
        resource_keys: Vec::new(),
    };

    if !bootstrap_manifests.is_empty() {
        let phase_result = apply_sorted_manifests(kube_client, &bootstrap_manifests).await?;
        if phase_result.failed_count == 0 && !bootstrap_crd_gvks.is_empty() && !remaining_manifests.is_empty() {
            wait_for_crd_api_discovery(raw_client, &bootstrap_crd_gvks).await?;
        }
        combined.outcomes.extend(phase_result.outcomes);
        combined.failed_count += phase_result.failed_count;
        combined.resource_keys.extend(phase_result.resource_keys);
    }

    if !remaining_manifests.is_empty() {
        let phase_result = if bootstrap_manifests.is_empty() {
            apply_sorted_manifests(kube_client, &remaining_manifests).await?
        } else {
            let refreshed_client = KubeRsClient::from_client(raw_client.clone()).await?;
            apply_sorted_manifests(&refreshed_client, &remaining_manifests).await?
        };
        combined.outcomes.extend(phase_result.outcomes);
        combined.failed_count += phase_result.failed_count;
        combined.resource_keys.extend(phase_result.resource_keys);
    }

    Ok(combined)
}

fn partition_bootstrap_manifests(manifests: &[serde_json::Value]) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    manifests.iter().cloned().partition(|manifest| {
        matches!(
            manifest.get("kind").and_then(serde_json::Value::as_str),
            Some("Namespace" | "CustomResourceDefinition")
        )
    })
}

fn collect_crd_gvks_from_manifests(manifests: &[serde_json::Value]) -> Vec<GroupVersionKind> {
    let mut seen = HashSet::new();
    let mut gvks = Vec::new();

    for manifest in manifests {
        let is_crd = manifest
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind == "CustomResourceDefinition");
        if !is_crd {
            continue;
        }

        let Some(group) = manifest
            .get("spec")
            .and_then(|spec| spec.get("group"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };

        let Some(kind) = manifest
            .get("spec")
            .and_then(|spec| spec.get("names"))
            .and_then(|names| names.get("kind"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };

        if let Some(versions) = manifest
            .get("spec")
            .and_then(|spec| spec.get("versions"))
            .and_then(serde_json::Value::as_array)
        {
            for version in versions {
                let is_served = version
                    .get("served")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !is_served {
                    continue;
                }
                let Some(version_name) = version.get("name").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let gvk = GroupVersionKind {
                    group: group.to_string(),
                    version: version_name.to_string(),
                    kind: kind.to_string(),
                };
                if seen.insert(gvk.clone()) {
                    gvks.push(gvk);
                }
            }
            continue;
        }

        if let Some(version_name) = manifest
            .get("spec")
            .and_then(|spec| spec.get("version"))
            .and_then(serde_json::Value::as_str)
        {
            let gvk = GroupVersionKind {
                group: group.to_string(),
                version: version_name.to_string(),
                kind: kind.to_string(),
            };
            if seen.insert(gvk.clone()) {
                gvks.push(gvk);
            }
        }
    }

    gvks
}

async fn wait_for_crd_api_discovery(raw_client: &kube::Client, crd_gvks: &[GroupVersionKind]) -> Result<()> {
    const CRD_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
    const RETRY_INTERVAL: Duration = Duration::from_millis(500);
    const PROBE_RESOURCE_NAME: &str = "__nyl_crd_discovery_probe__";

    let deadline = Instant::now() + CRD_DISCOVERY_TIMEOUT;
    let mut unresolved = crd_gvks.to_vec();

    loop {
        let refreshed_client = KubeRsClient::from_client(raw_client.clone()).await?;
        let mut next_unresolved = Vec::new();
        for gvk in &unresolved {
            match refreshed_client.get_resource(gvk, None, PROBE_RESOURCE_NAME).await {
                Ok(_) => {}
                Err(err) if err.is_api_resource_not_found_error() => next_unresolved.push(gvk.clone()),
                Err(err) => return Err(err),
            }
        }
        unresolved = next_unresolved;

        if unresolved.is_empty() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            let pending = unresolved
                .iter()
                .map(GroupVersionKind::format)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(NylError::Other(format!(
                "Timed out waiting for CRD API discovery for: {pending}"
            )));
        }

        sleep(RETRY_INTERVAL).await;
    }
}

async fn apply_sorted_manifests(
    client: &KubeRsClient,
    manifests: &[serde_json::Value],
) -> Result<ApplyExecutionResult> {
    let mut outcomes = Vec::new();
    let mut failed_count = 0;
    let mut resource_keys = Vec::new();

    for manifest in manifests {
        let key = ResourceKey::from_json_value(manifest)?;
        match apply_manifest(client, manifest).await {
            Ok(outcome) => {
                outcomes.push(outcome);
                resource_keys.push(key);
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
fn manifests_to_yaml(manifests: &[serde_json::Value]) -> Result<String> {
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
fn print_apply_summary(
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

    #[test]
    fn test_partition_bootstrap_manifests_extracts_namespace_and_crd() {
        let manifests = vec![
            json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "app"}}),
            json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "test"}}),
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {"name": "widgets.example.com"}
            }),
            json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "svc"}}),
        ];

        let (bootstrap, remaining) = partition_bootstrap_manifests(&manifests);

        assert_eq!(bootstrap.len(), 2);
        assert_eq!(bootstrap[0]["kind"], "Namespace");
        assert_eq!(bootstrap[1]["kind"], "CustomResourceDefinition");
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0]["kind"], "Deployment");
        assert_eq!(remaining[1]["kind"], "Service");
    }

    #[test]
    fn test_partition_bootstrap_manifests_keeps_non_bootstrap_resources() {
        let manifests = vec![
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg"}}),
            json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "ClusterRole", "metadata": {"name": "role"}}),
        ];

        let (bootstrap, remaining) = partition_bootstrap_manifests(&manifests);

        assert!(bootstrap.is_empty());
        assert_eq!(remaining, manifests);
    }

    #[test]
    fn test_collect_crd_gvks_from_manifests_collects_served_versions() {
        let manifests = vec![
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {"name": "widgets.example.com"},
                "spec": {
                    "group": "example.com",
                    "names": {"kind": "Widget"},
                    "versions": [
                        {"name": "v1", "served": true, "storage": true},
                        {"name": "v2", "served": false, "storage": false}
                    ]
                }
            }),
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {"name": "gadgets.example.com"},
                "spec": {
                    "group": "example.com",
                    "names": {"kind": "Gadget"},
                    "version": "v1beta1"
                }
            }),
        ];

        let gvks = collect_crd_gvks_from_manifests(&manifests);

        assert_eq!(gvks.len(), 2);
        assert_eq!(gvks[0].format(), "example.com/v1/Widget");
        assert_eq!(gvks[1].format(), "example.com/v1beta1/Gadget");
    }

    #[test]
    fn test_collect_crd_gvks_from_manifests_deduplicates_entries() {
        let manifests = vec![
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {"name": "widgets.example.com"},
                "spec": {
                    "group": "example.com",
                    "names": {"kind": "Widget"},
                    "versions": [{"name": "v1", "served": true, "storage": true}]
                }
            }),
            json!({
                "apiVersion": "apiextensions.k8s.io/v1",
                "kind": "CustomResourceDefinition",
                "metadata": {"name": "widgets2.example.com"},
                "spec": {
                    "group": "example.com",
                    "names": {"kind": "Widget"},
                    "versions": [{"name": "v1", "served": true, "storage": true}]
                }
            }),
        ];

        let gvks = collect_crd_gvks_from_manifests(&manifests);

        assert_eq!(gvks.len(), 1);
        assert_eq!(gvks[0].format(), "example.com/v1/Widget");
    }
}
