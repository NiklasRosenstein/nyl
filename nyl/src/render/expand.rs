//! Recursive Helm, Component, and RemoteManifest expansion.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::cache::RenderCache;
use super::RenderResource;
use crate::config::ProjectConfig;
use crate::constants::{API_VERSION, API_VERSION_ARGOCD, API_VERSION_COMPONENTS};
use crate::helm::{HelmChartResolver, HelmTemplateExecutor};
use crate::resources::{
    component_kind_to_chart_ref, is_nyl_component, is_remote_helm_chart_shortcut, parse_component_kind, ChartRef,
    HelmChart, NylComponent, Release, RemoteManifest,
};
use crate::template::TemplateContext;
use crate::{NylError, Result};

/// Filter resources by source kind (before expansion)
#[cfg(test)]
pub(crate) fn filter_resources(
    resources: Vec<serde_json::Value>,
    only_source_kind: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    if let Some(filter) = only_source_kind {
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

pub(crate) fn filter_render_resources(
    resources: Vec<RenderResource>,
    only_source_kind: Option<&str>,
) -> Vec<RenderResource> {
    let Some(filter) = only_source_kind else {
        return resources;
    };
    resources
        .into_iter()
        .filter(|resource| {
            let kind = resource.value.get("kind").and_then(|kind| kind.as_str()).unwrap_or("");
            let api_version = resource
                .value
                .get("apiVersion")
                .and_then(|api_version| api_version.as_str())
                .unwrap_or("");
            filter == kind || filter == format!("{api_version}/{kind}")
        })
        .collect()
}

/// Check whether a resource will be recursively expanded.
pub(crate) fn is_renderable_resource(resource: &serde_json::Value, config: &ProjectConfig) -> bool {
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());
    (kind == Some("HelmChart") && api_version == Some(API_VERSION))
        || (kind == Some("RemoteManifest") && api_version == Some(API_VERSION))
        || is_nyl_component(resource)
        || api_version
            .zip(kind)
            .and_then(|(av, k)| config.get_alias_target_for_kind(av, k))
            .is_some()
}

#[cfg(test)]
pub(crate) fn needs_helm_rendering(resources: &[serde_json::Value], config: &ProjectConfig) -> bool {
    resources.iter().any(|resource| {
        let kind = resource.get("kind").and_then(|k| k.as_str());
        let api_version = resource.get("apiVersion").and_then(|a| a.as_str());
        (kind == Some("HelmChart") && api_version == Some(API_VERSION))
            || api_version == Some(API_VERSION_COMPONENTS)
            || api_version
                .zip(kind)
                .and_then(|(av, k)| config.get_alias_target_for_kind(av, k))
                .is_some()
    })
}

/// Maximum Levenshtein distance for considering an API version as "similar" to a known Nyl domain.
/// Distance of 3 allows for common typos like:
/// - Single character substitution (e.g., "nikolas" instead of "niklas")
/// - Missing character (e.g., ".co" instead of ".com")
/// - Extra character (e.g., "githubb" instead of "github")
const MAX_TYPO_DISTANCE: usize = 3;
const MAX_REMOTE_MANIFEST_BYTES: usize = 30 * 1024 * 1024;

/// Check if an API version looks like it might be a Nyl resource API version
pub(crate) fn is_nyl_like_api_version(api_version: &str) -> bool {
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
pub(crate) fn levenshtein_distance(s1: &str, s2: &str) -> usize {
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

    for (i, c1) in chars1.iter().enumerate() {
        for (j, c2) in chars2.iter().enumerate() {
            let cost = usize::from(c1 != c2);
            matrix[i + 1][j + 1] = (matrix[i][j + 1] + 1)
                .min(matrix[i + 1][j] + 1)
                .min(matrix[i][j] + cost);
        }
    }

    matrix[len1][len2]
}

/// Check if a resource is a known Nyl resource type
pub(crate) fn is_known_nyl_resource(resource: &serde_json::Value) -> bool {
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

    // Check for Release
    if Release::is_release(resource) {
        return true;
    }

    // Check for RemoteManifest
    if RemoteManifest::is_remote_manifest(resource) {
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn generate_render_resource(
    resource: &RenderResource,
    context: &TemplateContext,
    config: &ProjectConfig,
    kube_version: &str,
    api_versions: &[String],
    credential_provider: Option<Arc<crate::git::CredentialProvider>>,
    track_parent: bool,
    gitops_cache: Option<&RenderCache>,
) -> Result<Vec<RenderResource>> {
    let provenance = resource.provenance.resource(&resource.value);
    let generated = generate_resource(
        &resource.value,
        context,
        config,
        kube_version,
        api_versions,
        credential_provider,
        track_parent,
        gitops_cache,
    )
    .await
    .map_err(|error| error.with_render_provenance(provenance.to_string()))?;
    Ok(generated
        .into_iter()
        .map(|value| RenderResource {
            value,
            provenance: provenance.clone(),
        })
        .collect())
}

pub(crate) fn render_resource_identity(resource: &serde_json::Value) -> String {
    let api_version = resource
        .get("apiVersion")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown-apiVersion>");
    let kind = resource
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown-kind>");
    let name = resource
        .pointer("/metadata/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<unknown-name>");
    let namespace = resource
        .pointer("/metadata/namespace")
        .and_then(serde_json::Value::as_str)
        .filter(|namespace| !namespace.is_empty());
    let identity = match namespace {
        Some(namespace) => format!("{api_version} {kind} {namespace}/{name}"),
        None => format!("{api_version} {kind} {name}"),
    };
    if api_version == API_VERSION && kind == "HelmChart" {
        let chart = render_chart_reference(resource.pointer("/spec/chart"));
        format!("{identity} (chart: {chart})")
    } else {
        identity
    }
}

fn render_chart_reference(chart: Option<&serde_json::Value>) -> String {
    let repository = chart
        .and_then(|chart| chart.get("repository"))
        .and_then(serde_json::Value::as_str);
    let name = chart
        .and_then(|chart| chart.get("name"))
        .and_then(serde_json::Value::as_str);
    let version = chart
        .and_then(|chart| chart.get("version"))
        .and_then(serde_json::Value::as_str);
    let mut reference = match (repository, name) {
        (Some(repository), Some(name)) => format!("{repository}#{name}"),
        (Some(repository), None) => repository.to_owned(),
        (None, Some(name)) => name.to_owned(),
        (None, None) => "<unknown-chart>".to_owned(),
    };
    if let Some(version) = version {
        reference.push('@');
        reference.push_str(version);
    }
    reference
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn generate_resource(
    resource: &serde_json::Value,
    context: &TemplateContext,
    config: &ProjectConfig,
    kube_version: &str,
    api_versions: &[String],
    credential_provider: Option<Arc<crate::git::CredentialProvider>>,
    track_parent: bool,
    gitops_cache: Option<&RenderCache>,
) -> Result<Vec<serde_json::Value>> {
    // Check if it's a HelmChart resource
    let kind = resource.get("kind").and_then(|k| k.as_str());
    let api_version = resource.get("apiVersion").and_then(|a| a.as_str());

    if kind == Some("HelmChart") && api_version == Some(API_VERSION) {
        // Parse as HelmChart and render
        let chart: HelmChart = serde_json::from_value(resource.clone())
            .map_err(|e| NylError::Config(format!("Failed to parse HelmChart: {}", e)))?;
        let manifests = render_helm_chart(
            &chart,
            context,
            config,
            kube_version,
            api_versions,
            credential_provider.clone(),
            gitops_cache,
        )?;

        Ok(apply_parent_tracking_annotations(
            manifests,
            track_parent,
            &chart.api_version,
            &chart.kind,
            &chart.metadata.name,
            chart.metadata.namespace.as_deref(),
        ))
    } else if kind == Some("RemoteManifest") && api_version == Some(API_VERSION) {
        let remote_manifest = RemoteManifest::from_value(resource)?;
        remote_manifest.validate()?;
        let manifests = fetch_remote_manifest_documents(&remote_manifest).await?;
        Ok(apply_parent_tracking_annotations(
            manifests,
            track_parent,
            &remote_manifest.api_version,
            &remote_manifest.kind,
            &remote_manifest.metadata.name,
            remote_manifest.metadata.namespace.as_deref(),
        ))
    } else if is_nyl_component(resource)
        || api_version
            .zip(kind)
            .and_then(|(av, k)| config.get_alias_target_for_kind(av, k))
            .is_some()
    {
        // Parse as Component and render via the existing Helm path
        let mut component: NylComponent = serde_json::from_value(resource.clone())
            .map_err(|e| NylError::Config(format!("Failed to parse Component: {}", e)))?;
        if let Some(target) = config.get_alias_target_for_kind(&component.api_version, &component.kind) {
            component.kind = target.to_string();
        }

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
                    values: component.spec,
                    include_crds: None,
                },
            };

            let manifests = render_helm_chart(
                &chart,
                context,
                config,
                kube_version,
                api_versions,
                credential_provider.clone(),
                gitops_cache,
            )?;

            Ok(apply_parent_tracking_annotations(
                manifests,
                track_parent,
                &component_api_version,
                &component_kind,
                &component_name,
                release_namespace.as_deref(),
            ))
        } else {
            // Local component path - use existing component resolution mechanism
            let chart_dir = config.resolve_component_chart_dir(&component.kind)?;

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
                    values: component.spec,
                    include_crds: None,
                },
            };

            let manifests = render_helm_chart(
                &chart,
                context,
                config,
                kube_version,
                api_versions,
                credential_provider.clone(),
                gitops_cache,
            )?;

            Ok(apply_parent_tracking_annotations(
                manifests,
                track_parent,
                &component_api_version,
                &component_kind,
                &component_name,
                release_namespace.as_deref(),
            ))
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
                     Known kinds: HelmChart, RemoteManifest, Release, ApplicationGenerator, and any Component kind.",
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

async fn fetch_remote_manifest_documents(remote_manifest: &RemoteManifest) -> Result<Vec<serde_json::Value>> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|e| {
            NylError::Process(format!(
                "Failed to initialize HTTPS client for RemoteManifest '{}': {}",
                remote_manifest.metadata.name, e
            ))
        })?;

    fetch_remote_manifest_documents_with_fetcher(remote_manifest, |url| {
        let client = &client;
        async move { fetch_single_remote_url(client, remote_manifest, &url).await }
    })
    .await
}

pub(crate) async fn fetch_remote_manifest_documents_with_fetcher<F, Fut>(
    remote_manifest: &RemoteManifest,
    mut fetcher: F,
) -> Result<Vec<serde_json::Value>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<serde_json::Value>>>,
{
    let urls = remote_manifest.resolve_urls()?;
    let mut all_documents = Vec::new();
    for url in urls {
        let docs = fetcher(url).await?;
        all_documents.extend(docs);
    }
    if remote_manifest.spec.override_namespace {
        override_fetched_manifest_namespaces(&mut all_documents, remote_manifest.metadata.namespace.as_deref());
    }
    Ok(all_documents)
}

async fn fetch_single_remote_url(
    client: &reqwest::Client,
    remote_manifest: &RemoteManifest,
    url: &str,
) -> Result<Vec<serde_json::Value>> {
    let sanitized_url = crate::util::sanitize_url(url);
    let mut response = client.get(url).send().await.map_err(|e| {
        let detail = if e.is_timeout() {
            "request timed out"
        } else if e.is_connect() {
            "connection failed"
        } else {
            "request failed"
        };
        NylError::Process(format!(
            "Failed to fetch RemoteManifest '{}' from {}: {}",
            remote_manifest.metadata.name, sanitized_url, detail
        ))
    })?;
    if !response.status().is_success() {
        return Err(NylError::Process(format!(
            "Failed to fetch RemoteManifest '{}' from {}: HTTP {}",
            remote_manifest.metadata.name,
            sanitized_url,
            response.status()
        )));
    }
    if let Some(content_length) = response.content_length() {
        if content_length > MAX_REMOTE_MANIFEST_BYTES as u64 {
            return Err(NylError::Process(format!(
                "RemoteManifest '{}' from {} exceeds size limit ({} bytes > {} bytes)",
                remote_manifest.metadata.name, sanitized_url, content_length, MAX_REMOTE_MANIFEST_BYTES
            )));
        }
    }

    let mut body_bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        NylError::Process(format!(
            "Failed to read RemoteManifest response body from {}: {}",
            sanitized_url, e
        ))
    })? {
        if body_bytes.len() + chunk.len() > MAX_REMOTE_MANIFEST_BYTES {
            return Err(NylError::Process(format!(
                "RemoteManifest '{}' from {} exceeds size limit (>{} bytes)",
                remote_manifest.metadata.name, sanitized_url, MAX_REMOTE_MANIFEST_BYTES
            )));
        }
        body_bytes.extend_from_slice(&chunk);
    }

    let body = String::from_utf8(body_bytes).map_err(|e| {
        NylError::Process(format!(
            "RemoteManifest '{}' from {} returned non-UTF-8 content: {}",
            remote_manifest.metadata.name, sanitized_url, e
        ))
    })?;
    let source_ctx = crate::util::SourceContext::new(PathBuf::from(format!("remote:{sanitized_url}")));
    source_ctx.parse_yaml_documents(&body)
}

