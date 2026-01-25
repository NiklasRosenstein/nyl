use clap::Args;
use std::path::Path;
use walkdir::WalkDir;

use crate::{
    config::ProjectConfig,
    generator::Generator,
    helm::{HelmChartResolver, HelmTemplateExecutor},
    kubernetes::{KubeClient, KubeRsClient},
    profiles::{deep_merge_value, Profile, ProfileConfig},
    resources::{extract_application_generators, extract_nyl_release, HelmChart, NylRelease, ReleaseMetadata},
    secrets::SecretsConfig,
    template::TemplateContext,
    NylError, Result,
};

/// Render Kubernetes manifests to stdout
#[derive(Args, Debug)]
pub struct RenderArgs {
    /// Path to the project directory
    #[arg(default_value = ".")]
    pub path: String,

    /// Component to render (if not specified, renders all)
    #[arg(short, long)]
    pub component: Option<String>,

    /// Profile to use for rendering
    #[arg(short, long, conflicts_with = "offline")]
    pub profile: Option<String>,

    /// Offline mode: skip profile loading, use default profile
    #[arg(long)]
    pub offline: bool,

    /// Kubernetes version for Helm templating (required with --offline)
    #[arg(long, required_if_eq("offline", "true"))]
    pub kube_version: Option<String>,

    /// Kubernetes API versions for Helm (required with --offline, comma-separated or repeated)
    #[arg(long, required_if_eq("offline", "true"), value_delimiter = ',')]
    pub kube_api_versions: Vec<String>,
}

/// Output format for rendered manifests
#[derive(Debug, Clone, Copy)]
enum OutputFormat {
    Yaml,
    #[allow(dead_code)]
    Json,
}

