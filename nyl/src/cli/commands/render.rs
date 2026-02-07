use clap::Args;
use std::path::Path;
use walkdir::WalkDir;

use crate::{
    config::ProjectConfig,
    constants::{API_VERSION, API_VERSION_ARGOCD, API_VERSION_COMPONENTS},
    generator::Generator,
    helm::{HelmChartResolver, HelmTemplateExecutor},
    kubernetes::{KubeClient, KubeRsClient},
    profiles::{deep_merge_value, Profile, ProfileConfig},
    resources::{
        component_kind_to_chart_ref, extract_application_generators, extract_nyl_release, is_nyl_component,
        is_remote_helm_chart_shortcut, parse_component_kind, ChartRef, HelmChart, NylComponent, NylRelease,
        ReleaseMetadata,
    },
    secrets::SecretsConfig,
    template::{TemplateContext, TemplateEngine},
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

    /// Maximum evaluation depth for recursive resource expansion (default: 10)
    #[arg(long, default_value = "10")]
    pub max_depth: usize,

    /// Track parent resource information in annotations
    #[arg(long)]
    pub track_parent: bool,
}

/// Output format for rendered manifests
#[derive(Debug, Clone, Copy)]
enum OutputFormat {
    Yaml,
    #[allow(dead_code)]
    Json,
}

