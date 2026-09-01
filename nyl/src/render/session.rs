//! Session-scoped, target-aware manifest rendering.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    deduplicate_manifests, filter_render_resources, generate_render_resource, is_helm_renderable_resource,
    is_renderable_resource, load_release_bundle_with_root, prepare_manifests_for_output, process_application_generator,
    resolve_strip_empty_metadata_labels_mode, RenderResource,
};
use crate::config::ProjectConfig;
use crate::helm::HelmChartResolver;
use crate::kubernetes::ResourceKey;
use crate::postprocess::apply_kyverno_policies;
use crate::resources::{
    component_kind_to_chart_ref, extract_all_kyverno_policies, extract_application_generators, extract_release,
    is_nyl_component, is_remote_helm_chart_shortcut, parse_component_kind, ApplicationGenerator, Cluster, GitOpsTarget,
    HelmChart, KyvernoScope, Release,
};
use crate::secrets::SecretsConfig;
use crate::template::TemplateContext;
use crate::{NylError, Result};

use super::cache::{CacheLayer, CacheMode, CacheOutcome, RenderCache};

/// Immutable rendering state shared by every release in one GitOps target.
pub struct RenderSession {
    project_root: PathBuf,
    project_config: ProjectConfig,
    target_name: Option<String>,
    kube_version: String,
    api_versions: Vec<String>,
    template_context: TemplateContext,
    credential_provider: Option<Arc<crate::git::CredentialProvider>>,
    missing_capabilities_error: Option<String>,
    cache: Option<RenderCache>,
}

/// Options that select one bundle and control its recursive expansion.
#[derive(Clone, Debug)]
pub struct RenderRequest<'a> {
    pub path: &'a Path,
    pub path_mode: RenderPathMode,
    pub provenance_root: Option<&'a Path>,
    pub only_source_kind: Option<&'a str>,
    pub max_depth: usize,
    pub track_parent: bool,
    pub expand_application_generators: bool,
    pub strip_empty_metadata_labels_default: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum RenderPathMode {
    ProjectRootRelative,
    AsProvided,
}

impl<'a> RenderRequest<'a> {
    pub fn new(path: &'a Path, provenance_root: Option<&'a Path>) -> Self {
        Self {
            path,
            path_mode: RenderPathMode::ProjectRootRelative,
            provenance_root,
            only_source_kind: None,
            max_depth: 10,
            track_parent: false,
            expand_application_generators: false,
            strip_empty_metadata_labels_default: false,
        }
    }
}

/// The result of rendering one source bundle through the authoritative pipeline.
#[derive(Debug)]
pub struct RenderedBundle {
    /// The release declaration. `None` means target templating omitted it.
    pub release: Option<Release>,
    /// Fully expanded, policy-processed and deduplicated Kubernetes manifests.
    pub manifests: Vec<Value>,
    /// ApplicationGenerator controls found in the source bundle.
    pub application_generators: Vec<ApplicationGenerator>,
    /// Duplicate Kubernetes resources discarded by last-write-wins processing.
    pub duplicates: HashMap<ResourceKey, usize>,
    /// Whether empty `metadata.labels` objects were normalized out.
    pub strip_empty_metadata_labels: bool,
    /// Per-resource Nyl expansion provenance, keyed by final Kubernetes identity.
    pub manifest_provenance: HashMap<ResourceKey, String>,
    /// Provenance of the Release declaration, used for Nyl-synthesized resources.
    pub release_provenance: Option<String>,
    /// Entry and included files that contributed to this release.
    pub inputs: Vec<PathBuf>,
    /// Whether every external renderer input can be validated without repeating the render.
    pub cacheable: bool,
    /// Inputs that prevent the complete bundle from being revalidated.
    pub cache_bypass_reasons: BTreeSet<String>,
    /// Concrete files and directories observed while rendering this bundle.
    pub cache_dependencies: std::collections::BTreeMap<String, super::cache::RecordedDependency>,
    /// Number of Helm expansions represented by this rendered bundle.
    pub helm_render_count: usize,
}

impl RenderSession {
    /// Build an offline rendering session from a project root and effective target.
    pub fn for_target(
        project_root: &Path,
        project_config: &ProjectConfig,
        target: &GitOpsTarget,
        cluster: &Cluster,
    ) -> Result<Self> {
        Self::build(project_root, Some(project_config), target, cluster, true, false)
    }

    /// Build with the central project configuration but without secrets or
    /// process environment for independently controlled source manifests.
    pub fn for_untrusted_source(
        project_root: &Path,
        project_config: &ProjectConfig,
        target: &GitOpsTarget,
        cluster: &Cluster,
    ) -> Result<Self> {
        Self::build(project_root, Some(project_config), target, cluster, false, false)
    }

