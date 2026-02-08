use clap::{Args, ValueEnum};
use colored::Colorize;
use kube::Client;
use std::collections::{HashMap, HashSet};

use crate::{
    cli::commands::render::{render_manifests_complete, RenderOptions},
    kubernetes::{
        extract_name, DiffEngine, KubeClient, KubeRsClient, KubernetesReleaseStorage, ReleaseStorage, ResourceKey,
    },
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

    /// Show summary only (counts, no detailed diff)
    #[arg(long)]
    pub summary: bool,

    /// Diff mode: 'normalized' (default) uses server-side apply to filter defaults,
    /// 'raw' compares manifests directly (may show server defaults)
    #[arg(long, default_value = "normalized")]
    pub mode: DiffMode,

    /// Preview append-release mode: show diff as if merging with previous release.
    /// When enabled, the diff will show resources from both the current apply and previous release,
    /// simulating what --append-release would do in the apply command.
    #[arg(long)]
    pub append_release: bool,

    /// Strict mode: fail immediately on first error (exit code 2)
    #[arg(long)]
    pub strict: bool,

    /// Exit with code 1 if changes are found, 0 if no changes (like git diff --exit-code)
    #[arg(long)]
    pub exit_code: bool,
}

pub async fn execute(args: DiffArgs) -> Result<()> {
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
        tracing::info!("No manifests to diff");
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

    // 6.5. Append-release mode: include previous release's resources in desired state
    let desired_manifests = if args.append_release {
        if let Some(ref prev_release) = previous_release {
            merge_with_previous_release(&kube_client, desired_manifests, prev_release).await?
        } else {
            tracing::info!("Append-release mode: no previous release found, showing diff as initial release");
            desired_manifests
        }
    } else {
        desired_manifests
    };

    // 7. Compute diff against LIVE cluster state
    let diff_result =
        compute_diff_from_live(&kube_client, &desired_manifests, previous_release.as_ref(), args.mode, args.strict).await?;

    // 8. Display errors if any
    if !diff_result.errors.is_empty() {
        for (key, error) in &diff_result.errors {
            println!("{} {} {}", "✗".red().bold(), key, format!("({})", error).red());
        }
        println!();
    }

    // 9. Display diff
    if args.summary {
        display_summary(&diff_result);
    } else {
        display_diff(&diff_result, &duplicates);
    }

    // 10. Determine exit code
    // Exit 2 if there were errors (including normalization failures)
    let total_errors = diff_result.total_error_count();
    if total_errors > 0 {
        tracing::error!("Diff completed with {} error(s)", total_errors);
        std::process::exit(2);
    }

    // Exit 1 if --exit-code and there are changes
    if args.exit_code {
        let has_changes = !diff_result.added.is_empty()
            || !diff_result.modified.is_empty()
            || !diff_result.deleted.is_empty();

        if has_changes {
            // Use a special error type to indicate "changes found" vs actual error
            std::process::exit(1);
        }
    }

    // Exit 0 otherwise
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
    modified: Vec<(ResourceKey, String, Option<String>)>, // (key, unified_diff_text, optional_error)
    deleted: Vec<ResourceKey>,
    unchanged: Vec<ResourceKey>,
    errors: Vec<(ResourceKey, String)>, // (key, error_message)
}

impl DiffResult {
    /// Count total errors including normalization failures
    fn total_error_count(&self) -> usize {
        let normalization_errors = self.modified.iter().filter(|(_, _, err)| err.is_some()).count();
        self.errors.len() + normalization_errors
    }
}

/// Compute diff between desired manifests and LIVE cluster state
async fn compute_diff_from_live(
    client: &dyn KubeClient,
    desired_manifests: &[serde_json::Value],
    previous_state: Option<&crate::kubernetes::ReleaseState>,
    mode: DiffMode,
    strict: bool,
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
    let mut errors = Vec::new();

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
                                modified.push((key, diff_text, None));
                            }
                            Err(e) => {
                                let diff_text = DiffEngine::diff_yaml(manifest, live)?;
                                let error_msg = format!("failed to normalize resource: {}", e);
                                modified.push((key, diff_text, Some(error_msg)));
                            }
                        },
                        Err(e) => {
                            if strict {
                                // In strict mode, fail immediately on errors
                                return Err(e);
                            } else {
                                // In non-strict mode, try raw comparison and annotate with error
                                let error_msg = format!("failed to normalize resource: {}", e);
                                match DiffEngine::are_equivalent(manifest, live) {
                                    Ok(true) => unchanged.push(key),
                                    Ok(false) => match DiffEngine::diff_yaml(manifest, live) {
                                        Ok(diff_text) => modified.push((key, diff_text, Some(error_msg))),
                                        Err(_diff_err) => {
                                            errors.push((key, error_msg));
                                        }
                                    },
                                    Err(_eq_err) => {
                                        errors.push((key, error_msg));
                                    }
                                }
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
                        modified.push((key, diff_text, None));
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
        errors,
    })
}