pub(crate) fn override_fetched_manifest_namespaces(manifests: &mut [serde_json::Value], namespace: Option<&str>) {
    let Some(namespace) = namespace else {
        return;
    };

    for manifest in manifests {
        let Some(obj) = manifest.as_object_mut() else {
            continue;
        };
        let Some(metadata_obj) = obj.get_mut("metadata").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        if metadata_obj.contains_key("namespace") {
            metadata_obj.insert(
                "namespace".to_string(),
                serde_json::Value::String(namespace.to_string()),
            );
        }

        // Special case: RoleBinding/ClusterRoleBinding subjects can carry namespaced ServiceAccount references.
        // Rewrite subject namespace references alongside metadata.namespace overrides.
        let is_rbac_binding_kind = obj
            .get("kind")
            .and_then(|v| v.as_str())
            .is_some_and(|k| k == "RoleBinding" || k == "ClusterRoleBinding");
        let is_rbac_api_group = obj
            .get("apiVersion")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v.starts_with("rbac.authorization.k8s.io/"));
        if is_rbac_binding_kind && is_rbac_api_group {
            let Some(spec_subjects) = obj.get_mut("subjects").and_then(|v| v.as_array_mut()) else {
                continue;
            };
            for subject in spec_subjects {
                let Some(subject_obj) = subject.as_object_mut() else {
                    continue;
                };
                let is_service_account = subject_obj.get("kind").and_then(|v| v.as_str()) == Some("ServiceAccount");
                if subject_obj.contains_key("namespace") || is_service_account {
                    subject_obj.insert(
                        "namespace".to_string(),
                        serde_json::Value::String(namespace.to_string()),
                    );
                }
            }
        }
    }
}