    /// Build a restricted remote-source session. Remote projects cannot load a
    /// secrets provider from their checkout.
    pub fn for_remote_target(project_root: &Path, target: &GitOpsTarget, cluster: &Cluster) -> Result<Self> {
        Self::build(project_root, None, target, cluster, false, true)
    }

    #[allow(clippy::too_many_lines)]
    fn build(
        project_root: &Path,
        project_config: Option<&ProjectConfig>,
        target: &GitOpsTarget,
        cluster: &Cluster,
        load_secrets: bool,
        restrict_checkout: bool,
    ) -> Result<Self> {
        target.validate()?;
        cluster.validate()?;
        let (kube_version, api_versions) = required_cluster_capabilities(cluster)?;
        let project_root = project_root
            .canonicalize()
            .map_err(|error| NylError::config(format!("Failed to resolve project root: {error}")))?;
        let project_config = match project_config {
            Some(project_config) => project_config.clone(),
            None => ProjectConfig::load_from_dir(None, Some(&project_root))?,
        };
        if restrict_checkout {
            let config_file = project_config.file.as_ref().ok_or_else(|| {
                NylError::config(format!(
                    "Remote renderer mode requires {}/nyl.toml",
                    project_root.display()
                ))
            })?;
            if config_file.parent().map(Path::to_path_buf) != Some(project_root.clone()) {
                return Err(NylError::config(format!(
                    "Remote renderer configuration must be rooted at {}",
                    project_root.display()
                )));
            }
            for entry in walkdir::WalkDir::new(&project_root).follow_links(false) {
                let entry = entry.map_err(|error| {
                    NylError::config(format!("Failed to inspect remote renderer checkout: {error}"))
                })?;
                if entry.file_type().is_symlink() {
                    return Err(NylError::config(format!(
                        "Remote renderer checkout must not contain symbolic links: {}",
                        entry.path().display()
                    )));
                }
            }
            for path in project_config
                .get_components_search_paths()
                .iter()
                .chain(project_config.get_helm_chart_search_paths())
            {
                let relative = path.strip_prefix(&project_root).map_err(|_| {
                    NylError::config(format!(
                        "Remote renderer path {} escapes {}",
                        path.display(),
                        project_root.display()
                    ))
                })?;
                if relative.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                }) {
                    return Err(NylError::config(format!(
                        "Remote renderer path {} escapes {}",
                        path.display(),
                        project_root.display()
                    )));
                }
                if path.exists() && !path.canonicalize()?.starts_with(&project_root) {
                    return Err(NylError::config(format!(
                        "Remote renderer path {} resolves outside {}",
                        path.display(),
                        project_root.display()
                    )));
                }
            }
        }
        let cluster_values = serde_json::to_value(&cluster.spec.values)?;
        let target_values = serde_json::to_value(&target.spec.values)?;
        let effective_values = crate::util::deep_merge_value(Some(cluster_values), target_values);

        let mut cluster_context = serde_json::to_value(cluster)?;
        cluster_context
            .get_mut("spec")
            .and_then(Value::as_object_mut)
            .expect("serialized Cluster spec is an object")
            .remove("live");
        let target_context = target_template_context(target, load_secrets)?;
        let template_context = if load_secrets {
            let secrets = SecretsConfig::load_from_dir(None, Some(&project_root))?;
            TemplateContext::build(effective_values.clone(), &secrets)?
        } else {
            TemplateContext {
                values: effective_values,
                secrets: serde_json::json!({}),
                env: serde_json::Map::new(),
                cluster: None,
                target: None,
            }
        }
        .with_gitops_context(cluster_context, target_context);

        Ok(Self {
            project_root,
            project_config,
            target_name: Some(target.metadata.name.clone()),
            kube_version,
            api_versions,
            template_context,
            credential_provider: None,
            missing_capabilities_error: None,
            cache: None,
        })
    }

    /// Build the session used by `render`, `apply`, and `diff`.
    pub async fn for_cli(
        project_root: &Path,
        project_config: &ProjectConfig,
        target: Option<(&GitOpsTarget, &Cluster)>,
        explicit_capabilities: Option<(String, Vec<String>)>,
        missing_capabilities_error: Option<String>,
    ) -> Result<Self> {
        let mut session = if let Some((target, cluster)) = target {
            Self::build(project_root, Some(project_config), target, cluster, true, false)?
        } else {
            let project_root = project_root
                .canonicalize()
                .map_err(|error| NylError::config(format!("Failed to resolve project root: {error}")))?;
            let secrets = SecretsConfig::load_from_dir(None, Some(&project_root))?;
            let (kube_version, api_versions) = explicit_capabilities.unwrap_or_default();
            Self {
                project_root,
                project_config: project_config.clone(),
                target_name: None,
                kube_version,
                api_versions,
                template_context: TemplateContext::build(serde_json::json!({}), &secrets)?,
                credential_provider: None,
                missing_capabilities_error,
                cache: None,
            }
        };
        session.credential_provider = crate::git::argocd_credential_provider_from_cluster().await;
        Ok(session)
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn target_name(&self) -> &str {
        self.target_name.as_deref().unwrap_or("targetless")
    }

    pub fn template_context(&self) -> &TemplateContext {
        &self.template_context
    }

    #[must_use]
    pub fn with_cache(mut self, cache: Option<RenderCache>) -> Self {
        self.cache = cache;
        self
    }

    pub fn set_cache(&mut self, cache: Option<RenderCache>) {
        self.cache = cache;
    }

    /// Render one source file without contacting a Kubernetes cluster.
    pub async fn render_release_file(&self, path: &Path) -> Result<RenderedBundle> {
        self.render(RenderRequest::new(path, Some(&self.project_root))).await
    }

    /// Render one source file while displaying provenance relative to the
    /// logical source root instead of an internal checkout location.
    pub async fn render_release_file_with_provenance_root(
        &self,
        path: &Path,
        provenance_root: &Path,
    ) -> Result<RenderedBundle> {
        self.render(RenderRequest::new(path, Some(provenance_root))).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn render(&self, request: RenderRequest<'_>) -> Result<RenderedBundle> {
        let path = if request.path.is_absolute() {
            request.path.to_path_buf()
        } else {
            self.project_root.join(request.path)
        };
        let source_path = match request.path_mode {
            RenderPathMode::ProjectRootRelative => &path,
            RenderPathMode::AsProvided => request.path,
        };
        let source_path_text = source_path
            .to_str()
            .ok_or_else(|| NylError::config(format!("Release path is not valid UTF-8: {}", path.display())))?;

        let bundle = load_release_bundle_with_root(
            Path::new(source_path_text),
            &self.template_context,
            request.provenance_root,
        )?;
        let resources = filter_render_resources(bundle.resources, request.only_source_kind);
        let mut cache_bypass_reasons = resources_render_cache_bypass_reasons(&resources, &self.project_config);
        let mut cacheable = cache_bypass_reasons.is_empty();
        let ReleaseCacheLookup {
            probe: mut cache_probe,
            cached,
        } = if cacheable {
            self.prepare_release_cache(
                &path,
                &bundle.inputs,
                &bundle.dependency_directories,
                &resources,
                &request,
            )?
        } else {
            ReleaseCacheLookup {
                probe: None,
                cached: None,
            }
        };
        if let Some(cached) = cached {
            tracing::debug!(path = %path.display(), "Reusing cached rendered Release");
            return Ok(cached.bundle.into_rendered(cached.dependencies));
        }
        let needs_helm_rendering = resources
            .iter()
            .any(|resource| is_renderable_resource(&resource.value, &self.project_config));
        let (kube_version, api_versions) = if needs_helm_rendering {
            if let Some(error) = self
                .missing_capabilities_error
                .as_ref()
                .filter(|_| self.kube_version.is_empty() || self.api_versions.is_empty())
            {
                return Err(NylError::config(error.clone()));
            }
            (self.kube_version.clone(), self.api_versions.clone())
        } else {
            (String::new(), Vec::new())
        };

        let mut manifests = Vec::new();
        let mut manifest_provenance = HashMap::new();
        let mut helm_render_count = 0;
        let mut pending = resources;
        for _ in 0..request.max_depth {
            let mut next = Vec::new();
            for resource in pending {
                helm_render_count += usize::from(is_helm_renderable_resource(&resource.value, &self.project_config));
                if let Some(probe) = &mut cache_probe {
                    record_resource_directory_dependency(&mut probe.recorder, &resource.value, &self.project_config)?;
                }
                for manifest in generate_render_resource(
                    &resource,
                    &self.template_context,
                    &self.project_config,
                    &kube_version,
                    &api_versions,
                    self.credential_provider.clone(),
                    request.track_parent,
                    self.cache.as_ref(),
                )
                .await?
                {
                    if let Some(reason) = resource_render_cache_bypass_reason(&manifest.value, &self.project_config) {
                        cacheable = false;
                        cache_bypass_reasons.insert(reason);
                    }
                    if is_renderable_resource(&manifest.value, &self.project_config) {
                        next.push(manifest);
                    } else {
                        push_rendered_manifest(&mut manifests, &mut manifest_provenance, manifest)?;
                    }
                }
            }
            pending = next;
            if pending.is_empty() {
                break;
            }
        }
        for resource in pending {
            push_rendered_manifest(&mut manifests, &mut manifest_provenance, resource)?;
        }

        let release_provenance = manifests
            .iter()
            .find(|manifest| Release::is_release(manifest))
            .and_then(|manifest| ResourceKey::from_json_value(manifest).ok())
            .and_then(|key| manifest_provenance.get(&key).cloned());

        let (release, manifests) = extract_release(&manifests)?;
        let strip_mode = resolve_strip_empty_metadata_labels_mode(
            self.project_config.get_strip_empty_metadata_labels_mode(),
            release.as_ref(),
        );

        let (application_generators, mut manifests) = extract_application_generators(&manifests)?;
        if request.expand_application_generators {
            for generator in &application_generators {
                manifests.extend(process_application_generator(
                    generator,
                    source_path_text,
                    self.credential_provider.clone(),
                    &self.template_context,
                )?);
            }
        }

        let (policies, manifests) = extract_all_kyverno_policies(&manifests)?;
        let global = policies.get(&KyvernoScope::Global).cloned().unwrap_or_default();
        let non_global_count: usize = policies
            .iter()
            .filter(|(scope, _)| **scope != KyvernoScope::Global)
            .map(|(_, policies)| policies.len())
            .sum();
        if non_global_count > 0 {
            tracing::warn!(
                "Found {} non-Global Kyverno policies. Only Global scope is currently supported. \
                 Immediate and Subtree scopes will be supported in a future version.",
                non_global_count
            );
        }
        let manifests = if global.is_empty() {
            manifests
        } else {
            apply_kyverno_policies(&manifests, &global)?
        };
        let (manifests, duplicates) = deduplicate_manifests(manifests)?;
        let strip_empty_metadata_labels = strip_mode.should_strip(request.strip_empty_metadata_labels_default);
        let manifests = prepare_manifests_for_output(&manifests, strip_empty_metadata_labels);

        for manifest in &manifests {
            let key = ResourceKey::from_json_value(manifest)?;
            manifest_provenance.entry(key).or_insert_with(|| {
                let mut provenance = release_provenance.clone().unwrap_or_default();
                if !provenance.is_empty() {
                    provenance.push('\n');
                }
                provenance.push_str("Generated or transformed during Nyl rendering");
                provenance
            });
        }

        let cache_dependencies = cache_probe
            .as_ref()
            .map(|probe| probe.recorder.filesystem_dependencies())
            .unwrap_or_default();
        if cache_probe.as_ref().is_some_and(|probe| !probe.recorder.is_cacheable()) {
            cacheable = false;
            cache_bypass_reasons.insert("unobservable filesystem dependency".to_string());
        }
        let rendered = RenderedBundle {
            release,
            manifests,
            application_generators,
            duplicates,
            strip_empty_metadata_labels,
            manifest_provenance,
            release_provenance,
            inputs: bundle.inputs,
            cacheable,
            cache_bypass_reasons,
            cache_dependencies,
            helm_render_count,
        };
        self.store_release_cache(cache_probe, &rendered)?;
        Ok(rendered)
    }

    fn prepare_release_cache(
        &self,
        path: &Path,
        inputs: &[PathBuf],
        dependency_directories: &[PathBuf],
        resources: &[RenderResource],
        request: &RenderRequest<'_>,
    ) -> Result<ReleaseCacheLookup> {
        let Some(cache) = &self.cache else {
            return Ok(ReleaseCacheLookup {
                probe: None,
                cached: None,
            });
        };
        if cache.mode() == CacheMode::Disabled {
            return Ok(ReleaseCacheLookup {
                probe: None,
                cached: None,
            });
        }
        let key = format!(
            "{}\0{}\0{}",
            self.project_root.display(),
            self.target_name(),
            path.display()
        );
        let previous = cache.load_record("release", &key)?;
        let mut recorder = cache.recorder();
        for input in inputs {
            recorder.record_path_file(input)?;
        }
        for directory in dependency_directories {
            recorder.record_directory(directory)?;
        }
        if let Some(config_file) = &self.project_config.file {
            recorder.record_path_file(config_file)?;
        }
        recorder.record_template_context(&self.template_context.to_json())?;
        recorder.record_value(
            "request",
            &serde_json::json!({
                "onlySourceKind": request.only_source_kind,
                "pathMode": format!("{:?}", request.path_mode),
                "maxDepth": request.max_depth,
                "trackParent": request.track_parent,
                "expandApplicationGenerators": request.expand_application_generators,
                "stripEmptyMetadataLabelsDefault": request.strip_empty_metadata_labels_default,
            }),
        )?;
        cache.record_renderer_tools(&mut recorder)?;
        for resource in resources {
            record_resource_directory_dependency(&mut recorder, &resource.value, &self.project_config)?;
        }
        if let Some(previous) = &previous {
            recorder.replay_filesystem_dependencies(previous)?;
        }
        let current = recorder.clone().finish("release", String::new());
        let cached = if cache.mode() == CacheMode::Refresh {
            cache.observe(CacheLayer::Release, CacheOutcome::Refreshed, &[]);
            None
        } else {
            match previous {
                None => {
                    cache.observe(CacheLayer::Release, CacheOutcome::Miss, &[]);
                    None
                }
                Some(previous) if previous.same_inputs(&current) => {
                    let cached: Option<CachedRenderedBundle> =
                        cache.load_artifact("release", &previous.artifact_digest)?;
                    if let Some(bundle) = &cached {
                        cache.observe_release_hit(bundle.helm_render_count);
                    } else {
                        cache.observe(CacheLayer::Release, CacheOutcome::Miss, &[]);
                    }
                    cached.map(|bundle| CachedReleaseHit {
                        bundle,
                        dependencies: recorder.filesystem_dependencies(),
                    })
                }
                Some(previous) => {
                    let changed_inputs = previous.changed_inputs(&current);
                    tracing::debug!(
                        path = %path.display(),
                        changed_inputs = ?changed_inputs,
                        "Invalidating cached rendered Release"
                    );
                    cache.observe(CacheLayer::Release, CacheOutcome::Invalidated, &[]);
                    None
                }
            }
        };
        Ok(ReleaseCacheLookup {
            probe: Some(ReleaseCacheProbe { key, recorder }),
            cached,
        })
    }

    fn store_release_cache(&self, probe: Option<ReleaseCacheProbe>, rendered: &RenderedBundle) -> Result<()> {
        let Some(cache) = &self.cache else {
            return Ok(());
        };
        if !rendered.cacheable {
            cache.observe(
                CacheLayer::Release,
                CacheOutcome::Bypassed,
                &rendered.cache_bypass_reasons.iter().cloned().collect::<Vec<_>>(),
            );
            return Ok(());
        }
        let Some(probe) = probe else {
            return Ok(());
        };
        if !probe.recorder.is_cacheable() {
            cache.observe(
                CacheLayer::Release,
                CacheOutcome::Bypassed,
                &["unobservable filesystem dependency".to_string()],
            );
            return Ok(());
        }
        let cached = CachedRenderedBundle::from_rendered(rendered);
        let Some(digest) = cache.store_artifact("release", &cached)? else {
            return Ok(());
        };
        let record = probe.recorder.finish("release", digest);
        cache.store_record("release", &probe.key, &record)?;
        cache.observe(CacheLayer::Release, CacheOutcome::Stored, &[]);
        Ok(())
    }
}

struct ReleaseCacheProbe {
    key: String,
    recorder: crate::render::cache::DependencyRecorder,
}

struct ReleaseCacheLookup {
    probe: Option<ReleaseCacheProbe>,
    cached: Option<CachedReleaseHit>,
}

struct CachedReleaseHit {
    bundle: CachedRenderedBundle,
    dependencies: std::collections::BTreeMap<String, super::cache::RecordedDependency>,
}

#[derive(Deserialize, Serialize)]
struct CachedRenderedBundle {
    release: Option<Release>,
    manifests: Vec<Value>,
    application_generators: Vec<ApplicationGenerator>,
    duplicates: Vec<(ResourceKey, usize)>,
    strip_empty_metadata_labels: bool,
    manifest_provenance: Vec<(ResourceKey, String)>,
    release_provenance: Option<String>,
    inputs: Vec<PathBuf>,
    helm_render_count: usize,
}

impl CachedRenderedBundle {
    fn from_rendered(rendered: &RenderedBundle) -> Self {
        let mut manifest_provenance = rendered
            .manifest_provenance
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        manifest_provenance.sort_by_cached_key(|(key, _)| serde_json::to_string(key).unwrap_or_default());
        let mut duplicates = rendered
            .duplicates
            .iter()
            .map(|(key, count)| (key.clone(), *count))
            .collect::<Vec<_>>();
        duplicates.sort_by_cached_key(|(key, _)| serde_json::to_string(key).unwrap_or_default());
        Self {
            release: rendered.release.clone(),
            manifests: rendered.manifests.clone(),
            application_generators: rendered.application_generators.clone(),
            duplicates,
            strip_empty_metadata_labels: rendered.strip_empty_metadata_labels,
            manifest_provenance,
            release_provenance: rendered.release_provenance.clone(),
            inputs: rendered.inputs.clone(),
            helm_render_count: rendered.helm_render_count,
        }
    }

    fn into_rendered(
        self,
        cache_dependencies: std::collections::BTreeMap<String, super::cache::RecordedDependency>,
    ) -> RenderedBundle {
        RenderedBundle {
            release: self.release,
            manifests: self.manifests,
            application_generators: self.application_generators,
            duplicates: self.duplicates.into_iter().collect(),
            strip_empty_metadata_labels: self.strip_empty_metadata_labels,
            manifest_provenance: self.manifest_provenance.into_iter().collect(),
            release_provenance: self.release_provenance,
            inputs: self.inputs,
            cacheable: true,
            cache_bypass_reasons: BTreeSet::new(),
            cache_dependencies,
            helm_render_count: self.helm_render_count,
        }
    }
}

fn record_resource_directory_dependency(
    recorder: &mut crate::render::cache::DependencyRecorder,
    resource: &Value,
    config: &ProjectConfig,
) -> Result<()> {
    if resource.get("apiVersion").and_then(Value::as_str) == Some(crate::constants::API_VERSION)
        && resource.get("kind").and_then(Value::as_str) == Some("HelmChart")
    {
        let chart: HelmChart = serde_json::from_value(resource.clone())?;
        if chart.spec.chart.repository.is_none() {
            let working_dir = config
                .file
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let resolver = HelmChartResolver::new(config.get_helm_chart_search_paths().to_vec(), working_dir);
            recorder.record_directory(&resolver.resolve_chart(&chart.spec.chart)?.path)?;
        }
        return Ok(());
    }
    if is_nyl_component(resource) {
        let api_version = resource.get("apiVersion").and_then(Value::as_str).unwrap_or_default();
        let kind = resource.get("kind").and_then(Value::as_str).unwrap_or_default();
        let effective = config.get_alias_target_for_kind(api_version, kind).unwrap_or(kind);
        if !is_remote_helm_chart_shortcut(effective) {
            recorder.record_directory(&config.resolve_component_chart_dir(effective)?)?;
        }
    }
    Ok(())
}

fn resource_render_cache_bypass_reason(resource: &Value, config: &ProjectConfig) -> Option<String> {
    if resource.get("apiVersion").and_then(Value::as_str) == Some(crate::constants::API_VERSION)
        && resource.get("kind").and_then(Value::as_str) == Some("HelmChart")
    {
        return match serde_json::from_value::<HelmChart>(resource.clone()) {
            Ok(chart) if chart.spec.chart.repository.is_some() && chart.spec.chart.version.is_none() => {
                Some("unpinned remote Helm chart".to_string())
            }
            Ok(_) => None,
            Err(_) => Some("invalid HelmChart dependency".to_string()),
        };
    }
    if !is_nyl_component(resource) {
        return None;
    }
    let Some(api_version) = resource.get("apiVersion").and_then(Value::as_str) else {
        return Some("component without apiVersion".to_string());
    };
    let Some(kind) = resource.get("kind").and_then(Value::as_str) else {
        return Some("component without kind".to_string());
    };
    let effective = config.get_alias_target_for_kind(api_version, kind).unwrap_or(kind);
    if !is_remote_helm_chart_shortcut(effective) {
        return None;
    }
    let chart_ref = component_kind_to_chart_ref(&parse_component_kind(effective));
    chart_ref
        .version
        .is_none()
        .then(|| "unpinned remote Helm chart".to_string())
}

fn resources_render_cache_bypass_reasons(resources: &[RenderResource], config: &ProjectConfig) -> BTreeSet<String> {
    resources
        .iter()
        .filter_map(|resource| resource_render_cache_bypass_reason(&resource.value, config))
        .collect()
}

fn push_rendered_manifest(
    manifests: &mut Vec<Value>,
    provenance: &mut HashMap<ResourceKey, String>,
    manifest: RenderResource,
) -> Result<()> {
    let key = ResourceKey::from_json_value(&manifest.value)?;
    provenance.insert(key, manifest.gitops_provenance_display()?);
    manifests.push(manifest.value);
    Ok(())
}

fn target_template_context(target: &GitOpsTarget, trusted_source: bool) -> Result<Value> {
    let mut context = serde_json::to_value(target)?;
    if !trusted_source {
        let publication = context
            .get_mut("spec")
            .and_then(Value::as_object_mut)
            .and_then(|spec| spec.get_mut("publication"))
            .and_then(Value::as_object_mut)
            .expect("serialized GitOpsTarget publication is an object");
        publication.remove("repository");
    }
    Ok(context)
}

fn required_cluster_capabilities(cluster: &Cluster) -> Result<(String, Vec<String>)> {
    let kube_version = cluster.spec.kubernetes.kube_version.clone().ok_or_else(|| {
        NylError::config(format!(
            "Cluster {:?} requires spec.kubernetes.kubeVersion for target rendering",
            cluster.metadata.name
        ))
    })?;
    if cluster.spec.kubernetes.api_versions.is_empty() {
        return Err(NylError::config(format!(
            "Cluster {:?} requires non-empty spec.kubernetes.apiVersions for target rendering",
            cluster.metadata.name
        )));
    }
    Ok((kube_version, cluster.spec.kubernetes.api_versions.clone()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::constants::{API_VERSION, API_VERSION_GITOPS};

    fn target() -> GitOpsTarget {
        serde_json::from_value(serde_json::json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": "GitOpsTarget",
            "metadata": {"name": "production", "labels": {"environment": "production"}},
            "spec": {
                "clusterRef": {"name": "kasoku"},
                "values": {"environment": "production", "nested": {"target": true}},
                "publication": {
                    "repository": {"repoURL": "https://example.invalid/deploy.git"},
                    "revision": "deploy/production"
                }
            }
        }))
        .unwrap()
    }

    fn cluster() -> Cluster {
        serde_json::from_value(serde_json::json!({
            "apiVersion": API_VERSION_GITOPS,
            "kind": "Cluster",
            "metadata": {"name": "kasoku", "labels": {"region": "fsn1"}},
            "spec": {
                "destination": {"server": "https://kubernetes.default.svc"},
                "kubernetes": {"kubeVersion": "1.31.4", "apiVersions": ["v1", "apps/v1"]},
                "values": {"nested": {"cluster": true}},
                "live": {"context": "kasoku-admin"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn only_unpinned_remote_charts_bypass_release_caching() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        let pinned = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "https://charts.example.com/#workload@1.2.3",
            "metadata": {"name": "workload"},
            "spec": {}
        });
        let unpinned = serde_json::json!({
            "apiVersion": "components.nyl.niklasrosenstein.github.com/v1",
            "kind": "https://charts.example.com/#workload",
            "metadata": {"name": "workload"},
            "spec": {}
        });
        let remote_manifest = serde_json::json!({
            "apiVersion": API_VERSION,
            "kind": "RemoteManifest",
            "metadata": {"name": "workload"},
            "spec": {"url": "https://example.com/releases/v1.2.3/workload.yaml"}
        });

        assert_eq!(resource_render_cache_bypass_reason(&pinned, &config), None);
        assert_eq!(
            resource_render_cache_bypass_reason(&unpinned, &config),
            Some("unpinned remote Helm chart".to_string())
        );
        assert_eq!(resource_render_cache_bypass_reason(&remote_manifest, &config), None);
    }

    fn write_strict_chart(root: &Path) {
        let chart = root.join("chart");
        fs::create_dir_all(chart.join("templates")).unwrap();
        fs::write(
            chart.join("Chart.yaml"),
            "apiVersion: v2\nname: strict-values\nversion: 1.0.0\n",
        )
        .unwrap();
        fs::write(chart.join("values.yaml"), "environment: \"\"\n").unwrap();
        fs::write(
            chart.join("values.schema.json"),
            r#"{
  "$schema": "https://json-schema.org/draft-07/schema#",
  "type": "object",
  "additionalProperties": false,
  "properties": {"environment": {"type": "string"}}
}
"#,
        )
        .unwrap();
        fs::write(
            chart.join("templates/configmap.yaml"),
            r"apiVersion: v1
kind: ConfigMap
metadata:
  name: strict-values
data:
  environment: {{ .Values.environment | quote }}
",
        )
        .unwrap();
    }

    #[test]
    fn central_session_reuses_the_parsed_project_configuration() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        fs::write(temp.path().join("nyl.toml"), "not valid TOML = [").unwrap();

        RenderSession::for_target(temp.path(), &config, &target(), &cluster()).unwrap();
    }

    #[tokio::test]
    async fn target_values_and_context_are_visible() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        fs::write(
            temp.path().join("app.yaml"),
            r#"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: {{ target.metadata.name }}
  namespace: production
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: context
data:
  cluster: "{{ values.nested.cluster }}"
  target: "{{ values.nested.target }}"
  environment: "{{ target.metadata.labels.environment }}"
  region: "{{ cluster.metadata.labels.region }}"
  clusterName: "{{ cluster.metadata.name }}"
"#,
        )
        .unwrap();

        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        let session = RenderSession::for_target(temp.path(), &config, &target(), &cluster()).unwrap();
        let rendered = session.render_release_file(Path::new("app.yaml")).await.unwrap();
        assert_eq!(rendered.release.unwrap().metadata.name, "production");
        assert_eq!(rendered.manifests[0]["data"]["cluster"], "true");
        assert_eq!(rendered.manifests[0]["data"]["target"], "true");
        assert_eq!(rendered.manifests[0]["data"]["environment"], "production");
        assert_eq!(rendered.manifests[0]["data"]["region"], "fsn1");
        assert_eq!(rendered.manifests[0]["data"]["clusterName"], "kasoku");
        let context = session.template_context().to_json();
        assert!(context.get("profile").is_none());
        assert!(context["cluster"]["spec"].get("live").is_none());
    }

    #[tokio::test]
    async fn target_values_reach_helm_only_through_explicit_templating() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        write_strict_chart(temp.path());
        fs::write(
            temp.path().join("app.yaml"),
            format!(
                r#"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: strict-values
  namespace: strict-values
---
apiVersion: {API_VERSION}
kind: HelmChart
metadata:
  name: strict-values
  namespace: strict-values
spec:
  chart:
    name: chart
  values:
    environment: "{{{{ values.environment }}}}"
"#
            ),
        )
        .unwrap();

        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        let session = RenderSession::for_target(temp.path(), &config, &target(), &cluster()).unwrap();
        let rendered = session.render_release_file(Path::new("app.yaml")).await.unwrap();

        assert_eq!(rendered.manifests[0]["data"]["environment"], "production");
    }

    #[tokio::test]
    async fn helm_errors_report_source_resource_and_chart_provenance() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        write_strict_chart(temp.path());
        fs::write(
            temp.path().join("bad.yaml"),
            format!(
                r"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: strict-values
  namespace: strict-values
---
apiVersion: {API_VERSION}
kind: HelmChart
metadata:
  name: strict-values
  namespace: strict-values
spec:
  chart:
    name: chart
  values:
    unexpected: true
"
            ),
        )
        .unwrap();

        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        let session = RenderSession::for_target(temp.path(), &config, &target(), &cluster()).unwrap();
        let error = session
            .render_release_file(Path::new("bad.yaml"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Source: bad.yaml (document 2)"), "{error}");
        assert!(
            error.contains("Resource: nyl.niklasrosenstein.github.com/v1 HelmChart strict-values/strict-values"),
            "{error}"
        );
        assert!(error.contains("chart: chart"), "{error}");
        assert!(
            error.contains("additional properties 'unexpected' not allowed"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn retains_application_generator_for_the_consumer() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        fs::write(
            temp.path().join("app.yaml"),
            r"apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
metadata:
  name: legacy
spec:
  destination:
    server: https://kubernetes.default.svc
    namespace: argocd
  source:
    repoURL: https://example.invalid/source.git
    path: applications
",
        )
        .unwrap();

        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        let session = RenderSession::for_target(temp.path(), &config, &target(), &cluster()).unwrap();
        let rendered = session.render_release_file(Path::new("app.yaml")).await.unwrap();
        assert_eq!(rendered.application_generators.len(), 1);
        assert!(rendered.manifests.is_empty());
    }

    #[test]
    fn remote_renderer_context_exposes_neither_secrets_nor_process_environment() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();

        let session = RenderSession::for_remote_target(temp.path(), &target(), &cluster()).unwrap();
        let context = session.template_context().to_json();
        assert_eq!(context["secrets"], serde_json::json!({}));
        assert_eq!(context["env"], serde_json::json!({}));
        assert!(context["target"]["spec"]["publication"].get("repository").is_none());

        let config = ProjectConfig::load_from_dir(None, Some(temp.path())).unwrap();
        let session = RenderSession::for_untrusted_source(temp.path(), &config, &target(), &cluster()).unwrap();
        let context = session.template_context().to_json();
        assert_eq!(context["secrets"], serde_json::json!({}));
        assert_eq!(context["env"], serde_json::json!({}));
        assert!(context["target"]["spec"]["publication"].get("repository").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn remote_renderer_rejects_symlinks_in_its_checkout() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("nyl.toml"), "").unwrap();
        symlink("/tmp", temp.path().join("components")).unwrap();

        let error = RenderSession::for_remote_target(temp.path(), &target(), &cluster())
            .err()
            .unwrap();
        assert!(error.to_string().contains("symbolic links"));
    }
}