/// Shared manifest rendering logic used by render, diff, and apply
pub async fn render_manifests(
    path: &str,
    component: Option<&str>,
    environment: Option<&str>,
    offline: bool,
    cli_kube_version: Option<&str>,
    cli_api_versions: &[String],
) -> Result<(Vec<serde_json::Value>, Profile, String)> {
    // 1. Load project configuration (with warning if not found)
    let project_config = ProjectConfig::load_with_warning(None)?;

    // 2. Select environment/profile
    let (profile, env_name): (Profile, String) = if offline {
        // Offline mode: skip profile loading, use default profile
        (Profile::default(), "offline".to_string())
    } else {
        // Load profile configuration
        let profile_config = ProfileConfig::load(None)?;

        // If user explicitly specified a profile, require it to exist.
        // If no profile specified and no profiles configured, use default Profile (current kubecontext).
        let env_name = environment.unwrap_or("default");
        let profile: Profile = if let Some(p) = profile_config.get(env_name) {
            p.clone()
        } else if environment.is_some() {
            // User explicitly requested a profile that doesn't exist
            return Err(NylError::Config(format!("Profile '{}' not found", env_name)));
        } else if profile_config.profiles.is_empty() {
            // No profiles configured at all - use default (current kubecontext)
            Profile::default()
        } else {
            // Profiles exist but "default" doesn't - user must specify which one
            return Err(NylError::Config(format!(
                "Profile '{}' not found. Available profiles: {}",
                env_name,
                profile_config.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        };
        (profile, env_name.to_string())
    };

    // 3. Load secrets
    let secrets_config = SecretsConfig::load(None)?;

    // 4. Build template context
    let context = TemplateContext::build(&profile, &secrets_config, &env_name)?;

    // 5. Create generator
    let generator = Generator::new(project_config.clone());

    // 6. Load and filter resources
    let resources = load_resources(path)?;
    let filtered = filter_resources(resources, component)?;

    // 7. Check if any HelmChart resources exist (need cluster info)
    let has_helm_charts = filtered.iter().any(|r| {
        let kind = r.get("kind").and_then(|k| k.as_str());
        let api_version = r.get("apiVersion").and_then(|a| a.as_str());
        kind == Some("HelmChart") && api_version.is_some_and(|v| v.starts_with("v1."))
    });

    // 8. Determine kube_version and api_versions (only if needed)
    let (kube_version, api_versions) = if !has_helm_charts {
        // No HelmCharts, version info not needed
        (String::new(), Vec::new())
    } else if offline {
        // In offline mode, use CLI arguments (required by clap)
        (
            cli_kube_version.unwrap_or_default().to_string(),
            cli_api_versions.to_vec(),
        )
    } else {
        // In non-offline mode, fetch from cluster unless CLI args override
        let client = KubeRsClient::from_profile(&profile, None).await?;
        let kube_version = if let Some(v) = cli_kube_version {
            v.to_string()
        } else {
            client.get_server_version().await?
        };
        let api_versions = if cli_api_versions.is_empty() {
            client.get_api_versions().await?
        } else {
            cli_api_versions.to_vec()
        };
        (kube_version, api_versions)
    };

    // 9. Generate manifests
    let mut all_manifests = Vec::new();
    for resource in filtered {
        let manifests = generate_resource(
            &generator,
            &resource,
            &context,
            &project_config,
            &kube_version,
            &api_versions,
        )?;
        all_manifests.extend(manifests);
    }

    Ok((all_manifests, profile, env_name))
}

pub async fn execute(args: RenderArgs) -> Result<()> {
    let (manifests, _, _) = render_manifests(
        &args.path,
        args.component.as_deref(),
        args.profile.as_deref(),
        args.offline,
        args.kube_version.as_deref(),
        &args.kube_api_versions,
    )
    .await?;

    // Extract ApplicationGenerator resources and filter them from output
    let (generators, mut final_manifests) = extract_application_generators(&manifests)?;

    // Process each ApplicationGenerator
    for generator in generators {
        let applications = process_application_generator(&generator, &args.path)?;
        final_manifests.extend(applications);
    }

    output_manifests(&final_manifests, OutputFormat::Yaml)?;
    Ok(())
}

/// Load all YAML/JSON resources from a directory
fn load_resources(path: &str) -> Result<Vec<serde_json::Value>> {
    let path = Path::new(path);
    let mut resources = Vec::new();

    for entry in WalkDir::new(path).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        let file_path = entry.path();
        if !file_path.is_file() {
            continue;
        }

        let ext = file_path.extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("yaml" | "yml" | "json")) {
            continue;
        }

        let content =
            std::fs::read_to_string(file_path).map_err(|e| NylError::Config(format!("Failed to read file: {}", e)))?;
        let docs = parse_yaml_documents(&content)?;
        resources.extend(docs);
    }

    Ok(resources)
}

/// Parse YAML multi-document stream
fn parse_yaml_documents(yaml_str: &str) -> Result<Vec<serde_json::Value>> {
    let mut documents = Vec::new();

    for doc in yaml_str.split("\n---\n") {
        let trimmed = doc.trim();

        // Skip empty or comment-only documents
        if trimmed.is_empty()
            || trimmed
                .lines()
                .all(|line| line.trim().starts_with('#') || line.trim().is_empty())
        {
            continue;
        }

        let value: serde_json::Value = serde_norway::from_str(trimmed).map_err(NylError::Yaml)?;

        if !value.is_null() {
            documents.push(value);
        }
    }

    Ok(documents)
}

/// Filter resources by component type
fn filter_resources(
    resources: Vec<serde_json::Value>,
    component_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    if let Some(filter) = component_filter {
        Ok(resources
            .into_iter()
            .filter(|r| {
                let kind = r.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                let api_version = r.get("apiVersion").and_then(|a| a.as_str()).unwrap_or("");
                filter == kind || filter == format!("{}/{}", api_version, kind)
            })
            .collect())
    } else {
        Ok(resources)
    }
}

/// Generate manifests from a resource
fn generate_resource(
    _generator: &Generator,
    resource: &serde_json::Value,
    context: &TemplateContext,
    config: &ProjectConfig,
    kube_version: &str,
    api_versions: &[String],
) -> Result<Vec<serde_json::Value>> {
    // Check if it's a HelmChart resource
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());

    if kind == Some("HelmChart") && api_version.is_some_and(|v| v.starts_with("v1.")) {
        // Parse as HelmChart and render
        let chart: HelmChart = serde_json::from_value(resource.clone())
            .map_err(|e| NylError::Config(format!("Failed to parse HelmChart: {}", e)))?;
        render_helm_chart(&chart, context, config, kube_version, api_versions)
    } else {
        // For Phase 3, pass through other resources as-is
        // Phase 4+: Use generator for component instantiation
        Ok(vec![resource.clone()])
    }
}