/// Shared manifest rendering logic used by render, diff, and apply
#[allow(clippy::too_many_arguments)]
pub async fn render_manifests(
    path: &str,
    component: Option<&str>,
    environment: Option<&str>,
    offline: bool,
    cli_kube_version: Option<&str>,
    cli_api_versions: &[String],
    max_depth: usize,
    track_parent: bool,
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

    // 6. Load and filter resources (rendering Jinja templates in manifest files)
    let resources = load_resources(path, &context)?;
    let filtered = filter_resources(resources, component)?;

    // 7. Check if any resources need Helm rendering (HelmChart or Component)
    let needs_helm_rendering = filtered.iter().any(|r| {
        let kind = r.get("kind").and_then(|k| k.as_str());
        let api_version = r.get("apiVersion").and_then(|a| a.as_str());
        (kind == Some("HelmChart") && api_version == Some(API_VERSION)) || api_version == Some(API_VERSION_COMPONENTS)
    });

    // 8. Determine kube_version and api_versions (only if needed)
    let (kube_version, api_versions) = if !needs_helm_rendering {
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

    // 9. Generate manifests, recursively expanding nested HelmChart/Component resources
    let mut all_manifests = Vec::new();
    let mut pending = filtered;
    for _ in 0..max_depth {
        let mut next_pending = Vec::new();
        for resource in pending {
            let manifests = generate_resource(
                &generator,
                &resource,
                &context,
                &project_config,
                &kube_version,
                &api_versions,
                track_parent,
            )?;
            for manifest in manifests {
                if is_renderable_resource(&manifest) {
                    next_pending.push(manifest);
                } else {
                    all_manifests.push(manifest);
                }
            }
        }
        pending = next_pending;
        if pending.is_empty() {
            break;
        }
    }

    // Include any remaining pending resources that weren't fully evaluated
    // This happens when max_depth is reached before all resources are expanded
    all_manifests.extend(pending);

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
        args.max_depth,
        args.track_parent,
    )
    .await?;

    // Filter out NylRelease (control resource, not applied to the cluster)
    let manifests: Vec<_> = manifests
        .into_iter()
        .filter(|m| !NylRelease::is_nyl_release(m))
        .collect();

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

/// Load YAML/JSON resources from a path, rendering Jinja templates.
///
/// If `path` is a file, only that file is loaded. If `path` is a directory,
/// only the immediate YAML/JSON files in that directory are loaded (non-recursive).
fn load_resources(path: &str, context: &TemplateContext) -> Result<Vec<serde_json::Value>> {
    let path = Path::new(path);
    let engine = TemplateEngine::new();
    let ctx_json = context.to_json();
    let mut resources = Vec::new();

    let files: Vec<std::path::PathBuf> = if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(path)
            .map_err(|e| NylError::Config(format!("Failed to read directory '{}': {}", path.display(), e)))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        entries.sort();
        entries
    };

    for file_path in &files {
        let ext = file_path.extension().and_then(|s| s.to_str());
        if !matches!(ext, Some("yaml" | "yml" | "json")) {
            continue;
        }

        // Skip nyl project configuration files — they are not manifests
        let stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(stem, "nyl-project" | "nyl-profiles" | "nyl-secrets") {
            continue;
        }

        tracing::debug!("Reading manifest file: {}", file_path.display());

        let raw =
            std::fs::read_to_string(file_path).map_err(|e| NylError::Config(format!("Failed to read file: {}", e)))?;

        let rendered = engine.render_named(&file_path.display().to_string(), &raw, &ctx_json)?;

        let docs = parse_yaml_documents(&rendered)?;
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

/// Check whether a resource will be expanded by generate_resource
/// (i.e. it is a HelmChart or Component, not a plain k8s manifest)
fn is_renderable_resource(resource: &serde_json::Value) -> bool {
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());
    (kind == Some("HelmChart") && api_version == Some(API_VERSION)) || is_nyl_component(resource)
}

/// Maximum Levenshtein distance for considering an API version as "similar" to a known Nyl domain.
/// Distance of 3 allows for common typos like:
/// - Single character substitution (e.g., "nikolas" instead of "niklas")
/// - Missing character (e.g., ".co" instead of ".com")
/// - Extra character (e.g., "githubb" instead of "github")
const MAX_TYPO_DISTANCE: usize = 3;

/// Check if an API version looks like it might be a Nyl resource API version
fn is_nyl_like_api_version(api_version: &str) -> bool {
    // Check if it contains the Nyl domain
    if api_version.contains("nyl.niklasrosenstein.github.com") {
        return true;
    }

    // Extract the domain part (before any version suffix like /v1)
    let domain = api_version.split('/').next().unwrap_or(api_version);

    // Check for similar patterns using Levenshtein distance
    // Extract base domains from the API version constants
    let nyl_api_versions = [API_VERSION, API_VERSION_COMPONENTS, API_VERSION_ARGOCD];

    for api_ver in &nyl_api_versions {
        let known_domain = api_ver.split('/').next().unwrap_or(api_ver);
        if levenshtein_distance(domain, known_domain) <= MAX_TYPO_DISTANCE {
            return true;
        }
    }

    false
}

/// Calculate Levenshtein distance between two strings
fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    let len1 = s1.len();
    let len2 = s2.len();

    if len1 == 0 {
        return len2;
    }
    if len2 == 0 {
        return len1;
    }

    // Convert to character vectors once for efficient indexing
    let chars1: Vec<char> = s1.chars().collect();
    let chars2: Vec<char> = s2.chars().collect();
    let len1 = chars1.len();
    let len2 = chars2.len();

    let mut matrix = vec![vec![0; len2 + 1]; len1 + 1];

    for (i, row) in matrix.iter_mut().enumerate().take(len1 + 1) {
        row[0] = i;
    }
    for (j, cell) in matrix[0].iter_mut().enumerate().take(len2 + 1) {
        *cell = j;
    }

    for i in 0..len1 {
        for j in 0..len2 {
            let cost = usize::from(chars1[i] != chars2[j]);
            matrix[i + 1][j + 1] = (matrix[i][j + 1] + 1)
                .min(matrix[i + 1][j] + 1)
                .min(matrix[i][j] + cost);
        }
    }

    matrix[len1][len2]
}

/// Check if a resource is a known Nyl resource type
fn is_known_nyl_resource(resource: &serde_json::Value) -> bool {
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());

    // Check for HelmChart
    if kind == Some("HelmChart") && api_version == Some(API_VERSION) {
        return true;
    }

    // Check for Component
    if is_nyl_component(resource) {
        return true;
    }

    // Check for NylRelease
    if NylRelease::is_nyl_release(resource) {
        return true;
    }

    // Check for ApplicationGenerator
    if let Some(api_ver) = api_version {
        if api_ver == API_VERSION_ARGOCD && kind == Some("ApplicationGenerator") {
            return true;
        }
    }

    false
}