pub(crate) fn apply_parent_tracking_annotations(
    manifests: Vec<serde_json::Value>,
    track_parent: bool,
    parent_api_version: &str,
    parent_kind: &str,
    parent_name: &str,
    parent_namespace: Option<&str>,
) -> Vec<serde_json::Value> {
    if !track_parent {
        return manifests;
    }

    manifests
        .into_iter()
        .map(|mut manifest| {
            add_parent_annotations(
                &mut manifest,
                parent_api_version,
                parent_kind,
                parent_name,
                parent_namespace,
            );
            manifest
        })
        .collect()
}

/// Render a Helm chart
fn render_helm_chart(
    chart: &HelmChart,
    context: &TemplateContext,
    config: &ProjectConfig,
    kube_version: &str,
    api_versions: &[String],
    credential_provider: Option<Arc<crate::git::CredentialProvider>>,
    gitops_cache: Option<&RenderCache>,
) -> Result<Vec<serde_json::Value>> {
    let working_dir = if let Some(config_file) = &config.file {
        config_file.parent().unwrap_or_else(|| Path::new(".")).to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| NylError::Config(format!("Failed to get current directory: {}", e)))?
    };

    let resolver = HelmChartResolver::with_cache_dir_and_provider(
        config.get_helm_chart_search_paths().to_vec(),
        working_dir,
        gitops_cache
            .and_then(RenderCache::external_cache_root)
            .map(Path::to_path_buf),
        credential_provider,
    );
    let resolved = resolver.resolve_chart(&chart.spec.chart)?;

    let executor = HelmTemplateExecutor::new()
        .with_kube_version(kube_version.to_string())
        .with_api_versions(api_versions.to_vec())
        .with_include_crds(chart.spec.include_crds.unwrap_or(true))
        .with_gitops_cache(gitops_cache.cloned());

    // Default namespace to "default" for deterministic rendering
    let namespace = chart.release_namespace().or(Some("default"));

    if context.target.is_some() {
        executor.template_with_source_comments(&resolved, chart.release_name(), namespace, &chart.spec.values)
    } else {
        executor.template(&resolved, chart.release_name(), namespace, &chart.spec.values)
    }
}

/// Add parent resource tracking annotations to a manifest
pub(crate) fn add_parent_annotations(
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
