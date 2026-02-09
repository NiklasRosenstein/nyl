use chrono::Utc;
use clap::Args;
use kube::{api::DynamicObject, Client};
use std::collections::HashMap;

use colored::Colorize;

use crate::{
    cli::commands::render::{render_manifests_complete, RenderOptions},
    kubernetes::{
        ApplyOutcome, KubeClient, KubeRsClient, KubernetesReleaseStorage, ReleaseState, ReleaseStatus, ReleaseStorage,
        ResourceKey, ResourceOrdering,
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
}

#[allow(clippy::too_many_lines)]
pub async fn execute(args: ApplyArgs) -> Result<()> {
    // 1. Render desired manifests using complete pipeline
    let (mut desired_manifests, nyl_release, profile, _env_name, duplicates) = render_manifests_complete(
        &args.common.path,
        args.common.only_source_kind.as_deref(),
        args.common.profile.as_deref(),
        false, // offline
        None,  // cli_kube_version
        &[],   // cli_api_versions
        args.common.max_depth,
        args.common.track_parent,
    )
    .await?;

    // Apply post-render kind filtering
    desired_manifests = crate::cli::filter::filter_manifests_by_kind(
        desired_manifests,
        &args.common.only_kind,
        &args.common.exclude_kind,
    )?;

    // Display duplicate resources warning if any
    if !duplicates.is_empty() {
        print_duplicate_warning(&duplicates);
    }

    if desired_manifests.is_empty() {
        tracing::info!("No manifests to apply");
        return Ok(());
    }

    // 2. Determine release name and namespace
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

    // 4. Initialize Kubernetes client
    let kube_client = KubeRsClient::from_profile(&profile, args.context.as_deref()).await?;

    // Get underlying client for state storage
    let config = if let Some(ctx) = &args.context {
        let kubeconfig = kube::config::Kubeconfig::read()?;
        kube::Config::from_custom_kubeconfig(
            kubeconfig,
            &kube::config::KubeConfigOptions {
                context: Some(ctx.clone()),
                ..Default::default()
            },
        )
        .await?
    } else {
        kube::Config::infer().await?
    };
    let client = Client::try_from(config)?;

    // 5. Initialize state storage
    let storage = KubernetesReleaseStorage::new(client);

    // 6. Determine next revision number
    let revisions = storage.list_revisions(&release_name, &release_namespace).await?;
    let next_revision = revisions.iter().max().map_or(1, |r| r + 1);

    // 7. Convert manifests to YAML string for storage
    let manifest_yaml = manifests_to_yaml(&desired_manifests)?;

    // 8. Create initial release state
    let mut release = ReleaseState {
        release_name: release_name.clone(),
        release_namespace: release_namespace.clone(),
        revision: next_revision,
        resource_keys: Vec::new(), // Will be populated during apply
        manifest: manifest_yaml,
        status: ReleaseStatus::Rendered,
        rendered_at: Utc::now(),
        applied_at: None,
        error: None,
    };

    // 9. Sort resources by priority (Namespace → CRD → RBAC → Config → Workload)
    let mut sorted_manifests = desired_manifests.clone();
    ResourceOrdering::sort_by_priority(&mut sorted_manifests)?;

    // 10. Apply each resource and track resource keys
    let mut outcomes = Vec::new();
    let mut failed_count = 0;
    let mut resource_keys = Vec::new();

    for manifest in &sorted_manifests {
        // Extract resource key
        let key = ResourceKey::from_json_value(manifest)?;

        match apply_manifest(&kube_client, manifest).await {
            Ok(outcome) => {
                outcomes.push(outcome);
                resource_keys.push(key);
            }
            Err(e) => {
                // Print error immediately with resource info
                let error_msg = format!("(failed to apply resource: {})", e);
                println!("{} {} {}", "✗".red().bold(), key, error_msg.red());
                failed_count += 1;
            }
        }
    }

    // 10. Update release state with resource keys
    release.resource_keys = resource_keys;

    // 10.5. Append-release mode: merge with previous release
    if args.append_release && next_revision > 1 {
        // Fetch previous release
        if let Ok(Some(previous_release)) = storage
            .get_release(&release_name, &release_namespace, next_revision - 1)
            .await
        {
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

    // 11. Update release status
    if failed_count == 0 {
        release.status = ReleaseStatus::Deployed;
        release.applied_at = Some(Utc::now());
    } else {
        release.status = ReleaseStatus::Failed;
        release.error = Some(format!("{} resource(s) failed to apply", failed_count));
    }

    // 12. Save release state
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

    // 13. Prune resources from previous release that are no longer desired
    let mut pruned_keys = Vec::new();
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
                            pruned_keys.push(key.clone());
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

    // 14. Print summary
    print_apply_summary(&outcomes, &release, &duplicates, failed_count);

    if failed_count > 0 {
        return Err(NylError::Other(format!(
            "Apply completed with {} error(s)",
            failed_count
        )));
    }

    Ok(())
}

/// Convert manifests to YAML string
fn manifests_to_yaml(manifests: &[serde_json::Value]) -> Result<String> {
    let mut yaml_parts = Vec::new();

    for manifest in manifests {
        let yaml = serde_norway::to_string(manifest)?;
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
    release: &ReleaseState,
    duplicates: &HashMap<ResourceKey, usize>,
    failed_count: usize,
) {
    for outcome in outcomes {
        match outcome {
            ApplyOutcome::Created {
                resource_key,
                kind,
                name,
                namespace,
            } => {
                let ns_name = format_namespace_name(namespace.as_deref(), name);
                let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
                println!("{} {} {}{}", "+".green().bold(), kind, ns_name, dup_annotation);
            }
            ApplyOutcome::Updated {
                resource_key,
                kind,
                name,
                namespace,
            } => {
                let ns_name = format_namespace_name(namespace.as_deref(), name);
                let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
                println!("{} {} {}{}", "~".yellow().bold(), kind, ns_name, dup_annotation);
            }
            ApplyOutcome::Unchanged {
                resource_key,
                kind,
                name,
                namespace,
            } => {
                let ns_name = format_namespace_name(namespace.as_deref(), name);
                let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
                println!("{} {} {}{}", "=".bright_black().bold(), kind, ns_name, dup_annotation);
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

/// Print a single outcome
fn print_single_outcome(outcome: &ApplyOutcome, duplicates: &HashMap<ResourceKey, usize>) {
    match outcome {
        ApplyOutcome::Created {
            resource_key,
            kind,
            name,
            namespace,
        } => {
            let ns_name = format_namespace_name(namespace.as_deref(), name);
            let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
            println!("{} {} {}{}", "+".green().bold(), kind, ns_name, dup_annotation);
        }
        ApplyOutcome::Updated {
            resource_key,
            kind,
            name,
            namespace,
        } => {
            let ns_name = format_namespace_name(namespace.as_deref(), name);
            let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
            println!("{} {} {}{}", "~".yellow().bold(), kind, ns_name, dup_annotation);
        }
        ApplyOutcome::Unchanged {
            resource_key,
            kind,
            name,
            namespace,
        } => {
            let ns_name = format_namespace_name(namespace.as_deref(), name);
            let dup_annotation = get_duplicate_annotation(resource_key, duplicates);
            println!("{} {} {}{}", "=".bright_black().bold(), kind, ns_name, dup_annotation);
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
}