/// Generate manifests from a resource
#[allow(clippy::too_many_lines)]
fn generate_resource(
    _generator: &Generator,
    resource: &serde_json::Value,
    context: &TemplateContext,
    config: &ProjectConfig,
    kube_version: &str,
    api_versions: &[String],
    track_parent: bool,
) -> Result<Vec<serde_json::Value>> {
    // Check if it's a HelmChart resource
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());

    if kind == Some("HelmChart") && api_version == Some(API_VERSION) {
        // Parse as HelmChart and render
        let chart: HelmChart = serde_json::from_value(resource.clone())
            .map_err(|e| NylError::Config(format!("Failed to parse HelmChart: {}", e)))?;
        let manifests = render_helm_chart(&chart, context, config, kube_version, api_versions)?;

        // Add parent tracking annotations if enabled
        if track_parent {
            Ok(manifests
                .into_iter()
                .map(|mut m| {
                    add_parent_annotations(
                        &mut m,
                        &chart.api_version,
                        &chart.kind,
                        &chart.metadata.name,
                        chart.metadata.namespace.as_deref(),
                    );
                    m
                })
                .collect())
        } else {
            Ok(manifests)
        }
    } else if is_nyl_component(resource) {
        // Parse as Component and render via the existing Helm path
        let component: NylComponent = serde_json::from_value(resource.clone())
            .map_err(|e| NylError::Config(format!("Failed to parse Component: {}", e)))?;

        // Check if the kind uses the shortcut format for remote Helm charts
        if is_remote_helm_chart_shortcut(&component.kind) {
            // Shortcut format: <repository>#<name>@<version>
            let parsed = parse_component_kind(&component.kind);

            // Validate HTTP(S) shortcuts: require an explicit chart name segment (`#<chart-name>`).
            if (parsed.base.starts_with("http://") || parsed.base.starts_with("https://")) && parsed.name.is_none() {
                return Err(NylError::Config(format!(
                    "Invalid remote Helm chart shortcut '{}': missing chart name. \
                     Use '<repository>#<chart-name>' or '<repository>#<chart-name>@<version>'.",
                    component.kind
                )));
            }

            let chart_ref = component_kind_to_chart_ref(&parsed);

            let release_name = component.metadata.name.clone();
            let release_namespace = component.metadata.namespace.clone();
            let component_api_version = component.api_version.clone();
            let component_kind = component.kind.clone();
            let component_name = component.metadata.name.clone();

            let chart = HelmChart {
                api_version: API_VERSION.to_string(),
                kind: "HelmChart".to_string(),
                metadata: component.metadata,
                spec: crate::resources::HelmChartSpec {
                    chart: chart_ref,
                    release: Some(ReleaseMetadata {
                        name: release_name,
                        namespace: release_namespace.clone(),
                        create_namespace: false,
                    }),
                    values: component.spec,
                },
            };

            let manifests = render_helm_chart(&chart, context, config, kube_version, api_versions)?;

            // Add parent tracking annotations if enabled
            if track_parent {
                Ok(manifests
                    .into_iter()
                    .map(|mut m| {
                        add_parent_annotations(
                            &mut m,
                            &component_api_version,
                            &component_kind,
                            &component_name,
                            release_namespace.as_deref(),
                        );
                        m
                    })
                    .collect())
            } else {
                Ok(manifests)
            }
        } else {
            // Local component path - use existing component resolution mechanism
            let chart_dir = config.get_components_path().join(&component.kind);
            let chart_yaml = chart_dir.join("Chart.yaml");
            if !chart_yaml.exists() {
                return Err(NylError::Config(format!(
                    "Component '{}' references chart path '{}', but {} does not exist",
                    component.kind,
                    chart_dir.display(),
                    chart_yaml.display()
                )));
            }

            let release_name = component.metadata.name.clone();
            let release_namespace = component.metadata.namespace.clone();
            let component_api_version = component.api_version.clone();
            let component_kind = component.kind.clone();
            let component_name = component.metadata.name.clone();

            let chart = HelmChart {
                api_version: API_VERSION.to_string(),
                kind: "HelmChart".to_string(),
                metadata: component.metadata,
                spec: crate::resources::HelmChartSpec {
                    chart: ChartRef {
                        name: Some(chart_dir.to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                    release: Some(ReleaseMetadata {
                        name: release_name,
                        namespace: release_namespace.clone(),
                        create_namespace: false,
                    }),
                    values: component.spec,
                },
            };

            let manifests = render_helm_chart(&chart, context, config, kube_version, api_versions)?;

            // Add parent tracking annotations if enabled
            if track_parent {
                Ok(manifests
                    .into_iter()
                    .map(|mut m| {
                        add_parent_annotations(
                            &mut m,
                            &component_api_version,
                            &component_kind,
                            &component_name,
                            release_namespace.as_deref(),
                        );
                        m
                    })
                    .collect())
            } else {
                Ok(manifests)
            }
        }
    } else {
        // Check if this looks like an unknown Nyl resource
        if let Some(api_ver) = api_version {
            if is_nyl_like_api_version(api_ver) && !is_known_nyl_resource(resource) {
                let kind_str = kind.unwrap_or("<unknown>");
                // Dynamically build the list of known API versions from constants
                let known_api_versions = [API_VERSION, API_VERSION_COMPONENTS, API_VERSION_ARGOCD];
                let api_versions_str = known_api_versions
                    .iter()
                    .map(|s| format!("'{}'", s))
                    .collect::<Vec<_>>()
                    .join(", ");

                tracing::warn!(
                    "Resource with apiVersion '{}' and kind '{}' looks like a Nyl resource but is not recognized. \
                     It will be treated as a regular Kubernetes manifest. \
                     Known Nyl apiVersions: {}. \
                     Known kinds: HelmChart, NylRelease, ApplicationGenerator, and any Component kind.",
                    api_ver,
                    kind_str,
                    api_versions_str
                );
            }
        }

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
        tracing::debug!("Reading YAML file: {}", file_path.display());

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
    // Calculate subdirectory relative to the scanned base path
    let rel_dir = file_path
        .strip_prefix(base_path)
        .unwrap_or(file_path)
        .parent()
        .unwrap_or(Path::new(""));

    // Normalize the relative directory to POSIX-style separators for ArgoCD.
    let mut rel_dir_normalized = String::new();
    for component in rel_dir.components() {
        if let std::path::Component::Normal(os_str) = component {
            if !rel_dir_normalized.is_empty() {
                rel_dir_normalized.push('/');
            }
            rel_dir_normalized.push_str(&os_str.to_string_lossy());
        }
    }

    // Application path must be relative to the repo root, not the worktree.
    // Start from the generator's source.path and append any subdirectory.
    let path_str = if rel_dir_normalized.is_empty() {
        generator.spec.source.path.clone()
    } else {
        format!("{}/{}", generator.spec.source.path, rel_dir_normalized)
    };

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

/// Add parent resource tracking annotations to a manifest
fn add_parent_annotations(
    manifest: &mut serde_json::Value,
    parent_api_version: &str,
    parent_kind: &str,
    parent_name: &str,
    parent_namespace: Option<&str>,
) {
    use crate::constants::{
        ANNOTATION_PARENT_API_VERSION, ANNOTATION_PARENT_KIND, ANNOTATION_PARENT_NAME, ANNOTATION_PARENT_NAMESPACE,
    };

    if let Some(metadata) = manifest.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        let annotations = metadata
            .entry("annotations")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut();

        if let Some(annotations) = annotations {
            annotations.insert(
                ANNOTATION_PARENT_API_VERSION.to_string(),
                serde_json::Value::String(parent_api_version.to_string()),
            );
            annotations.insert(
                ANNOTATION_PARENT_KIND.to_string(),
                serde_json::Value::String(parent_kind.to_string()),
            );
            annotations.insert(
                ANNOTATION_PARENT_NAME.to_string(),
                serde_json::Value::String(parent_name.to_string()),
            );
            if let Some(ns) = parent_namespace {
                annotations.insert(
                    ANNOTATION_PARENT_NAMESPACE.to_string(),
                    serde_json::Value::String(ns.to_string()),
                );
            }
        }
    }
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

    #[test]
    fn test_path_normalization_posix() {
        use std::path::Path;

        // Simulate the path normalization logic in create_argocd_application_from_generator
        let rel_dir = Path::new("subdir/nested");
        let mut rel_dir_normalized = String::new();
        for component in rel_dir.components() {
            if let std::path::Component::Normal(os_str) = component {
                if !rel_dir_normalized.is_empty() {
                    rel_dir_normalized.push('/');
                }
                rel_dir_normalized.push_str(&os_str.to_string_lossy());
            }
        }

        assert_eq!(rel_dir_normalized, "subdir/nested");
    }

    #[test]
    fn test_path_normalization_with_join() {
        use std::path::Path;

        // Test with platform-native path construction
        let rel_dir = Path::new("subdir").join("nested");
        let mut rel_dir_normalized = String::new();
        for component in rel_dir.components() {
            if let std::path::Component::Normal(os_str) = component {
                if !rel_dir_normalized.is_empty() {
                    rel_dir_normalized.push('/');
                }
                rel_dir_normalized.push_str(&os_str.to_string_lossy());
            }
        }

        // Should always produce POSIX-style paths regardless of platform
        assert_eq!(rel_dir_normalized, "subdir/nested");
    }

    #[test]
    fn test_path_normalization_root() {
        use std::path::Path;

        // Test empty path handling
        let rel_dir = Path::new("");
        let mut rel_dir_normalized = String::new();
        for component in rel_dir.components() {
            if let std::path::Component::Normal(os_str) = component {
                if !rel_dir_normalized.is_empty() {
                    rel_dir_normalized.push('/');
                }
                rel_dir_normalized.push_str(&os_str.to_string_lossy());
            }
        }

        assert_eq!(rel_dir_normalized, "");
    }

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", "abd"), 1);
        assert_eq!(levenshtein_distance("abc", "abcd"), 1);
        assert_eq!(levenshtein_distance("abc", "def"), 3);
        assert_eq!(
            levenshtein_distance("nyl.niklasrosenstein.github.com", "nyl.niklasrosenstein.github.com"),
            0
        );
        assert_eq!(
            levenshtein_distance("nyl.niklasrosenstein.github.com", "nyl.nikolasrosenstein.github.com"),
            1

    #[test]
    fn test_add_parent_annotations() {
        use crate::constants::{
            ANNOTATION_PARENT_API_VERSION, ANNOTATION_PARENT_KIND, ANNOTATION_PARENT_NAME, ANNOTATION_PARENT_NAMESPACE,
        };

        // Test adding annotations to a manifest
        let mut manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "test-pod",
                "namespace": "default"
            },
            "spec": {
                "containers": []
            }
        });

        add_parent_annotations(
            &mut manifest,
            "nyl.niklasrosenstein.github.com/v1",
            "HelmChart",
            "my-chart",
            Some("default"),
        );

        // Verify annotations were added
        let annotations = manifest["metadata"]["annotations"].as_object().unwrap();
        assert_eq!(
            annotations
                .get(ANNOTATION_PARENT_API_VERSION)
                .unwrap()
                .as_str()
                .unwrap(),
            "nyl.niklasrosenstein.github.com/v1"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_KIND).unwrap().as_str().unwrap(),
            "HelmChart"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAME).unwrap().as_str().unwrap(),
            "my-chart"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAMESPACE).unwrap().as_str().unwrap(),
            "default"
        );
    }

    #[test]
    fn test_is_nyl_like_api_version_exact_match() {
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.com/v1"));
        assert!(is_nyl_like_api_version("components.nyl.niklasrosenstein.github.com/v1"));
        assert!(is_nyl_like_api_version("argocd.nyl.niklasrosenstein.github.com/v1"));
    }

    #[test]
    fn test_is_nyl_like_api_version_contains() {
        // Should match anything containing the domain
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.com/v2"));
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.com"));
        assert!(is_nyl_like_api_version("foo.nyl.niklasrosenstein.github.com/v1"));
    }

    #[test]
    fn test_is_nyl_like_api_version_similar() {
        // Typos within Levenshtein distance of 3
        assert!(is_nyl_like_api_version("nyl.nikolasrosenstein.github.com/v1")); // one character difference
        assert!(is_nyl_like_api_version("nyl.niklasrosenstein.github.co/v1")); // missing 'm'
    }

    #[test]
    fn test_is_nyl_like_api_version_not_similar() {
        // Standard Kubernetes API versions should not match
        assert!(!is_nyl_like_api_version("v1"));
        assert!(!is_nyl_like_api_version("apps/v1"));
        assert!(!is_nyl_like_api_version("batch/v1"));
        assert!(!is_nyl_like_api_version("argoproj.io/v1alpha1"));
    }

    #[test]
    fn test_is_known_nyl_resource_helm_chart() {
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "HelmChart",
            "metadata": {"name": "test"},
            "spec": {"chart": {"name": "nginx"}}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_component() {
        let resource = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/MyComponent",
            "metadata": {"name": "test"},
            "spec": {}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_nyl_release() {
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "NylRelease",
            "metadata": {"name": "test", "namespace": "default"}
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_application_generator() {
        let resource = serde_json::json!({
            "apiVersion": "argocd.nyl.niklasrosenstein.github.com/v1",
            "kind": "ApplicationGenerator",
            "metadata": {"name": "test"},
            "spec": {
                "destination": {"server": "https://k8s", "namespace": "argocd"},
                "source": {"repoURL": "https://github.com/test/repo", "path": "apps"}
            }
        });
        assert!(is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_unknown() {
        // Unknown Nyl-like resource
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "UnknownKind",
            "metadata": {"name": "test"}
        });
        assert!(!is_known_nyl_resource(&resource));
    }

    #[test]
    fn test_is_known_nyl_resource_standard_k8s() {
        // Standard Kubernetes resource
        let resource = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        assert!(!is_known_nyl_resource(&resource));

    #[test]
    fn test_add_parent_annotations_without_namespace() {
        use crate::constants::{
            ANNOTATION_PARENT_API_VERSION, ANNOTATION_PARENT_KIND, ANNOTATION_PARENT_NAME, ANNOTATION_PARENT_NAMESPACE,
        };

        let mut manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test-config"
            }
        });

        add_parent_annotations(
            &mut manifest,
            "nyl.niklasrosenstein.github.com/v1",
            "Component",
            "my-component",
            None,
        );

        // Verify annotations were added (except namespace)
        let annotations = manifest["metadata"]["annotations"].as_object().unwrap();
        assert_eq!(
            annotations
                .get(ANNOTATION_PARENT_API_VERSION)
                .unwrap()
                .as_str()
                .unwrap(),
            "nyl.niklasrosenstein.github.com/v1"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_KIND).unwrap().as_str().unwrap(),
            "Component"
        );
        assert_eq!(
            annotations.get(ANNOTATION_PARENT_NAME).unwrap().as_str().unwrap(),
            "my-component"
        );
        // Namespace annotation should not be present
        assert!(annotations.get(ANNOTATION_PARENT_NAMESPACE).is_none());
    }

    #[test]
    fn test_add_parent_annotations_preserves_existing() {
        use crate::constants::ANNOTATION_PARENT_API_VERSION;

        // Test that existing annotations are preserved
        let mut manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {
                "name": "test-service",
                "annotations": {
                    "existing-annotation": "existing-value"
                }
            }
        });

        add_parent_annotations(
            &mut manifest,
            "nyl.niklasrosenstein.github.com/v1",
            "HelmChart",
            "my-chart",
            None,
        );

        let annotations = manifest["metadata"]["annotations"].as_object().unwrap();
        // Original annotation should still be there
        assert_eq!(
            annotations.get("existing-annotation").unwrap().as_str().unwrap(),
            "existing-value"
        );
        // New annotation should also be there
        assert_eq!(
            annotations
                .get(ANNOTATION_PARENT_API_VERSION)
                .unwrap()
                .as_str()
                .unwrap(),
            "nyl.niklasrosenstein.github.com/v1"
        );
    }

    #[test]
    fn test_is_renderable_resource_helm_chart() {
        let resource = serde_json::json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "HelmChart",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource));
    }

    #[test]
    fn test_is_renderable_resource_component() {
        let resource = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "example/v1/Nginx",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource));
    }

    #[test]
    fn test_is_renderable_resource_component_shortcut() {
        let resource = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "https://charts.example.com/repo#nginx@1.0.0",
            "metadata": {"name": "test"}
        });
        assert!(is_renderable_resource(&resource));
    }

    #[test]
    fn test_is_renderable_resource_plain_k8s() {
        let resource = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "test"}
        });
        assert!(!is_renderable_resource(&resource));
    }
}
