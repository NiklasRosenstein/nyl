use chrono::Utc;
use clap::Args;
use kube::{api::DynamicObject, Client};

use crate::{
    cli::commands::{diff::extract_component_name, render::render_manifests},
    kubernetes::{
        ApplyOutcome, KubeClient, KubeRsClient, KubernetesReleaseStorage, ReleaseState,
        ReleaseStatus, ReleaseStorage,
    },
    NylError, Result,
};

/// Apply rendered manifests to the cluster
#[derive(Args, Debug)]
pub struct ApplyArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Component to apply (if not specified, applies all)
    #[arg(short, long)]
    pub component: Option<String>,

    /// Environment to apply to
    #[arg(short, long)]
    pub environment: Option<String>,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,

    /// Dry run mode
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn execute(args: ApplyArgs) -> Result<()> {
    // 1. Render desired manifests
    let (desired_manifests, profile, _env_name) = render_manifests(
        &args.path,
        args.component.as_deref(),
        args.environment.as_deref(),
    )?;

    if desired_manifests.is_empty() {
        println!("No manifests to apply");
        return Ok(());
    }

    // 2. Determine component name
    let component_name = extract_component_name(&desired_manifests)?;

    // 3. Initialize Kubernetes client
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

    // 4. Initialize state storage
    let storage = KubernetesReleaseStorage::new(client, "nyl-state".to_string());

    // 5. Determine next revision number
    let revisions = storage.list_revisions(&component_name).await?;
    let next_revision = revisions.iter().max().map(|r| r + 1).unwrap_or(1);

    // 6. Convert manifests to YAML string for storage
    let manifest_yaml = manifests_to_yaml(&desired_manifests)?;

    // 7. Create initial release state
    let mut release = ReleaseState {
        component: component_name.clone(),
        revision: next_revision,
        manifest: manifest_yaml,
        values: serde_json::json!({}), // Could be populated with actual values
        helmchart: None,                // Could be populated if using Helm
        status: ReleaseStatus::Rendered,
        rendered_at: Utc::now(),
        applied_at: None,
        error: None,
    };

    // 8. Apply each resource
    let mut outcomes = Vec::new();
    let mut errors = Vec::new();

    for manifest in &desired_manifests {
        match apply_manifest(&kube_client, manifest, args.dry_run).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => {
                errors.push(format!("Failed to apply resource: {}", e));
            }
        }
    }

    // 9. Update release status
    if errors.is_empty() {
        release.status = ReleaseStatus::Deployed;
        release.applied_at = Some(Utc::now());
    } else {
        release.status = ReleaseStatus::Failed;
        release.error = Some(errors.join("; "));
    }

    // 10. Save release state (unless dry run)
    if !args.dry_run {
        storage.save_release(&release).await?;

        // Mark previous revision as superseded (if successful)
        if release.status == ReleaseStatus::Deployed && next_revision > 1 {
            let prev_revision = next_revision - 1;
            storage
                .update_release_status(
                    &component_name,
                    prev_revision,
                    ReleaseStatus::Superseded,
                    None,
                )
                .await
                .ok(); // Ignore errors if previous revision doesn't exist
        }
    }

    // 11. Print summary
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
async fn apply_manifest(
    client: &KubeRsClient,
    manifest: &serde_json::Value,
    dry_run: bool,
) -> Result<ApplyOutcome> {
    // Convert JSON to DynamicObject
    let resource: DynamicObject = serde_json::from_value(manifest.clone())?;

    // Apply using client
    client.apply_resource(&resource, "nyl", dry_run).await
}

/// Print apply summary
fn print_apply_summary(
    outcomes: &[ApplyOutcome],
    errors: &[String],
    release: &ReleaseState,
    dry_run: bool,
) {
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
        println!("[DRY RUN] Would create release {} revision {}", release.component, release.revision);
    } else if release.status == ReleaseStatus::Deployed {
        println!(
            "Release: {} revision {} deployed successfully",
            release.component, release.revision
        );
    } else {
        println!(
            "Release: {} revision {} failed",
            release.component, release.revision
        );
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
        assert_eq!(
            format_namespace_name(Some("default"), "myapp"),
            "default/myapp"
        );
    }

    #[test]
    fn test_format_namespace_name_without_namespace() {
        assert_eq!(format_namespace_name(None, "mynamespace"), "mynamespace");
    }
}