/// Render a Helm chart
fn render_helm_chart(
    chart: &HelmChart,
    context: &TemplateContext,
    config: &ProjectConfig,
    kube_version: &str,
    api_versions: &[String],
) -> Result<Vec<serde_json::Value>> {
    let working_dir =
        std::env::current_dir().map_err(|e| NylError::Config(format!("Failed to get current directory: {}", e)))?;

    let resolver = HelmChartResolver::new(config.config.settings.search_path.clone(), working_dir);
    let resolved = resolver.resolve_chart(&chart.spec.chart)?;

    // Merge context values into chart values
    let merged_values = deep_merge_value(Some(chart.spec.values.clone()), context.values.clone());

    let release = chart
        .spec
        .release
        .clone()
        .unwrap_or_else(|| ReleaseMetadata::new(chart.effective_release_name()));

    let executor = HelmTemplateExecutor::new()
        .with_kube_version(kube_version.to_string())
        .with_api_versions(api_versions.to_vec());

    executor.template(&resolved, &release, &merged_values)
}

/// Output manifests in the specified format
fn output_manifests(manifests: &[serde_json::Value], format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Yaml => {
            for (i, manifest) in manifests.iter().enumerate() {
                if i > 0 {
                    println!("---");
                }
                let yaml = serde_norway::to_string(manifest).map_err(NylError::Yaml)?;
                print!("{}", yaml);
            }
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(manifests)
                .map_err(|e| NylError::Config(format!("Failed to serialize JSON: {}", e)))?;
            println!("{}", json);
        }
    }
    Ok(())
}

/// Process ApplicationGenerator - scan directory and generate Applications
fn process_application_generator(
    generator: &crate::resources::ApplicationGenerator,
    _base_dir: &str,
) -> Result<Vec<serde_json::Value>> {
    // Clone Git repository and resolve to local path
    let mut git_manager = crate::git::GitManager::new()?;

    let source_path = git_manager.resolve_ref(
        &generator.spec.source.repo_url,
        Some(&generator.spec.source.target_revision),
        Some(&generator.spec.source.path),
    )?;

    // Find YAML files matching filters
    let yaml_files = find_yaml_files_filtered(
        &source_path,
        &generator.spec.source.include,
        &generator.spec.source.exclude,
    )?;

    let mut applications = Vec::new();

    for file_path in yaml_files {
        // Read and parse file
        let content = std::fs::read_to_string(&file_path)
            .map_err(|e| NylError::Config(format!("Failed to read file {}: {}", file_path.display(), e)))?;
        let docs = parse_yaml_documents(&content)?;

        // Extract NylRelease
        let (nyl_release, _) = extract_nyl_release(&docs)?;

        if let Some(release) = nyl_release {
            // Generate ArgoCD Application
            let app = create_argocd_application_from_generator(&release, &file_path, &source_path, generator)?;
            applications.push(app);
        }
        // Skip files without NylRelease (no warning to avoid noise)
    }

    Ok(applications)
}

/// Find YAML files matching include/exclude patterns
fn find_yaml_files_filtered(dir: &Path, include: &[String], exclude: &[String]) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();

    if !dir.exists() {
        return Err(NylError::Config(format!(
            "Source path does not exist: {}",
            dir.display()
        )));
    }

    for entry in WalkDir::new(dir).follow_links(true) {
        let entry = entry.map_err(|e| NylError::Config(format!("Failed to walk directory: {}", e)))?;
        let path = entry.path();

        // Skip if not a file
        if !path.is_file() {
            continue;
        }

        // Skip if doesn't match include patterns
        if !matches_glob_patterns(path, include)? {
            continue;
        }

        // Skip if matches exclude patterns
        if matches_glob_patterns(path, exclude)? {
            continue;
        }

        files.push(path.to_path_buf());
    }

    Ok(files)
}

