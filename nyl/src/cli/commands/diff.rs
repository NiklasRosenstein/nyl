use clap::{Args, ValueEnum};
use colored::Colorize;
use std::collections::{HashMap, HashSet};

use crate::{
    cli::{
        commands::render::{run_render_preflight, ClusterClientRequirement, RenderOptions, RenderPreflightOptions},
        namespace_resolution::{adjust_duplicate_keys_for_namespace_resolution, resolve_manifest_namespaces},
    },
    kubernetes::{
        extract_name, DiffEngine, GroupVersionKind, KubeClient, KubernetesReleaseStorage, ReleaseStatus,
        ReleaseStorage, ResourceKey,
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

    /// Exit with code 1 if changes are found, 0 if no changes (like git diff --exit-code)
    #[arg(long)]
    pub exit_code: bool,
}

pub async fn execute(args: DiffArgs) -> Result<()> {
    if args.common.target.is_none() {
        return Err(NylError::config("nyl diff requires --target"));
    }
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

    // Resolve missing namespaces with release namespace hint for diff/apply parity.
    resolve_manifest_namespaces(&kube_client, &mut desired_manifests, Some(&release_namespace)).await?;
    duplicates =
        adjust_duplicate_keys_for_namespace_resolution(&kube_client, &duplicates, Some(&release_namespace)).await?;

    // Display duplicate resources warning if any
    if !duplicates.is_empty() {
        print_duplicate_warning(&duplicates);
    }

    // 5. Initialize state storage
    let storage = KubernetesReleaseStorage::new(client);

    // 6. Fetch previous release for tracking resource deletions
    let previous_release = storage.get_latest_release(&release_name, &release_namespace).await?;
    if previous_release.is_none() {
        tracing::warn!("{}", missing_release_warning_message(&release_namespace, &release_name));
    }

    // 6.5. Append-release mode: include previous release's resources in desired state
    let desired_manifests = if args.append_release {
        if let Some(ref prev_release) = previous_release {
            // Validate that previous release was successfully deployed
            // Only Deployed releases have complete resource sets safe to merge from
            if prev_release.status != ReleaseStatus::Deployed {
                return Err(NylError::Config(format!(
                    "Cannot use --append-release when previous release (revision {}) is in {:?} state. \
                     The previous release must be in Deployed state to safely merge resources.",
                    prev_release.revision, prev_release.status
                )));
            }
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
        compute_diff_from_live(&kube_client, &desired_manifests, previous_release.as_ref(), args.mode).await?;

    // 8. Display errors if any
    if !diff_result.errors.is_empty() {
        for (key, error) in &diff_result.errors {
            println!("{} {} {}", "✗".red().bold(), key, format!("({})", error).red());
        }
        println!();
    }

    // 9. Display diff
    if args.summary {
        display_summary(&diff_result, &duplicates);
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
        let has_changes =
            !diff_result.added.is_empty() || !diff_result.modified.is_empty() || !diff_result.deleted.is_empty();

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

/// Annotation attached to an added resource
#[derive(Debug)]
struct AddedNote {
    message: String,
    is_error: bool,
}

/// Diff result categorization
#[derive(Debug)]
struct DiffResult {
    added: Vec<(ResourceKey, Option<AddedNote>)>,
    modified: Vec<(ResourceKey, String, Option<String>)>, // (key, unified_diff_text, optional_error)
    deleted: Vec<ResourceKey>,
    unchanged: Vec<ResourceKey>,
    errors: Vec<(ResourceKey, String)>, // (key, error_message)
}

impl DiffResult {
    /// Count total errors including normalization failures and added resources with errors
    fn total_error_count(&self) -> usize {
        let normalization_errors = self.modified.iter().filter(|(_, _, err)| err.is_some()).count();
        let added_errors = self
            .added
            .iter()
            .filter(|(_, note)| note.as_ref().is_some_and(|n| n.is_error))
            .count();
        self.errors.len() + normalization_errors + added_errors
    }
}

/// Compute diff between desired manifests and LIVE cluster state
#[allow(clippy::too_many_lines)]
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
    let mut crd_not_found_keys: HashSet<ResourceKey> = HashSet::new();
    for manifest in desired_manifests {
        let key = ResourceKey::from_json_value(manifest)?;
        match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
            Ok(Some(resource)) => {
                let live_json = serde_json::to_value(&resource)?;
                live_resources.insert(key.clone(), live_json);
            }
            Ok(None) => {}
            Err(e) if e.is_api_resource_not_found_error() => {
                crd_not_found_keys.insert(key);
            }
            Err(e) => return Err(e),
        }
    }

    // Get previous resource keys for deletion tracking
    let previous_keys: HashSet<ResourceKey> =
        previous_state.map_or_else(HashSet::new, |s| s.resource_keys.iter().cloned().collect());

    // Also fetch resources from previous state that aren't in desired
    for key in &previous_keys {
        if !desired_keys.contains(key) {
            // This resource is being deleted - check if it still exists in cluster
            match client.get_resource(&key.gvk, key.namespace.as_deref(), &key.name).await {
                Ok(Some(resource)) => {
                    let live_json = serde_json::to_value(&resource)?;
                    live_resources.insert(key.clone(), live_json);
                }
                Ok(None) => {}
                Err(e) if e.is_api_resource_not_found_error() => {}
                Err(e) => return Err(e),
            }
        }
    }

    // Precompute the set of CRD (group, kind, version) tuples present in desired_manifests.
    let crd_set = build_crd_version_set(desired_manifests);

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
                            // Try raw comparison and annotate with error
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
            let note = if crd_not_found_keys.contains(&key) {
                if crd_in_manifests(&key.gvk, &crd_set) {
                    Some(AddedNote {
                        message: "CRD will be installed".to_string(),
                        is_error: false,
                    })
                } else {
                    Some(AddedNote {
                        message: "CRD not installed in cluster".to_string(),
                        is_error: true,
                    })
                }
            } else {
                None
            };
            added.push((key, note));
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
fn print_summary(diff: &DiffResult, duplicates: &HashMap<ResourceKey, usize>) {
    let total_errors = diff.total_error_count();
    let total_duplicates_ignored: usize = duplicates.values().map(|count| count - 1).sum();

    let mut parts = vec![
        format!("{} to add", diff.added.len().to_string().green()),
        format!("{} to modify", diff.modified.len().to_string().yellow()),
        format!("{} to delete", diff.deleted.len().to_string().red()),
        format!("{} unchanged", diff.unchanged.len()),
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

    if total_errors > 0 {
        parts.push(format!("{} failed", total_errors.to_string().red()));
    }

    println!("Summary: {}", parts.join(", "));
}

/// Display diff results with kubectl-style unified diff output
fn display_diff(diff: &DiffResult, duplicates: &HashMap<ResourceKey, usize>) {
    // Show added resources
    for (key, note) in &diff.added {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        let note_annotation = format_added_note(note.as_ref());
        println!("{} {}{}{}", "+".green().bold(), key, dup_annotation, note_annotation);
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
    print_summary(diff, duplicates);
}

/// Display resource list and counts, without unified diff content
fn display_summary(diff: &DiffResult, duplicates: &HashMap<ResourceKey, usize>) {
    for (key, note) in &diff.added {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        let note_annotation = format_added_note(note.as_ref());
        println!("{} {}{}{}", "+".green().bold(), key, dup_annotation, note_annotation);
    }

    for (key, _, error) in &diff.modified {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        let error_annotation = if let Some(err) = error {
            format!(" {}", format!("({})", err).red())
        } else {
            String::new()
        };
        println!("{} {}{}{}", "~".yellow().bold(), key, dup_annotation, error_annotation);
    }

    for key in &diff.deleted {
        let dup_annotation = get_duplicate_annotation_for_key(key, duplicates);
        println!("{} {}{}", "-".red().bold(), key, dup_annotation);
    }

    if !diff.added.is_empty() || !diff.modified.is_empty() || !diff.deleted.is_empty() {
        println!();
    }

    print_summary(diff, duplicates);
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

fn format_added_note(note: Option<&AddedNote>) -> String {
    match note {
        Some(n) if n.is_error => format!(" {}", format!("({})", n.message).red()),
        Some(n) => format!(" {}", format!("({})", n.message).yellow()),
        None => String::new(),
    }
}

/// Get duplicate annotation for a ResourceKey if it's a duplicate
fn get_duplicate_annotation_for_key(key: &ResourceKey, duplicates: &HashMap<ResourceKey, usize>) -> String {
    if let Some(count) = duplicates.get(key) {
        let ignored_count = count - 1;
        let plural = if ignored_count == 1 { "duplicate" } else { "duplicates" };
        return format!(" {}", format!("({} {} ignored)", ignored_count, plural).yellow());
    }
    String::new()
}

/// Build a set of (group, kind, version) tuples from CRDs present in the manifests.
/// For v1 CRDs only versions with `served: true` are included.
/// For v1beta1 CRDs both `spec.version` (single) and `spec.versions[]` are checked.
fn build_crd_version_set(manifests: &[serde_json::Value]) -> HashSet<(String, String, String)> {
    let mut set = HashSet::new();
    for m in manifests {
        let api_version = m.get("apiVersion").and_then(|v| v.as_str()).unwrap_or("");
        let kind = m.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "CustomResourceDefinition"
            || (api_version != "apiextensions.k8s.io/v1" && api_version != "apiextensions.k8s.io/v1beta1")
        {
            continue;
        }
        let spec = m.get("spec");
        let Some(group) = spec.and_then(|s| s.get("group")).and_then(|g| g.as_str()) else {
            continue;
        };
        let Some(crd_kind) = spec
            .and_then(|s| s.get("names"))
            .and_then(|n| n.get("kind"))
            .and_then(|k| k.as_str())
        else {
            continue;
        };
        if api_version == "apiextensions.k8s.io/v1" {
            if let Some(versions) = spec.and_then(|s| s.get("versions")).and_then(|v| v.as_array()) {
                for ver in versions {
                    let served = ver.get("served").and_then(|s| s.as_bool()).unwrap_or(false);
                    if !served {
                        continue;
                    }
                    if let Some(ver_name) = ver.get("name").and_then(|v| v.as_str()) {
                        set.insert((group.to_string(), crd_kind.to_string(), ver_name.to_string()));
                    }
                }
            }
        } else {
            // v1beta1: spec.version (single string) or spec.versions (array)
            if let Some(ver) = spec.and_then(|s| s.get("version")).and_then(|v| v.as_str()) {
                set.insert((group.to_string(), crd_kind.to_string(), ver.to_string()));
            }
            if let Some(versions) = spec.and_then(|s| s.get("versions")).and_then(|v| v.as_array()) {
                for ver_entry in versions {
                    if let Some(ver_name) = ver_entry.get("name").and_then(|v| v.as_str()) {
                        set.insert((group.to_string(), crd_kind.to_string(), ver_name.to_string()));
                    }
                }
            }
        }
    }
    set
}

fn crd_in_manifests(gvk: &GroupVersionKind, crd_set: &HashSet<(String, String, String)>) -> bool {
    crd_set.contains(&(gvk.group.clone(), gvk.kind.clone(), gvk.version.clone()))
}

fn missing_release_warning_message(namespace: &str, release_name: &str) -> String {
    format!(
        "No previous release state found for {}/{}. Diff can only compare current desired resources; prune candidates cannot be determined.",
        namespace, release_name
    )
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
    let mut missing_count = 0;
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
                missing_count += 1;
            }
        }
    }

    // Calculate overlap (resources in both previous and current)
    let overlap = previous_release.resource_keys.len() - added_count - missing_count;
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
    use super::*;
    use crate::kubernetes::ApplyOutcome;
    use async_trait::async_trait;
    use kube::api::DynamicObject;
    use serde_json::json;

    #[test]
    fn test_missing_release_warning_message_mentions_prune_limitation() {
        let msg = missing_release_warning_message("default", "demo");
        assert!(msg.contains("default/demo"));
        assert!(msg.contains("prune candidates cannot be determined"));
    }

    #[test]
    fn test_build_crd_version_set_v1_served_only() {
        let manifests = vec![json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "spec": {
                "group": "example.com",
                "names": { "kind": "MyResource" },
                "versions": [
                    { "name": "v1", "served": true, "storage": true },
                    { "name": "v1beta1", "served": false, "storage": false }
                ]
            }
        })];
        let set = build_crd_version_set(&manifests);
        assert!(set.contains(&("example.com".to_string(), "MyResource".to_string(), "v1".to_string())));
        assert!(!set.contains(&(
            "example.com".to_string(),
            "MyResource".to_string(),
            "v1beta1".to_string()
        )));
    }

    #[test]
    fn test_build_crd_version_set_v1beta1_single_version() {
        let manifests = vec![json!({
            "apiVersion": "apiextensions.k8s.io/v1beta1",
            "kind": "CustomResourceDefinition",
            "spec": {
                "group": "example.com",
                "names": { "kind": "MyResource" },
                "version": "v1alpha1"
            }
        })];
        let set = build_crd_version_set(&manifests);
        assert!(set.contains(&(
            "example.com".to_string(),
            "MyResource".to_string(),
            "v1alpha1".to_string()
        )));
    }

    #[test]
    fn test_crd_in_manifests_matches_group_kind_and_version() {
        let manifests = vec![json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "spec": {
                "group": "example.com",
                "names": { "kind": "MyResource" },
                "versions": [{ "name": "v1", "served": true, "storage": true }]
            }
        })];
        let set = build_crd_version_set(&manifests);

        let matching_gvk = GroupVersionKind {
            group: "example.com".to_string(),
            version: "v1".to_string(),
            kind: "MyResource".to_string(),
        };
        assert!(crd_in_manifests(&matching_gvk, &set));

        let wrong_version = GroupVersionKind {
            group: "example.com".to_string(),
            version: "v2".to_string(),
            kind: "MyResource".to_string(),
        };
        assert!(!crd_in_manifests(&wrong_version, &set));

        let wrong_group = GroupVersionKind {
            group: "other.io".to_string(),
            version: "v1".to_string(),
            kind: "MyResource".to_string(),
        };
        assert!(!crd_in_manifests(&wrong_group, &set));
    }

    /// A KubeClient that returns ApiResourceNotFound for a specific set of GVKs and Ok(None) for all others.
    struct SelectiveApiResourceNotFoundClient {
        error_gvks: HashSet<(String, String, String)>,
    }

    impl SelectiveApiResourceNotFoundClient {
        fn new(error_gvks: impl IntoIterator<Item = (&'static str, &'static str, &'static str)>) -> Self {
            Self {
                error_gvks: error_gvks
                    .into_iter()
                    .map(|(g, v, k)| (g.to_string(), v.to_string(), k.to_string()))
                    .collect(),
            }
        }
    }

    #[async_trait]
    impl KubeClient for SelectiveApiResourceNotFoundClient {
        async fn get_resource(
            &self,
            gvk: &GroupVersionKind,
            _namespace: Option<&str>,
            _name: &str,
        ) -> Result<Option<DynamicObject>> {
            if self
                .error_gvks
                .contains(&(gvk.group.clone(), gvk.version.clone(), gvk.kind.clone()))
            {
                Err(NylError::ApiResourceNotFound(format!(
                    "{}/{}/{}",
                    gvk.group, gvk.version, gvk.kind
                )))
            } else {
                Ok(None)
            }
        }
        async fn apply_resource(
            &self,
            _resource: &DynamicObject,
            _field_manager: &str,
            _dry_run: bool,
        ) -> Result<ApplyOutcome> {
            Err(NylError::Other("not used".to_string()))
        }
        async fn get_server_version(&self) -> Result<String> {
            Ok("1.30.0".to_string())
        }
        async fn get_api_versions(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn is_namespaced(&self, _gvk: &GroupVersionKind) -> Result<bool> {
            Ok(true)
        }
        fn default_namespace(&self) -> &'static str {
            "default"
        }
        async fn delete_resource(&self, _gvk: &GroupVersionKind, _namespace: Option<&str>, _name: &str) -> Result<()> {
            Ok(())
        }
        async fn get_normalized_resource(
            &self,
            resource: &DynamicObject,
            _field_manager: &str,
        ) -> Result<DynamicObject> {
            Ok(resource.clone())
        }
    }

    #[tokio::test]
    async fn test_diff_api_resource_not_found_crd_will_be_installed() {
        let crd = json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": { "name": "mypolicies.kyverno.io" },
            "spec": {
                "group": "kyverno.io",
                "names": { "kind": "ClusterPolicy", "plural": "clusterpolicies" },
                "versions": [{ "name": "v1", "served": true, "storage": true }],
                "scope": "Cluster"
            }
        });
        let cr = json!({
            "apiVersion": "kyverno.io/v1",
            "kind": "ClusterPolicy",
            "metadata": { "name": "my-policy" }
        });
        let manifests = vec![crd, cr];
        // Only the CR's GVK is unknown; the CRD manifest type is always available.
        let client = SelectiveApiResourceNotFoundClient::new([("kyverno.io", "v1", "ClusterPolicy")]);
        let result = compute_diff_from_live(&client, &manifests, None, DiffMode::Raw)
            .await
            .unwrap();

        // The CR should be in `added` with a non-error note ("CRD will be installed")
        let cr_entry = result.added.iter().find(|(k, _)| k.gvk.kind == "ClusterPolicy");
        assert!(cr_entry.is_some(), "ClusterPolicy should appear in added");
        let note = cr_entry.unwrap().1.as_ref().expect("should have a note");
        assert!(!note.is_error, "note should not be an error when CRD is in manifests");
        assert!(note.message.contains("CRD will be installed"));
        assert_eq!(result.total_error_count(), 0);
    }

    #[tokio::test]
    async fn test_diff_api_resource_not_found_crd_not_installed() {
        let cr = json!({
            "apiVersion": "kyverno.io/v1",
            "kind": "ClusterPolicy",
            "metadata": { "name": "my-policy" }
        });
        let manifests = vec![cr];
        let client = SelectiveApiResourceNotFoundClient::new([("kyverno.io", "v1", "ClusterPolicy")]);
        let result = compute_diff_from_live(&client, &manifests, None, DiffMode::Raw)
            .await
            .unwrap();

        let cr_entry = result.added.iter().find(|(k, _)| k.gvk.kind == "ClusterPolicy");
        assert!(cr_entry.is_some(), "ClusterPolicy should appear in added");
        let note = cr_entry.unwrap().1.as_ref().expect("should have a note");
        assert!(note.is_error, "note should be an error when CRD is not in manifests");
        assert!(note.message.contains("CRD not installed"));
        assert_eq!(result.total_error_count(), 1);
    }

    #[tokio::test]
    async fn test_diff_api_resource_not_found_wrong_version_counts_as_error() {
        // CRD in manifests but only serves v1beta1, resource requests v1
        let crd = json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": { "name": "mypolicies.kyverno.io" },
            "spec": {
                "group": "kyverno.io",
                "names": { "kind": "ClusterPolicy", "plural": "clusterpolicies" },
                "versions": [{ "name": "v1beta1", "served": true, "storage": true }],
                "scope": "Cluster"
            }
        });
        let cr = json!({
            "apiVersion": "kyverno.io/v1",
            "kind": "ClusterPolicy",
            "metadata": { "name": "my-policy" }
        });
        let manifests = vec![crd, cr];
        let client = SelectiveApiResourceNotFoundClient::new([("kyverno.io", "v1", "ClusterPolicy")]);
        let result = compute_diff_from_live(&client, &manifests, None, DiffMode::Raw)
            .await
            .unwrap();

        let cr_entry = result.added.iter().find(|(k, _)| k.gvk.kind == "ClusterPolicy");
        assert!(cr_entry.is_some());
        let note = cr_entry.unwrap().1.as_ref().expect("should have a note");
        assert!(note.is_error, "should be an error: CRD in manifests but wrong version");
        assert_eq!(result.total_error_count(), 1);
    }
}
