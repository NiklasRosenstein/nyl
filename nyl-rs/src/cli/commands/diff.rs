use clap::Args;
use kube::Client;
use std::collections::{HashMap, HashSet};

use crate::{
    cli::commands::render::render_manifests,
    kubernetes::{
        extract_name, KubeClient, KubeRsClient, KubernetesReleaseStorage, ResourceKey,
        ReleaseStorage,
    },
    resources::extract_nyl_release,
    NylError, Result,
};

/// Show diff between rendered manifests and cluster state
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Release name (required if no NylRelease in file)
    #[arg(long)]
    pub name: Option<String>,

    /// Release namespace (required if no NylRelease in file)
    #[arg(long)]
    pub namespace: Option<String>,

    /// Component to diff (if not specified, diffs all)
    #[arg(short, long)]
    pub component: Option<String>,

    /// Environment to diff for
    #[arg(short, long)]
    pub environment: Option<String>,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,

    /// Show summary only (counts, no detailed diff)
    #[arg(long)]
    pub summary: bool,
}

pub async fn execute(args: DiffArgs) -> Result<()> {
    // 1. Render desired manifests
    let (raw_manifests, profile, _env_name) = render_manifests(
        &args.path,
        args.component.as_deref(),
        args.environment.as_deref(),
    )?;

    if raw_manifests.is_empty() {
        println!("No manifests to diff");
        return Ok(());
    }

    // 2. Extract NylRelease metadata and filter it from manifests
    let (nyl_release, desired_manifests) = extract_nyl_release(&raw_manifests)?;

    // 3. Determine release name and namespace
    let (release_name, release_namespace) = match nyl_release {
        Some(ref release) => (
            release.metadata.name.clone(),
            release.metadata.namespace.clone(),
        ),
        None => {
            // Require CLI flags if no NylRelease
            let name = args.name.ok_or_else(|| {
                NylError::Config(
                    "No NylRelease resource found. Specify --name and --namespace".to_string(),
                )
            })?;
            let namespace = args.namespace.ok_or_else(|| {
                NylError::Config(
                    "No NylRelease resource found. Specify --name and --namespace".to_string(),
                )
            })?;
            (name, namespace)
        }
    };

    if desired_manifests.is_empty() {
        println!("No Kubernetes resources to diff (only NylRelease found)");
        return Ok(());
    }

    // 4. Initialize Kubernetes client
    let kube_client = KubeRsClient::from_profile(&profile, args.context.as_deref()).await?;

    // Also get raw client for state storage
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

    // 6. Fetch previous release for tracking resource deletions
    let previous_release = storage
        .get_latest_release(&release_name, &release_namespace)
        .await?;

    // 7. Compute diff against LIVE cluster state
    let diff_result =
        compute_diff_from_live(&kube_client, &desired_manifests, previous_release.as_ref())
            .await?;

    // 8. Display diff
    if args.summary {
        display_summary(&diff_result);
    } else {
        display_diff(&diff_result);
    }

    Ok(())
}

/// Extract component name from manifests (use first resource name)
pub fn extract_component_name(manifests: &[serde_json::Value]) -> Result<String> {
    if manifests.is_empty() {
        return Err(NylError::Config("No manifests to diff".to_string()));
    }

    // Try to extract from first resource
    let first = &manifests[0];
    let name = extract_name(first)?;

    // Use the resource name as component name (could be improved)
    Ok(name)
}

/// Diff result categorization
#[derive(Debug)]
struct DiffResult {
    added: Vec<ResourceKey>,
    modified: Vec<ResourceKey>,
    deleted: Vec<ResourceKey>,
    unchanged: Vec<ResourceKey>,
}

/// Compute diff between desired manifests and LIVE cluster state
async fn compute_diff_from_live(
    client: &dyn KubeClient,
    desired_manifests: &[serde_json::Value],
    previous_state: Option<&crate::kubernetes::ReleaseState>,
) -> Result<DiffResult> {
    // Build set of desired resource keys
    let desired_keys: HashSet<ResourceKey> = desired_manifests
        .iter()
        .map(|m| ResourceKey::from_json_value(m))
        .collect::<Result<_>>()?;

    // Fetch live resources for desired manifests
    let mut live_resources = HashMap::new();
    for manifest in desired_manifests {
        let key = ResourceKey::from_json_value(manifest)?;
        if let Some(resource) = client
            .get_resource(&key.gvk, key.namespace.as_deref(), &key.name)
            .await?
        {
            // Convert DynamicObject to JSON for comparison
            let live_json = serde_json::to_value(&resource)?;
            live_resources.insert(key.clone(), live_json);
        }
    }

    // Get previous resource keys for deletion tracking
    let previous_keys = previous_state
        .map(|s| s.resource_keys.iter().cloned().collect())
        .unwrap_or_else(HashSet::new);

    // Also fetch resources from previous state that aren't in desired
    for key in &previous_keys {
        if !desired_keys.contains(key) {
            // This resource is being deleted - check if it still exists in cluster
            if let Some(resource) = client
                .get_resource(&key.gvk, key.namespace.as_deref(), &key.name)
                .await?
            {
                let live_json = serde_json::to_value(&resource)?;
                live_resources.insert(key.clone(), live_json);
            }
        }
    }

    // Categorize changes
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut unchanged = Vec::new();

    // Check desired vs live
    for manifest in desired_manifests {
        let key = ResourceKey::from_json_value(manifest)?;
        if let Some(live) = live_resources.get(&key) {
            if are_resources_equivalent(manifest, live) {
                unchanged.push(key);
            } else {
                modified.push(key);
            }
        } else {
            added.push(key);
        }
    }

    // Check for deletions (in previous state but not in desired)
    for key in previous_keys {
        if !desired_keys.contains(&key) {
            deleted.push(key);
        }
    }

    Ok(DiffResult {
        added,
        modified,
        deleted,
        unchanged,
    })
}

/// Check if two resources are equivalent (deep equality)
fn are_resources_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    // For now, use simple JSON equality
    // Could be enhanced to ignore certain fields like metadata.resourceVersion
    a == b
}

/// Display diff results
fn display_diff(diff: &DiffResult) {
    for key in &diff.added {
        println!("+ {}", key.to_string());
    }

    for key in &diff.modified {
        println!("~ {}", key.to_string());
    }

    for key in &diff.deleted {
        println!("- {}", key.to_string());
    }

    println!();
    println!(
        "Summary: {} added, {} modified, {} deleted, {} unchanged",
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len(),
        diff.unchanged.len()
    );
}

/// Display summary only (counts, no detailed diff)
fn display_summary(diff: &DiffResult) {
    println!(
        "Summary: {} added, {} modified, {} deleted, {} unchanged",
        diff.added.len(),
        diff.modified.len(),
        diff.deleted.len(),
        diff.unchanged.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_are_resources_equivalent() {
        let a = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        let b = a.clone();
        assert!(are_resources_equivalent(&a, &b));

        let c = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "different"}
        });
        assert!(!are_resources_equivalent(&a, &c));
    }

    // Note: Full diff testing with live cluster requires integration tests
    // with MockKubeClient or a real cluster. Unit tests removed since
    // compute_diff_from_live is async and requires a KubeClient.
}