/// Display summary line with colored counts
fn print_summary(diff: &DiffResult) {
    let total_errors = diff.total_error_count();
    if total_errors == 0 {
        println!(
            "Summary: {} to add, {} to modify, {} to delete, {} unchanged",
            diff.added.len().to_string().green(),
            diff.modified.len().to_string().yellow(),
            diff.deleted.len().to_string().red(),
            diff.unchanged.len()
        );
    } else {
        println!(
            "Summary: {} to add, {} to modify, {} to delete, {} unchanged, {} failed",
            diff.added.len().to_string().green(),
            diff.modified.len().to_string().yellow(),
            diff.deleted.len().to_string().red(),
            diff.unchanged.len(),
            total_errors.to_string().red()
        );
    }
}

/// Display diff results with kubectl-style unified diff output
fn display_diff(diff: &DiffResult, duplicates: &HashMap<ResourceKey, usize>) {
    // Show added resources
    for key in &diff.added {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        println!("{} {}{}", "+".green().bold(), key, dup_annotation);
    }
    if !diff.added.is_empty() {
        println!();
    }

    // Show modified resources with unified diff (kubectl-style)
    for (key, unified_diff, error) in &diff.modified {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        let error_annotation = if let Some(err) = error {
            format!(" {}", format!("({})", err).red())
        } else {
            String::new()
        };
        println!("{} {}{}{}", "~".yellow().bold(), key, dup_annotation, error_annotation);

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
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        println!("{} {}{}", "-".red().bold(), key, dup_annotation);
    }
    if !diff.deleted.is_empty() {
        println!();
    }

    // Show unchanged resources
    for key in &diff.unchanged {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        println!("{} {}{}", "=".bright_black().bold(), key, dup_annotation);
    }
    if !diff.unchanged.is_empty() {
        println!();
    }

    // Summary with colors
    print_summary(diff);
}

/// Display summary only (counts, no detailed diff)
fn display_summary(diff: &DiffResult) {
    print_summary(diff);
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

/// Get duplicate annotation for a ResourceKey if it's a duplicate
fn get_duplicate_annotation_for_key(key: &ResourceKey, duplicates: &HashMap<ResourceKey, usize>) -> String {
    if let Some(count) = duplicates.get(key) {
        let ignored_count = count - 1;
        let plural = if ignored_count == 1 { "duplicate" } else { "duplicates" };
        return format!(" {}", format!("({} ignored {})", ignored_count, plural).yellow());
    }
    String::new()
}

/// Merge current manifests with previous release's resources (for --append-release preview)
async fn merge_with_previous_release(
    client: &dyn KubeClient,
    mut current_manifests: Vec<serde_json::Value>,
    previous_release: &crate::kubernetes::ReleaseState,
) -> Result<Vec<serde_json::Value>> {
    // Build set of current resource keys
    let current_keys: HashSet<ResourceKey> = current_manifests
        .iter()
        .map(ResourceKey::from_json_value)
        .collect::<Result<_>>()?;

    // Fetch previous resources that are not in current manifests
    let mut added_count = 0;
    for prev_key in &previous_release.resource_keys {
        if !current_keys.contains(prev_key) {
            // This resource is in previous release but not in current - fetch it from cluster
            if let Some(resource) = client
                .get_resource(&prev_key.gvk, prev_key.namespace.as_deref(), &prev_key.name)
                .await?
            {
                // Convert DynamicObject to JSON
                let resource_json = serde_json::to_value(&resource)?;
                current_manifests.push(resource_json);
                added_count += 1;
            } else {
                tracing::debug!("Previous resource {} no longer exists in cluster, skipping", prev_key);
            }
        }
    }

    // Calculate overlap for better logging
    let overlap = previous_release.resource_keys.len() - added_count;
    if overlap > 0 {
        tracing::info!(
            "Append-release mode: merged {} from previous + {} current ({} overlap, {} total)",
            added_count,
            current_keys.len(),
            overlap,
            current_manifests.len()
        );
    } else {
        tracing::info!(
            "Append-release mode: merged {} from previous + {} current ({} total)",
            added_count,
            current_keys.len(),
            current_manifests.len()
        );
    }

    Ok(current_manifests)
}

#[cfg(test)]
mod tests {
    // Note: Full diff testing with live cluster requires integration tests
    // with MockKubeClient or a real cluster. Unit tests moved to the DiffEngine
    // module in kubernetes/diff.rs.
}