/// Check if path matches any glob pattern
fn matches_glob_patterns(path: &Path, patterns: &[String]) -> Result<bool> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    for pattern in patterns {
        // Simple glob matching (*.yaml, .*, _*, etc.)
        if let Some(ext) = pattern.strip_prefix("*.") {
            // Extension match: *.yaml
            if file_name.ends_with(ext) {
                return Ok(true);
            }
        } else if pattern == ".*" {
            // Hidden files: .*
            if file_name.starts_with('.') {
                return Ok(true);
            }
        } else if pattern.starts_with('_') && pattern.ends_with('*') {
            // Prefix match: _*
            let prefix = &pattern[..pattern.len() - 1];
            if file_name.starts_with(prefix) {
                return Ok(true);
            }
        } else if pattern.ends_with('*') {
            // Generic prefix match: test*
            let prefix = &pattern[..pattern.len() - 1];
            if file_name.starts_with(prefix) {
                return Ok(true);
            }
        } else if file_name == pattern {
            // Exact match
            return Ok(true);
        }
    }

    Ok(false)
}

/// Create ArgoCD Application from generator config
fn create_argocd_application_from_generator(
    release: &NylRelease,
    file_path: &Path,
    base_path: &Path,
    generator: &crate::resources::ApplicationGenerator,
) -> Result<serde_json::Value> {
    // Calculate relative path from base
    let rel_path = file_path
        .strip_prefix(base_path)
        .unwrap_or(file_path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_str()
        .ok_or_else(|| NylError::Config("Invalid file path".to_string()))?;

    // Use relative path or "." if empty
    let path_str = if rel_path.is_empty() { "." } else { rel_path };

    // Build the Application manifest
    let mut app = serde_json::json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "Application",
        "metadata": {
            "name": release.metadata.name,
            "namespace": generator.spec.destination.namespace,
        },
        "spec": {
            "project": generator.spec.project,
            "source": {
                "repoURL": generator.spec.source.repo_url,
                "path": path_str,
                "targetRevision": generator.spec.source.target_revision,
                "plugin": {
                    "name": "nyl",
                    "env": [
                        {"name": "NYL_RELEASE_NAME", "value": release.metadata.name},
                        {"name": "NYL_RELEASE_NAMESPACE", "value": release.metadata.namespace},
                    ],
                },
            },
            "destination": {
                "server": generator.spec.destination.server,
                "namespace": release.metadata.namespace,
            },
        },
    });

    // Add labels if present
    if !generator.spec.labels.is_empty() {
        app["metadata"]["labels"] = serde_json::to_value(&generator.spec.labels)?;
    }

    // Add annotations if present
    if !generator.spec.annotations.is_empty() {
        app["metadata"]["annotations"] = serde_json::to_value(&generator.spec.annotations)?;
    }

    // Add sync policy if present
    if let Some(ref sync_policy) = generator.spec.sync_policy {
        app["spec"]["syncPolicy"] = serde_json::to_value(sync_policy)?;
    }

    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_yaml_documents_single() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test
";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_parse_yaml_documents_multiple() {
        let yaml = r"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test1
---
apiVersion: v1
kind: Service
metadata:
  name: test2
";
        let docs = parse_yaml_documents(yaml).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["kind"], "ConfigMap");
        assert_eq!(docs[1]["kind"], "Service");
    }

    #[test]
    fn test_filter_resources_no_filter() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service"
            }),
        ];

        let filtered = filter_resources(resources.clone(), None).unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_resources_by_kind() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "Service"
            }),
        ];

        let filtered = filter_resources(resources, Some("ConfigMap")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_filter_resources_by_api_kind() {
        let resources = vec![
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "ConfigMap"
            }),
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment"
            }),
        ];

        let filtered = filter_resources(resources, Some("apps/v1/Deployment")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "Deployment");
    }
}
