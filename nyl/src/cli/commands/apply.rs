use chrono::Utc;
use clap::Args;
use kube::{api::DynamicObject, Client};

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

    /// Dry run mode
    #[arg(long)]
    pub dry_run: bool,
}

#[allow(clippy::too_many_lines)]
pub async fn execute(args: ApplyArgs) -> Result<()> {
    // 1. Render desired manifests using complete pipeline
    let (desired_manifests, nyl_release, profile, _env_name) = render_manifests_complete(
        &args.common.path,
        args.common.component.as_deref(),
        args.common.profile.as_deref(),
        false, // offline
        None,  // cli_kube_version
        &[],   // cli_api_versions
        args.common.max_depth,
        args.common.track_parent,
    )
    .await?;

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
    let mut errors = Vec::new();
    let mut resource_keys = Vec::new();

    for manifest in &sorted_manifests {
        // Extract resource key
        let key = ResourceKey::from_json_value(manifest)?;

        match apply_manifest(&kube_client, manifest, args.dry_run).await {
            Ok(outcome) => {
                outcomes.push(outcome);
                resource_keys.push(key);
            }
            Err(e) => {
                errors.push(format!("Failed to apply resource: {}", e));
            }
        }
    }

    // 10. Update release state with resource keys
    release.resource_keys = resource_keys;

    // 11. Update release status
    if errors.is_empty() {
        release.status = ReleaseStatus::Deployed;
        release.applied_at = Some(Utc::now());
    } else {
        release.status = ReleaseStatus::Failed;
        release.error = Some(errors.join("; "));
    }

    // 12. Save release state (unless dry run)
    if !args.dry_run {
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
    }

    // 13. Prune resources from previous release that are no longer desired
    let mut pruned_keys = Vec::new();
    if !args.dry_run && release.status == ReleaseStatus::Deployed && next_revision > 1 {
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
    print_apply_summary(&outcomes, &errors, &release, args.dry_run);

    if !errors.is_empty() {
        return Err(NylError::Other(format!(
            "Apply completed with {} error(s)",
            errors.len()
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
async fn apply_manifest(client: &KubeRsClient, manifest: &serde_json::Value, dry_run: bool) -> Result<ApplyOutcome> {
    // Convert JSON to DynamicObject
    let resource: DynamicObject = serde_json::from_value(manifest.clone())?;

    // Apply using client
    client.apply_resource(&resource, "nyl", dry_run).await
}

/// Print apply summary
fn print_apply_summary(outcomes: &[ApplyOutcome], errors: &[String], release: &ReleaseState, dry_run: bool) {
    let prefix = if dry_run { "[DRY RUN] " } else { "" };

    for outcome in outcomes {
        match outcome {
            ApplyOutcome::Created { name, namespace } => {
                let ns_name = format_namespace_name(namespace.as_deref(), name);
                println!("{}✓ Created {}", prefix, ns_name);
            }
            ApplyOutcome::Updated { name, namespace } => {
                let ns_name = format_namespace_name(namespace.as_deref(), name);
                println!("{}✓ Updated {}", prefix, ns_name);
            }
            ApplyOutcome::Unchanged { name, namespace } => {
                let ns_name = format_namespace_name(namespace.as_deref(), name);
                println!("{}✓ Unchanged {}", prefix, ns_name);
            }
            ApplyOutcome::DryRun { would_be } => {
                // Recursively print inner outcome
                print_single_outcome(would_be, true);
            }
        }
    }

    for error in errors {
        println!("{}✗ {}", prefix, error);
    }

    println!();

    if dry_run {
        println!(
            "[DRY RUN] Would create release {} revision {} in namespace {}",
            release.release_name, release.revision, release.release_namespace
        );
    } else if release.status == ReleaseStatus::Deployed {
        println!(
            "Release: {} revision {} deployed successfully to namespace {}",
            release.release_name, release.revision, release.release_namespace
        );
    } else {
        println!("Release: {} revision {} failed", release.release_name, release.revision);
    }
}

/// Print a single outcome
fn print_single_outcome(outcome: &ApplyOutcome, dry_run: bool) {
    let prefix = if dry_run { "[DRY RUN] " } else { "" };

    match outcome {
        ApplyOutcome::Created { name, namespace } => {
            let ns_name = format_namespace_name(namespace.as_deref(), name);
            println!("{}✓ Would create {}", prefix, ns_name);
        }
        ApplyOutcome::Updated { name, namespace } => {
            let ns_name = format_namespace_name(namespace.as_deref(), name);
            println!("{}✓ Would update {}", prefix, ns_name);
        }
        ApplyOutcome::Unchanged { name, namespace } => {
            let ns_name = format_namespace_name(namespace.as_deref(), name);
            println!("{}✓ Would leave unchanged {}", prefix, ns_name);
        }
        ApplyOutcome::DryRun { would_be } => {
            print_single_outcome(would_be, true);
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
