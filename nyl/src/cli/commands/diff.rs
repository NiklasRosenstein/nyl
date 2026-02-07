use clap::{Args, ValueEnum};
use colored::Colorize;
use kube::Client;
use std::collections::{HashMap, HashSet};

use crate::{
    cli::commands::render::render_manifests,
    kubernetes::{
        extract_name, DiffEngine, KubeClient, KubeRsClient, KubernetesReleaseStorage, ReleaseStorage, ResourceKey,
    },
    resources::extract_nyl_release,
    NylError, Result,
};

/// Diff mode for comparing manifests
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum DiffMode {
    /// Normalized mode: applies server defaults via dry-run (default, like kubectl diff)
    #[default]
    Normalized,

    /// Raw mode: compares raw manifests without server normalization (may show server defaults)
    Raw,
}

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

    /// Profile to use for diffing
    #[arg(short, long)]
    pub profile: Option<String>,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,

    /// Show summary only (counts, no detailed diff)
    #[arg(long)]
    pub summary: bool,

    /// Diff mode: 'normalized' (default) uses server-side apply to filter defaults,
    /// 'raw' compares manifests directly (may show server defaults)
    #[arg(long, default_value = "normalized")]
    pub mode: DiffMode,
}

pub async fn execute(args: DiffArgs) -> Result<()> {
    // 1. Render desired manifests
    let (raw_manifests, profile, _env_name, _credential_provider) = render_manifests(
        &args.path,
        args.component.as_deref(),
        args.profile.as_deref(),
        false, // offline
        None,  // cli_kube_version
        &[],   // cli_api_versions
    )
    .await?;

    if raw_manifests.is_empty() {
        tracing::info!("No manifests to diff");
        return Ok(());
    }

    // 2. Extract NylRelease metadata and filter it from manifests
    let (nyl_release, desired_manifests) = extract_nyl_release(&raw_manifests)?;

    // 3. Determine release name and namespace
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

    if desired_manifests.is_empty() {
        tracing::info!("No Kubernetes resources to diff (only NylRelease found)");
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
    let previous_release = storage.get_latest_release(&release_name, &release_namespace).await?;

    // 7. Compute diff against LIVE cluster state
    let diff_result =
        compute_diff_from_live(&kube_client, &desired_manifests, previous_release.as_ref(), args.mode).await?;

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
    modified: Vec<(ResourceKey, String)>, // (key, unified_diff_text)
    deleted: Vec<ResourceKey>,
    unchanged: Vec<ResourceKey>,
}

/// Compute diff between desired manifests and LIVE cluster state
async fn compute_diff_from_live(
    client: &dyn KubeClient,
    desired_manifests: &[serde_json::Value],
    previous_state: Option<&crate::kubernetes::ReleaseState>,
    mode: DiffMode,
) -> Result<DiffResult> {
    // Build set of desired resource keys
    let desired_keys: HashSet<ResourceKey> = desired_manifests
        .iter()
        .map(ResourceKey::from_json_value)
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
    let previous_keys: HashSet<ResourceKey> =
        previous_state.map_or_else(HashSet::new, |s| s.resource_keys.iter().cloned().collect());

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
            match mode {
                DiffMode::Normalized => {
                    // Normalized mode: normalize via dry-run apply (default)
                    // Fall back to raw mode if server normalization fails
                    match DiffEngine::are_equivalent_with_server(manifest, live, client).await {
                        Ok(true) => {
                            unchanged.push(key);
                        }
                        Ok(false) => match DiffEngine::diff_yaml_with_server(manifest, live, client).await {
                            Ok(diff_text) => {
                                modified.push((key, diff_text));
                            }
                            Err(e) => {
                                tracing::warn!("Server-side diff failed for {}, falling back to raw: {}", key, e);
                                let diff_text = DiffEngine::diff_yaml(manifest, live)?;
                                modified.push((key, diff_text));
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                "Server-side normalization failed for {}, falling back to raw: {}",
                                key,
                                e
                            );
                            if DiffEngine::are_equivalent(manifest, live)? {
                                unchanged.push(key);
                            } else {
                                let diff_text = DiffEngine::diff_yaml(manifest, live)?;
                                modified.push((key, diff_text));
                            }
                        }
                    }
                }
                DiffMode::Raw => {
                    // Raw mode: compare raw manifests (original behavior)
                    if DiffEngine::are_equivalent(manifest, live)? {
                        unchanged.push(key);
                    } else {
                        let diff_text = DiffEngine::diff_yaml(manifest, live)?;
                        modified.push((key, diff_text));
                    }
                }
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

/// Display summary line with colored counts
fn print_summary(diff: &DiffResult) {
    println!(
        "Summary: {} added, {} modified, {} deleted, {} unchanged",
        diff.added.len().to_string().green(),
        diff.modified.len().to_string().yellow(),
        diff.deleted.len().to_string().red(),
        diff.unchanged.len()
    );
}

/// Display diff results with kubectl-style unified diff output
fn display_diff(diff: &DiffResult) {
    // Show added resources
    for key in &diff.added {
        println!("{} {}", "+".green().bold(), key);
    }
    if !diff.added.is_empty() {
        println!();
    }

    // Show modified resources with unified diff (kubectl-style)
    for (key, unified_diff) in &diff.modified {
        println!("{} {}", "~".yellow().bold(), key);

        // Print unified diff with colors
        for line in unified_diff.lines() {
            if line.starts_with('+') && !line.starts_with("+++") {
                println!("{}", line.green());
            } else if line.starts_with('-') && !line.starts_with("---") {
                println!("{}", line.red());
            } else if line.starts_with("@@") {
                println!("{}", line.cyan());
            } else {
                println!("{}", line);
            }
        }
        println!();
    }

    // Show deleted resources
    for key in &diff.deleted {
        println!("{} {}", "-".red().bold(), key);
    }
    if !diff.deleted.is_empty() {
        println!();
    }

    // Summary with colors
    print_summary(diff);
}

/// Display summary only (counts, no detailed diff)
fn display_summary(diff: &DiffResult) {
    print_summary(diff);
}

#[cfg(test)]
mod tests {
    // Note: Full diff testing with live cluster requires integration tests
    // with MockKubeClient or a real cluster. Unit tests moved to the DiffEngine
    // module in kubernetes/diff.rs.
}
