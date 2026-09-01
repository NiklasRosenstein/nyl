//! Compilation of one effective GitOps target into a rendered file tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::git::GitManager;
use crate::render::cache::{CacheLayer, CacheMode, CacheOutcome};
use crate::resources::{
    is_supported_application_field_path, path_matches_glob, AppProjectDefinition, AppProjectManagement,
    ApplicationGroup, ApplicationGroupSource, ArgoCDInstance, ArgoCDInstanceSpec, CatalogApplicationDefaults, Cluster,
    ClusterDestination, GitOpsResource, GitOpsResourceKind, GitOpsTarget, GitPublication, InlineGitRepository,
    ManagedResourceDeletionPolicy, RendererConfig, RendererConfigMode, SharedNamespaceOwner,
};
use crate::template::TemplateEngine;
use crate::util::SourceContext;
use crate::{NylError, Result};

use super::{
    build_directory_application, ensure_managed_namespace, merge_sync_options, render_manifest_layout_with_provenance,
    take_managed_namespace, DirectoryApplicationInput, GitOpsCache, GitOpsInventory, RenderSession,
};

/// Pure output of compiling one target. Paths are relative to the target prefix.
#[derive(Debug, Deserialize, Serialize)]
pub struct CompiledTargetTree {
    pub target: GitOpsTarget,
    pub cluster: Cluster,
    pub repository_name: Option<String>,
    pub repository: InlineGitRepository,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    pub inputs: BTreeSet<PathBuf>,
}

/// Stable source identity for one Release as it enters tree compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseProgress {
    pub application_group: String,
    pub name: Option<String>,
    pub source_path: PathBuf,
}

/// Receives progress events from the sequential target-tree compiler.
pub trait TreeRenderObserver {
    fn started(&mut self, _total: usize) {}
    fn release_started(&mut self, _current: usize, _total: usize, _release: &ReleaseProgress) {}
    fn release_finished(&mut self, _completed: usize) {}
    fn finished(&mut self) {}
}

struct NoTreeRenderObserver;

impl TreeRenderObserver for NoTreeRenderObserver {}

#[derive(Debug, Deserialize, Serialize)]
struct CachedTargetTree {
    compiled: CompiledTargetTree,
    releases: usize,
    helm_renders: usize,
}

#[derive(Serialize)]
struct CachedTargetTreeRef<'a> {
    compiled: &'a CompiledTargetTree,
    releases: usize,
    helm_renders: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct ManagedNamespaceOwner {
    manifest: Value,
    provenance: String,
    application_namespace: String,
    project: String,
    destination: ClusterDestination,
    sync_policy: Option<crate::resources::GitOpsSyncPolicy>,
    deletion_policy: crate::resources::ApplicationDeletionPolicy,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
}

#[derive(Debug)]
struct PendingWorkload {
    group: ApplicationGroup,
    release: crate::resources::Release,
    manifests: Vec<Value>,
    manifest_provenance: HashMap<crate::kubernetes::ResourceKey, String>,
    release_provenance: Option<String>,
    destination_namespace: String,
    argocd_project_name: String,
    release_directory: PathBuf,
    application_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NamespaceScopeIssue {
    release: String,
    allowed_namespaces: BTreeSet<String>,
    resource: String,
    violation: NamespaceScopeViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NamespaceScopeViolation {
    UnexpectedNamespace(String),
    InvalidResourceNamespace,
    InvalidNamespaceName,
}

struct EffectiveProject {
    catalog_id: String,
    source_path: Option<PathBuf>,
    name: String,
    manifest: Option<Value>,
    destination_namespaces: Option<Vec<String>>,
}

struct PreparedGroup {
    group_resource_path: PathBuf,
    group: ApplicationGroup,
    project: EffectiveProject,
    source: ResolvedGroupSource,
}

struct EffectiveArgoCDInstance {
    identity: String,
    resource: ArgoCDInstance,
    source_path: Option<PathBuf>,
    cluster: Cluster,
    cluster_path: PathBuf,
}

/// Validate cross-resource references and target-prefix ownership without rendering releases.
pub fn validate_gitops_inventory(inventory: &GitOpsInventory) -> Result<()> {
    let mut publications = Vec::new();
    let instance_count = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == GitOpsResourceKind::ArgoCDInstance)
        .count();
    for discovered in inventory.resources.values() {
        match discovered.resource.as_ref() {
            Some(GitOpsResource::GitOpsTarget(target)) => {
                resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
                resolve_argocd_instance(inventory, target, instance_count)?;
                let (_, repository, _) = resolve_git_publication(inventory, &target.spec.publication)?;
                publications.push((
                    target.metadata.name.as_str(),
                    crate::git::normalize_git_url_for_equality(&repository.repo_url),
                    crate::git::normalize_git_url_for_equality(
                        repository.publish_url.as_deref().unwrap_or(&repository.repo_url),
                    ),
                    repository.repo_url,
                    normalize_branch_revision(&target.spec.publication.revision),
                    target.spec.publication.path_prefix.as_str(),
                ));
            }
            Some(GitOpsResource::ArgoCDInstance(instance)) => {
                resolve_cluster(inventory, &instance.spec.cluster_ref.name)?;
            }
            Some(GitOpsResource::ApplicationGroup(group)) => {
                if let Some(project_ref) = &group.spec.project_ref {
                    resolve_project(inventory, project_ref)?;
                }
                if let Some(source) = &group.spec.source {
                    if source.is_remote() {
                        resolve_source_repository(inventory, source)?;
                    }
                }
            }
            _ => {}
        }
    }
    for (index, left) in publications.iter().enumerate() {
        for right in &publications[index + 1..] {
            if (left.1 == right.1 || left.2 == right.2) && left.4 == right.4 && paths_overlap(left.5, right.5) {
                return Err(NylError::config(format!(
                    "GitOpsTarget {:?} and {:?} have overlapping publication path prefixes {:?} and {:?} on {}@{}",
                    left.0, right.0, left.5, right.5, left.3, left.4
                )));
            }
        }
    }
    validate_same_instance_catalog_collisions(inventory, instance_count)?;
    Ok(())
}

fn normalize_branch_revision(revision: &str) -> &str {
    revision.strip_prefix("refs/heads/").unwrap_or(revision)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left.is_empty()
        || right.is_empty()
        || left == right
        || left.strip_prefix(right).is_some_and(|rest| rest.starts_with('/'))
        || right.strip_prefix(left).is_some_and(|rest| rest.starts_with('/'))
}

/// Compile all applicable groups and releases for one target.
// The orchestration stays linear so ownership and policy checks remain visibly ordered.
#[allow(clippy::too_many_lines)]
pub async fn compile_target_tree(inventory: &GitOpsInventory, target_name: &str) -> Result<CompiledTargetTree> {
    compile_target_tree_inner(inventory, target_name, None, &mut NoTreeRenderObserver).await
}

/// Compile one target while reusing a verified content-addressed result when possible.
pub async fn compile_target_tree_cached(
    inventory: &GitOpsInventory,
    target_name: &str,
    cache: &GitOpsCache,
) -> Result<CompiledTargetTree> {
    compile_target_tree_inner(inventory, target_name, Some(cache), &mut NoTreeRenderObserver).await
}

/// Compile one target with cache reuse and observable Release progress.
pub async fn compile_target_tree_cached_with_observer(
    inventory: &GitOpsInventory,
    target_name: &str,
    cache: &GitOpsCache,
    observer: &mut dyn TreeRenderObserver,
) -> Result<CompiledTargetTree> {
    compile_target_tree_inner(inventory, target_name, Some(cache), observer).await
}

#[allow(clippy::too_many_lines)]
async fn compile_target_tree_inner(
    inventory: &GitOpsInventory,
    target_name: &str,
    cache: Option<&GitOpsCache>,
    observer: &mut dyn TreeRenderObserver,
) -> Result<CompiledTargetTree> {
    validate_gitops_inventory(inventory)?;
    let target_discovered = inventory
        .get(GitOpsResourceKind::GitOpsTarget, target_name)
        .ok_or_else(|| NylError::config(format!("GitOpsTarget {target_name:?} was not found")))?;
    let target = match &target_discovered
        .resource
        .as_ref()
        .ok_or_else(|| NylError::config(format!("GitOpsTarget {target_name:?} must be static")))?
    {
        GitOpsResource::GitOpsTarget(target) => target.clone(),
        _ => unreachable!("inventory kind key and resource variant must agree"),
    };
    let (cluster, cluster_path) = resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
    let instance_count = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == GitOpsResourceKind::ArgoCDInstance)
        .count();
    let argocd = resolve_argocd_instance(inventory, &target, instance_count)?;
    if argocd.source_path.is_none() {
        tracing::info!(
            target = %target.metadata.name,
            cluster = %target.spec.cluster_ref.name,
            "Using the implicit target-local ArgoCDInstance"
        );
    }
    let (repository_name, repository, repository_path) = resolve_git_publication(inventory, &target.spec.publication)?;
    let central_session =
        RenderSession::for_target(&inventory.project_root, &inventory.project_config, &target, &cluster)?
            .with_cache(cache.cloned());
    let mut git_manager = None;

    let mut groups = Vec::new();
    for discovered in inventory.resources.values() {
        if discovered.identity.kind != GitOpsResourceKind::ApplicationGroup {
            continue;
        }
        if !target_selects_group(&target, &discovered.static_labels) {
            continue;
        }
        let Some(GitOpsResource::ApplicationGroup(group)) = render_effective_control(discovered, &central_session)?
        else {
            continue;
        };
        if group.spec.enabled {
            groups.push((discovered.source_path.clone(), *group));
        }
    }
    groups.sort_by(|left, right| left.1.metadata.name.cmp(&right.1.metadata.name));

    let mut prepared_groups = Vec::new();
    for (group_resource_path, group) in groups {
        let project = resolve_effective_group_project(
            inventory,
            &group,
            &central_session,
            &repository,
            &cluster,
            &argocd.resource,
        )?;
        let mut source = resolve_group_source(
            inventory,
            &group_resource_path,
            &group,
            &target,
            &mut git_manager,
            cache,
        )?;
        if let Some(session) = &mut source.source_session {
            session.set_cache(cache.cloned());
        }
        prepared_groups.push(PreparedGroup {
            group_resource_path,
            group,
            project,
            source,
        });
    }

    let base_input_paths = [
        Some(target_discovered.source_path.as_path()),
        Some(cluster_path.as_path()),
        Some(argocd.cluster_path.as_path()),
        argocd.source_path.as_deref(),
        repository_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let mut cache_probe = prepare_target_cache(
        inventory,
        target_name,
        &central_session,
        cache,
        &base_input_paths,
        &prepared_groups,
    )?;
    let progress_total = prepared_groups.iter().map(|prepared| prepared.source.files.len()).sum();
    observer.started(progress_total);
    if let Some(compiled) = load_cached_target(cache, cache_probe.as_ref())? {
        observer.finished();
        return Ok(compiled);
    }
    let mut target_cacheable = cache_probe.as_ref().is_some_and(|probe| probe.cacheable);
    let mut target_cache_bypass_reasons = BTreeSet::new();
    let mut files = BTreeMap::new();
    let mut inputs = BTreeSet::from([
        target_discovered.source_path.clone(),
        cluster_path,
        argocd.cluster_path.clone(),
    ]);
    if let Some(path) = &argocd.source_path {
        inputs.insert(path.clone());
    }
    if let Some(repository_path) = repository_path {
        inputs.insert(repository_path);
    }
    let mut emitted_projects = BTreeSet::new();
    let mut namespace_owners = BTreeMap::<(String, String), ManagedNamespaceOwner>::new();
    let mut workload_owners = HashMap::new();
    let mut pending_workloads = Vec::new();
    let mut namespace_scope_errors = Vec::new();
    let mut release_count = 0;
    let mut progress_completed = 0;
    let mut helm_render_count = 0;

    for PreparedGroup {
        group_resource_path,
        group,
        project,
        source,
    } in prepared_groups
    {
        inputs.insert(group_resource_path.clone());
        if let Some(project_path) = project.source_path.clone() {
            inputs.insert(project_path);
        }
        let argocd_project_name = project.name;
        if let Some(manifest) = project.manifest {
            if emitted_projects.insert(project.catalog_id.clone()) {
                insert_yaml(
                    &mut files,
                    PathBuf::from("_nyl/catalog/projects").join(format!("{}.yaml", project.catalog_id)),
                    &manifest,
                )?;
            }
        }
        inputs.extend(source.provenance_inputs.iter().cloned());
        let session = match source.renderer_mode {
            RendererConfigMode::Central if !source.remote => &central_session,
            _ => source
                .source_session
                .as_ref()
                .expect("remote source must carry a restricted rendering session"),
        };
        let mut claimed_source_files = BTreeSet::new();

        for source_file in &source.files {
            claimed_source_files.insert(manifest_path_identity(&source_file.path));
            let input_path = if source.remote {
                PathBuf::from("@remote").join(source_file.path.strip_prefix(&source.root).unwrap_or(&source_file.path))
            } else {
                source_file
                    .path
                    .strip_prefix(&inventory.project_root)
                    .unwrap_or(&source_file.path)
                    .to_path_buf()
            };
            let current = progress_completed + 1;
            observer.release_started(
                current,
                progress_total,
                &ReleaseProgress {
                    application_group: group.metadata.name.clone(),
                    name: source_file.name.clone(),
                    source_path: input_path.clone(),
                },
            );
            inputs.insert(input_path);
            let provenance_root = if source.remote {
                &source.root
            } else {
                &inventory.project_root
            };
            let mut rendered = session
                .render_release_file_with_provenance_root(&source_file.path, provenance_root)
                .await?;
            progress_completed += 1;
            observer.release_finished(progress_completed);
            helm_render_count += rendered.helm_render_count;
            if let Some(probe) = &mut cache_probe {
                probe
                    .recorder
                    .extend_filesystem_dependencies(&rendered.cache_dependencies);
            }
            if !rendered.application_generators.is_empty() {
                return Err(NylError::config(format!(
                    "ApplicationGenerator is not supported in rendered GitOps source {}",
                    source_file.path.display()
                )));
            }
            target_cacheable &= rendered.cacheable;
            target_cache_bypass_reasons.extend(rendered.cache_bypass_reasons.iter().cloned());
            for bundle_input in &rendered.inputs {
                claimed_source_files.insert(manifest_path_identity(bundle_input));
                let input_path = if source.remote {
                    PathBuf::from("@remote").join(bundle_input.strip_prefix(&source.root).unwrap_or(bundle_input))
                } else {
                    bundle_input
                        .strip_prefix(&inventory.project_root)
                        .unwrap_or(bundle_input)
                        .to_path_buf()
                };
                inputs.insert(input_path);
            }
            let Some(release) = rendered.release.take() else {
                continue;
            };
            release_count += 1;
            let destination_namespace = group
                .spec
                .destination_namespace
                .clone()
                .unwrap_or_else(|| release.metadata.namespace.clone());
            namespace_scope_errors.extend(validate_release_namespace_scope(
                &rendered.manifests,
                &release,
                &destination_namespace,
            ));
            if let Some(patterns) = &project.destination_namespaces {
                validate_project_namespace_scope(&group, &release, &destination_namespace, patterns)?;
            }

            let group_output = group.spec.output_path.as_deref().unwrap_or(&group.metadata.name);
            crate::resources::validate_relative_path("ApplicationGroup outputPath", group_output, false, false)?;
            validate_path_segment("Release metadata.name", &release.metadata.name)?;
            if release.metadata.name == "_namespaces" {
                return Err(NylError::config(
                    "Release metadata.name \"_namespaces\" is reserved by the rendered GitOps layout",
                ));
            }
            let release_directory = PathBuf::from(group_output).join(&release.metadata.name);
            let application_name = render_application_name(session, &group, &release)?;
            validate_path_segment("rendered Application name", &application_name)?;
            pending_workloads.push(PendingWorkload {
                group: group.clone(),
                release,
                manifests: rendered.manifests,
                manifest_provenance: rendered.manifest_provenance,
                release_provenance: rendered.release_provenance,
                destination_namespace,
                argocd_project_name: argocd_project_name.clone(),
                release_directory,
                application_name,
            });
        }
        warn_unclaimed_group_manifests(&group, &source, &claimed_source_files);
    }

    if !namespace_scope_errors.is_empty() {
        return Err(NylError::validation(format_namespace_scope_issues(
            &namespace_scope_errors,
        )));
    }

    resolve_namespace_ownership(&mut pending_workloads, &mut namespace_owners, &cluster)?;

    for workload in pending_workloads {
        for manifest in &workload.manifests {
            let key = crate::kubernetes::ResourceKey::from_json_value(manifest)?;
            if let Some(previous) = workload_owners.insert(
                (cluster.metadata.name.clone(), key.clone()),
                application_name_hint(&workload.group, &workload.release),
            ) {
                return Err(NylError::config(format!(
                    "Rendered resource {key} is owned by more than one workload Application ({previous:?} and {:?})",
                    workload.release.metadata.name
                )));
            }
        }
        for (relative, bytes) in
            render_manifest_layout_with_provenance(&workload.manifests, &workload.manifest_provenance)?
        {
            insert_file(&mut files, workload.release_directory.join(relative), bytes)?;
        }

        let rendered_path = join_posix(
            target.spec.publication.path_prefix.as_str(),
            &workload.release_directory,
        )?;
        let mut application = build_directory_application(&DirectoryApplicationInput {
            name: workload.application_name.clone(),
            application_namespace: workload.group.spec.application_namespace.clone(),
            project: workload.argocd_project_name,
            repo_url: repository.repo_url.clone(),
            revision: target.spec.publication.revision.clone(),
            rendered_path,
            destination: cluster.spec.destination.clone(),
            destination_namespace: workload.destination_namespace,
            sync_policy: workload.group.spec.sync_policy.clone(),
            deletion_policy: workload.group.spec.application_deletion_policy,
            labels: workload.group.spec.labels.clone(),
            annotations: workload.group.spec.annotations.clone(),
        })?;
        apply_release_application_override(&mut application, &workload.release, &workload.group)?;
        insert_yaml(
            &mut files,
            PathBuf::from("_nyl/catalog/applications")
                .join(&workload.group.spec.application_namespace)
                .join(format!("{}.yaml", workload.application_name)),
            &application,
        )?;
    }

    for ((cluster, namespace), owner) in namespace_owners {
        let digest = crate::gitops::reconcile::sha256(format!("{cluster}\0{namespace}").as_bytes());
        let suffix = &digest[..20];
        let application_name = format!("nyl-namespace-{suffix}");
        let namespace_directory = PathBuf::from("_nyl/namespaces").join(suffix);
        let key = crate::kubernetes::ResourceKey::from_json_value(&owner.manifest)?;
        let provenance = HashMap::from([(key, owner.provenance.clone())]);
        for (relative, bytes) in render_manifest_layout_with_provenance(&[owner.manifest], &provenance)? {
            insert_file(&mut files, namespace_directory.join(relative), bytes)?;
        }
        let rendered_path = join_posix(target.spec.publication.path_prefix.as_str(), &namespace_directory)?;
        let application = build_directory_application(&DirectoryApplicationInput {
            name: application_name.clone(),
            application_namespace: owner.application_namespace.clone(),
            project: owner.project,
            repo_url: repository.repo_url.clone(),
            revision: target.spec.publication.revision.clone(),
            rendered_path,
            destination: owner.destination,
            destination_namespace: namespace,
            sync_policy: owner.sync_policy,
            deletion_policy: owner.deletion_policy,
            labels: owner.labels,
            annotations: owner.annotations,
        })?;
        insert_yaml(
            &mut files,
            PathBuf::from("_nyl/catalog/applications")
                .join(&owner.application_namespace)
                .join(format!("{application_name}.yaml")),
            &application,
        )?;
    }

    if target.spec.catalog_application.enabled {
        let (parent_name, application) = build_catalog_application(&target, &repository, &argocd)?;
        insert_yaml(
            &mut files,
            PathBuf::from("_nyl/catalog/applications")
                .join(&argocd.resource.spec.namespace)
                .join(format!("{parent_name}.yaml")),
            &application,
        )?;
    }

    let compiled = CompiledTargetTree {
        target,
        cluster,
        repository_name,
        repository,
        files,
        inputs,
    };
    store_cached_target(
        cache,
        cache_probe,
        target_cacheable,
        &target_cache_bypass_reasons,
        &compiled,
        release_count,
        helm_render_count,
    )?;
    observer.finished();
    Ok(compiled)
}

struct TargetCacheProbe {
    key: String,
    recorder: crate::render::cache::DependencyRecorder,
    cacheable: bool,
}

fn prepare_target_cache(
    inventory: &GitOpsInventory,
    target_name: &str,
    session: &RenderSession,
    cache: Option<&GitOpsCache>,
    base_input_paths: &[&Path],
    groups: &[PreparedGroup],
) -> Result<Option<TargetCacheProbe>> {
    let Some(cache) = cache else {
        return Ok(None);
    };
    if cache.mode() == crate::gitops::CacheMode::Disabled {
        return Ok(None);
    }
    let key = format!("{}\0{target_name}", inventory.project_root.display());
    let previous = cache.load_record("target", &key)?;
    let mut recorder = cache.recorder();
    if let Some(config_file) = &inventory.project_config.file {
        recorder.record_path_file(config_file)?;
    }
    for path in base_input_paths {
        recorder.record_path_file(&inventory.project_root.join(path))?;
    }
    for prepared in groups {
        recorder.record_path_file(&inventory.project_root.join(&prepared.group_resource_path))?;
        if let Some(path) = &prepared.project.source_path {
            recorder.record_path_file(&inventory.project_root.join(path))?;
        }
        for path in &prepared.source.provenance_inputs {
            recorder.record_path_file(&inventory.project_root.join(path))?;
        }
        let members = prepared
            .source
            .candidate_files
            .iter()
            .map(|path| {
                path.strip_prefix(&prepared.source.root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();
        recorder.record_value(format!("source:{}:members", prepared.group.metadata.name), &members)?;
        for path in &prepared.source.candidate_files {
            recorder.record_path_file(path)?;
        }
    }
    recorder.record_template_context(&session.template_context().to_json())?;
    cache.record_renderer_tools(&mut recorder)?;
    if let Some(previous) = &previous {
        recorder.replay_filesystem_dependencies(previous)?;
    }
    let cacheable = recorder.is_cacheable();
    Ok(Some(TargetCacheProbe {
        key,
        recorder,
        cacheable,
    }))
}

fn load_cached_target(
    cache: Option<&GitOpsCache>,
    probe: Option<&TargetCacheProbe>,
) -> Result<Option<CompiledTargetTree>> {
    let (Some(cache), Some(probe)) = (cache, probe) else {
        return Ok(None);
    };
    if !probe.cacheable {
        cache.observe(
            CacheLayer::Target,
            CacheOutcome::Bypassed,
            &["unobservable project input".to_string()],
        );
        return Ok(None);
    }
    if cache.mode() == CacheMode::Refresh {
        cache.observe(CacheLayer::Target, CacheOutcome::Refreshed, &[]);
        return Ok(None);
    }
    let Some(record) = cache.load_record("target", &probe.key)? else {
        cache.observe(CacheLayer::Target, CacheOutcome::Miss, &[]);
        return Ok(None);
    };
    let current = probe.recorder.clone().finish("target", String::new());
    if !record.same_inputs(&current) {
        let changed_inputs = record.changed_inputs(&current);
        tracing::debug!(
            target = %probe.key.rsplit('\0').next().unwrap_or(&probe.key),
            changed_inputs = ?changed_inputs,
            "Invalidating cached GitOps target tree"
        );
        cache.observe(CacheLayer::Target, CacheOutcome::Invalidated, &[]);
        return Ok(None);
    }
    let cached: Option<CachedTargetTree> = cache.load_artifact("target", &record.artifact_digest)?;
    if let Some(cached) = cached {
        cache.observe_target_hit(cached.releases, cached.helm_renders);
        tracing::debug!(target = %probe.key.rsplit('\0').next().unwrap_or(&probe.key), "Reusing cached GitOps target tree");
        Ok(Some(cached.compiled))
    } else {
        cache.observe(CacheLayer::Target, CacheOutcome::Miss, &[]);
        Ok(None)
    }
}

fn store_cached_target(
    cache: Option<&GitOpsCache>,
    probe: Option<TargetCacheProbe>,
    cacheable: bool,
    bypass_reasons: &BTreeSet<String>,
    compiled: &CompiledTargetTree,
    releases: usize,
    helm_renders: usize,
) -> Result<()> {
    let (Some(cache), Some(probe)) = (cache, probe) else {
        return Ok(());
    };
    if !cacheable {
        cache.observe(
            CacheLayer::Target,
            CacheOutcome::Bypassed,
            &bypass_reasons.iter().cloned().collect::<Vec<_>>(),
        );
        return Ok(());
    }
    let cached = CachedTargetTreeRef {
        compiled,
        releases,
        helm_renders,
    };
    let Some(digest) = cache.store_artifact("target", &cached)? else {
        return Ok(());
    };
    let record = probe.recorder.finish("target", digest);
    cache.store_record("target", &probe.key, &record)?;
    cache.observe(CacheLayer::Target, CacheOutcome::Stored, &[]);
    Ok(())
}

fn application_name_hint(group: &ApplicationGroup, release: &crate::resources::Release) -> String {
    format!("{}/{}", group.metadata.name, release.metadata.name)
}

fn resolve_namespace_ownership(
    workloads: &mut [PendingWorkload],
    namespace_owners: &mut BTreeMap<(String, String), ManagedNamespaceOwner>,
    cluster: &Cluster,
) -> Result<()> {
    let mut consumers = BTreeMap::<String, BTreeSet<usize>>::new();
    for (index, workload) in workloads.iter().enumerate() {
        consumers
            .entry(workload.destination_namespace.clone())
            .or_default()
            .insert(index);
        for namespace in &workload.release.spec.additional_namespaces {
            consumers.entry(namespace.clone()).or_default().insert(index);
        }
        for manifest in &workload.manifests {
            if let Some(namespace) = manifest.pointer("/metadata/namespace").and_then(Value::as_str) {
                if !namespace.is_empty() {
                    consumers.entry(namespace.to_owned()).or_default().insert(index);
                }
            }
            if is_namespace(manifest) {
                let namespace = manifest
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .expect("Namespace names were validated before ownership resolution");
                consumers.entry(namespace.to_owned()).or_default().insert(index);
            }
        }
    }

    for (namespace, consumer_indexes) in consumers {
        let consumer_indexes = consumer_indexes.into_iter().collect::<Vec<_>>();
        let owner = resolve_shared_namespace_owner(workloads, &namespace, &consumer_indexes)?;
        match owner {
            None => manage_namespace_in_release(&mut workloads[consumer_indexes[0]], &namespace)?,
            Some(SharedNamespaceOwner::Release {
                application_group,
                release,
            }) => {
                let owner_index = resolve_release_namespace_owner(
                    workloads,
                    &namespace,
                    &consumer_indexes,
                    &application_group,
                    &release,
                )?;
                reject_namespace_manifests_from_non_owner(workloads, &namespace, &consumer_indexes, owner_index)?;
                manage_namespace_in_release(&mut workloads[owner_index], &namespace)?;
            }
            Some(SharedNamespaceOwner::Dedicated { application_group }) => {
                reject_all_workload_namespace_manifests(workloads, &namespace, &consumer_indexes, "Dedicated")?;
                let owner_index =
                    resolve_group_namespace_owner(workloads, &namespace, &consumer_indexes, &application_group)?;
                let workload = &workloads[owner_index];
                if !workload.group.spec.namespace.create {
                    return Err(NylError::config(format!(
                        "Shared namespace {namespace:?} uses a Dedicated owner, but ApplicationGroup {application_group:?} has spec.namespace.create disabled"
                    )));
                }
                let mut resources = Vec::new();
                let manifest = take_managed_namespace(&mut resources, &namespace, &workload.group.spec.namespace)?
                    .expect("Dedicated ownership with namespace creation yields a manifest");
                register_namespace_owner(
                    namespace_owners,
                    cluster,
                    &workload.group,
                    &workload.argocd_project_name,
                    &namespace,
                    manifest,
                    generated_namespace_provenance(workload, &namespace),
                )?;
            }
            Some(SharedNamespaceOwner::External) => {
                reject_all_workload_namespace_manifests(workloads, &namespace, &consumer_indexes, "External")?;
            }
        }
    }
    Ok(())
}

fn resolve_shared_namespace_owner(
    workloads: &[PendingWorkload],
    namespace: &str,
    consumer_indexes: &[usize],
) -> Result<Option<SharedNamespaceOwner>> {
    let group_indexes = consumer_indexes
        .iter()
        .map(|index| (workloads[*index].group.metadata.name.as_str(), *index))
        .collect::<BTreeMap<_, _>>();
    let declarations = group_indexes
        .values()
        .filter_map(|index| {
            workloads[*index]
                .group
                .spec
                .shared_namespaces
                .get(namespace)
                .map(|policy| (workloads[*index].group.metadata.name.as_str(), &policy.owner))
        })
        .collect::<Vec<_>>();

    if declarations.is_empty() {
        if is_kubernetes_bootstrap_namespace(namespace) {
            return Ok(Some(SharedNamespaceOwner::External));
        }
        if consumer_indexes.len() == 1 {
            return Ok(None);
        }
        return Err(NylError::config(format!(
            "Namespace {namespace:?} is consumed by multiple workload Applications [{}]; every contributing ApplicationGroup must declare the same spec.sharedNamespaces.{namespace} owner",
            consumer_indexes
                .iter()
                .map(|index| application_name_hint(&workloads[*index].group, &workloads[*index].release))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if declarations.len() != group_indexes.len() {
        let declared = declarations.iter().map(|(group, _)| *group).collect::<BTreeSet<_>>();
        let missing = group_indexes
            .keys()
            .filter(|group| !declared.contains(**group))
            .copied()
            .collect::<Vec<_>>();
        return Err(NylError::config(format!(
            "Namespace {namespace:?} is shared across ApplicationGroups, but [{}] do not declare spec.sharedNamespaces.{namespace}",
            missing.join(", ")
        )));
    }
    let owner = declarations[0].1;
    if declarations.iter().any(|(_, candidate)| *candidate != owner) {
        return Err(NylError::config(format!(
            "ApplicationGroups declare conflicting owners for shared namespace {namespace:?}"
        )));
    }
    Ok(Some(owner.clone()))
}

fn is_kubernetes_bootstrap_namespace(namespace: &str) -> bool {
    matches!(namespace, "default" | "kube-system" | "kube-public" | "kube-node-lease")
}

fn resolve_release_namespace_owner(
    workloads: &[PendingWorkload],
    namespace: &str,
    consumer_indexes: &[usize],
    application_group: &str,
    release: &str,
) -> Result<usize> {
    let matches = consumer_indexes
        .iter()
        .copied()
        .filter(|index| {
            workloads[*index].group.metadata.name == application_group
                && workloads[*index].release.metadata.name == release
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => Err(NylError::config(format!(
            "Shared namespace {namespace:?} owner references Release {application_group}/{release}, which does not consume that namespace"
        ))),
        _ => Err(NylError::config(format!(
            "Shared namespace {namespace:?} owner Release {application_group}/{release} is ambiguous"
        ))),
    }
}

fn resolve_group_namespace_owner(
    workloads: &[PendingWorkload],
    namespace: &str,
    consumer_indexes: &[usize],
    application_group: &str,
) -> Result<usize> {
    consumer_indexes
        .iter()
        .copied()
        .find(|index| workloads[*index].group.metadata.name == application_group)
        .ok_or_else(|| {
            NylError::config(format!(
                "Shared namespace {namespace:?} Dedicated owner references ApplicationGroup {application_group:?}, which does not consume that namespace"
            ))
        })
}

fn manage_namespace_in_release(workload: &mut PendingWorkload, namespace: &str) -> Result<()> {
    let existed = workload_renders_namespace(workload, namespace);
    ensure_managed_namespace(&mut workload.manifests, namespace, &workload.group.spec.namespace)?;
    if !existed && workload_renders_namespace(workload, namespace) {
        let manifest = workload
            .manifests
            .iter()
            .find(|manifest| {
                is_namespace(manifest) && manifest.pointer("/metadata/name").and_then(Value::as_str) == Some(namespace)
            })
            .expect("managed Namespace was added");
        let key = crate::kubernetes::ResourceKey::from_json_value(manifest)?;
        workload
            .manifest_provenance
            .insert(key, generated_namespace_provenance(workload, namespace));
    }
    Ok(())
}

fn generated_namespace_provenance(workload: &PendingWorkload, namespace: &str) -> String {
    let mut provenance = workload.release_provenance.clone().unwrap_or_default();
    if !provenance.is_empty() {
        provenance.push('\n');
    }
    let _ = write!(
        provenance,
        "Generated: Namespace {namespace:?} for Release {:?}",
        workload.release.metadata.name
    );
    provenance
}

fn reject_namespace_manifests_from_non_owner(
    workloads: &[PendingWorkload],
    namespace: &str,
    consumer_indexes: &[usize],
    owner_index: usize,
) -> Result<()> {
    for index in consumer_indexes.iter().copied().filter(|index| *index != owner_index) {
        if workload_renders_namespace(&workloads[index], namespace) {
            return Err(NylError::config(format!(
                "Release {:?} renders shared Namespace {namespace:?}, but ownership is delegated to another Release",
                application_name_hint(&workloads[index].group, &workloads[index].release)
            )));
        }
    }
    Ok(())
}

fn reject_all_workload_namespace_manifests(
    workloads: &[PendingWorkload],
    namespace: &str,
    consumer_indexes: &[usize],
    owner_kind: &str,
) -> Result<()> {
    if let Some(index) = consumer_indexes
        .iter()
        .copied()
        .find(|index| workload_renders_namespace(&workloads[*index], namespace))
    {
        return Err(NylError::config(format!(
            "Release {:?} renders Namespace {namespace:?}, but its configured owner kind is {owner_kind}",
            application_name_hint(&workloads[index].group, &workloads[index].release)
        )));
    }
    Ok(())
}

fn workload_renders_namespace(workload: &PendingWorkload, namespace: &str) -> bool {
    workload.manifests.iter().any(|manifest| {
        is_namespace(manifest) && manifest.pointer("/metadata/name").and_then(Value::as_str) == Some(namespace)
    })
}

fn is_namespace(manifest: &Value) -> bool {
    manifest.get("apiVersion").and_then(Value::as_str) == Some("v1")
        && manifest.get("kind").and_then(Value::as_str) == Some("Namespace")
}

fn validate_release_namespace_scope(
    manifests: &[Value],
    release: &crate::resources::Release,
    destination_namespace: &str,
) -> Vec<NamespaceScopeIssue> {
    let mut allowed_namespaces = BTreeSet::from([destination_namespace.to_owned()]);
    allowed_namespaces.extend(release.spec.additional_namespaces.iter().cloned());
    let mut errors = Vec::new();
    for manifest in manifests {
        let resource = manifest_identity_hint(manifest);
        if let Some(namespace) = manifest.pointer("/metadata/namespace") {
            match namespace {
                Value::Null => {}
                Value::String(namespace) if namespace.is_empty() => {}
                Value::String(namespace) if allowed_namespaces.contains(namespace) => {}
                Value::String(namespace) => {
                    errors.push(NamespaceScopeIssue {
                        release: release.metadata.name.clone(),
                        allowed_namespaces: allowed_namespaces.clone(),
                        resource: resource.clone(),
                        violation: NamespaceScopeViolation::UnexpectedNamespace(namespace.clone()),
                    });
                }
                _ => {
                    errors.push(NamespaceScopeIssue {
                        release: release.metadata.name.clone(),
                        allowed_namespaces: allowed_namespaces.clone(),
                        resource: resource.clone(),
                        violation: NamespaceScopeViolation::InvalidResourceNamespace,
                    });
                }
            }
        }
        if is_namespace(manifest) {
            match manifest.pointer("/metadata/name").and_then(Value::as_str) {
                Some(name) if allowed_namespaces.contains(name) => {}
                Some(name) => errors.push(NamespaceScopeIssue {
                    release: release.metadata.name.clone(),
                    allowed_namespaces: allowed_namespaces.clone(),
                    resource: resource.clone(),
                    violation: NamespaceScopeViolation::UnexpectedNamespace(name.to_owned()),
                }),
                None => errors.push(NamespaceScopeIssue {
                    release: release.metadata.name.clone(),
                    allowed_namespaces: allowed_namespaces.clone(),
                    resource: resource.clone(),
                    violation: NamespaceScopeViolation::InvalidNamespaceName,
                }),
            }
        }
    }
    errors
}

fn format_namespace_scope_issues(issues: &[NamespaceScopeIssue]) -> String {
    let mut releases = BTreeMap::<(String, BTreeSet<String>), Vec<&NamespaceScopeIssue>>::new();
    for issue in issues {
        releases
            .entry((issue.release.clone(), issue.allowed_namespaces.clone()))
            .or_default()
            .push(issue);
    }

    let mut output = format!(
        "Rendered namespace scope validation found {} {} across {} {}:",
        issues.len(),
        if issues.len() == 1 { "issue" } else { "issues" },
        releases.len(),
        if releases.len() == 1 { "release" } else { "releases" }
    );
    for ((release, allowed_namespaces), release_issues) in releases {
        let allowed_display = allowed_namespaces
            .iter()
            .map(|namespace| format!("{namespace:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            output,
            "\n\nRelease {release:?} ({} {})\n  Allowed namespaces: {allowed_display}",
            release_issues.len(),
            if release_issues.len() == 1 { "issue" } else { "issues" }
        );

        let mut violations = BTreeMap::<NamespaceScopeViolation, Vec<&str>>::new();
        for issue in release_issues {
            violations
                .entry(issue.violation.clone())
                .or_default()
                .push(issue.resource.as_str());
        }
        for (violation, mut resources) in violations {
            resources.sort_unstable();
            let heading = match violation {
                NamespaceScopeViolation::UnexpectedNamespace(namespace) => {
                    format!("Unexpected namespace {namespace:?}")
                }
                NamespaceScopeViolation::InvalidResourceNamespace => {
                    "Invalid metadata.namespace (expected a string)".to_owned()
                }
                NamespaceScopeViolation::InvalidNamespaceName => {
                    "Invalid Namespace metadata.name (expected a string)".to_owned()
                }
            };
            let _ = write!(
                output,
                "\n  {heading} ({} {}):",
                resources.len(),
                if resources.len() == 1 { "resource" } else { "resources" }
            );
            for resource in resources {
                let _ = write!(output, "\n    - {resource}");
            }
        }
    }
    output
}

fn manifest_identity_hint(manifest: &Value) -> String {
    let kind = manifest.get("kind").and_then(Value::as_str).unwrap_or("resource");
    match manifest.pointer("/metadata/name").and_then(Value::as_str) {
        Some(name) => format!("{kind} {name:?}"),
        None => kind.to_owned(),
    }
}

fn register_namespace_owner(
    namespace_owners: &mut BTreeMap<(String, String), ManagedNamespaceOwner>,
    cluster: &Cluster,
    group: &ApplicationGroup,
    argocd_project_name: &str,
    namespace: &str,
    manifest: Value,
    provenance: String,
) -> Result<()> {
    let key = (cluster.metadata.name.clone(), namespace.to_owned());
    let owner = ManagedNamespaceOwner {
        manifest,
        provenance,
        application_namespace: group.spec.application_namespace.clone(),
        project: argocd_project_name.to_owned(),
        destination: cluster.spec.destination.clone(),
        sync_policy: group.spec.sync_policy.clone(),
        deletion_policy: group.spec.application_deletion_policy,
        labels: group.spec.labels.clone(),
        annotations: group.spec.annotations.clone(),
    };
    if let Some(existing) = namespace_owners.get(&key) {
        if existing != &owner {
            return Err(NylError::config(format!(
                "Namespace {namespace:?} has conflicting ownership policy across ApplicationGroups"
            )));
        }
    } else {
        namespace_owners.insert(key, owner);
    }
    Ok(())
}

fn target_selects_group(target: &GitOpsTarget, group_labels: &BTreeMap<String, String>) -> bool {
    target
        .spec
        .application_group_selector
        .match_labels
        .iter()
        .all(|(key, value)| group_labels.get(key) == Some(value))
}

fn resolve_argocd_instance(
    inventory: &GitOpsInventory,
    target: &GitOpsTarget,
    instance_count: usize,
) -> Result<EffectiveArgoCDInstance> {
    if instance_count == 0 {
        if let Some(reference) = &target.spec.argocd_ref {
            return Err(NylError::config(format!(
                "GitOpsTarget {:?} references ArgoCDInstance {:?}, but no ArgoCDInstance resources are defined",
                target.metadata.name, reference.name
            )));
        }
        let (cluster, cluster_path) = resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
        return Ok(EffectiveArgoCDInstance {
            identity: format!("implicit:{}", target.metadata.name),
            resource: ArgoCDInstance {
                api_version: crate::constants::API_VERSION_GITOPS.to_owned(),
                kind: crate::resources::KIND_ARGOCD_INSTANCE.to_owned(),
                metadata: crate::resources::GitOpsResourceMetadata {
                    name: format!("{}-implicit", target.metadata.name),
                    labels: BTreeMap::new(),
                },
                spec: ArgoCDInstanceSpec {
                    cluster_ref: target.spec.cluster_ref.clone(),
                    namespace: "argocd".to_owned(),
                    catalog_application_defaults: CatalogApplicationDefaults::default(),
                },
            },
            source_path: None,
            cluster,
            cluster_path,
        });
    }

    let reference = target.spec.argocd_ref.as_ref().ok_or_else(|| {
        NylError::config(format!(
            "GitOpsTarget {:?} must set spec.argocdRef because explicit ArgoCDInstance resources are defined",
            target.metadata.name
        ))
    })?;
    let discovered = inventory
        .get(GitOpsResourceKind::ArgoCDInstance, &reference.name)
        .ok_or_else(|| NylError::config(format!("ArgoCDInstance {:?} was not found", reference.name)))?;
    let Some(GitOpsResource::ArgoCDInstance(instance)) = &discovered.resource else {
        return Err(NylError::config(format!(
            "ArgoCDInstance {:?} must be static",
            reference.name
        )));
    };
    let (cluster, cluster_path) = resolve_cluster(inventory, &instance.spec.cluster_ref.name)?;
    Ok(EffectiveArgoCDInstance {
        identity: reference.name.clone(),
        resource: instance.clone(),
        source_path: Some(discovered.source_path.clone()),
        cluster,
        cluster_path,
    })
}

fn build_catalog_application(
    target: &GitOpsTarget,
    repository: &InlineGitRepository,
    argocd: &EffectiveArgoCDInstance,
) -> Result<(String, Value)> {
    let defaults = &argocd.resource.spec.catalog_application_defaults;
    let overrides = &target.spec.catalog_application;
    let name = overrides
        .name
        .clone()
        .unwrap_or_else(|| format!("{}-catalog", target.metadata.name));
    let project = overrides.project.clone().unwrap_or_else(|| defaults.project.clone());
    let sync_policy = overrides
        .sync_policy
        .clone()
        .unwrap_or_else(|| defaults.sync_policy.clone());
    let deletion_policy = overrides
        .application_deletion_policy
        .unwrap_or(defaults.application_deletion_policy);
    let self_prune_policy = overrides.self_prune_policy.unwrap_or(defaults.self_prune_policy);
    let mut labels = defaults.labels.clone();
    labels.extend(overrides.labels.clone());
    let mut annotations = defaults.annotations.clone();
    annotations.extend(overrides.annotations.clone());
    set_catalog_prune_option(&mut annotations, self_prune_policy);
    let rendered_path = join_posix(&target.spec.publication.path_prefix, Path::new("_nyl/catalog"))?;
    let application = build_directory_application(&DirectoryApplicationInput {
        name: name.clone(),
        application_namespace: argocd.resource.spec.namespace.clone(),
        project,
        repo_url: repository.repo_url.clone(),
        revision: target.spec.publication.revision.clone(),
        rendered_path,
        destination: argocd.cluster.spec.destination.clone(),
        destination_namespace: argocd.resource.spec.namespace.clone(),
        sync_policy: Some(sync_policy),
        deletion_policy,
        labels,
        annotations,
    })?;
    Ok((name, application))
}

fn set_catalog_prune_option(annotations: &mut BTreeMap<String, String>, policy: ManagedResourceDeletionPolicy) {
    const KEY: &str = "argocd.argoproj.io/sync-options";
    let mut options = annotations
        .get(KEY)
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.starts_with("Prune="))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    match policy {
        ManagedResourceDeletionPolicy::Automatic => {}
        ManagedResourceDeletionPolicy::Confirm => options.push("Prune=confirm".to_owned()),
        ManagedResourceDeletionPolicy::Retain => options.push("Prune=false".to_owned()),
    }
    if options.is_empty() {
        annotations.remove(KEY);
    } else {
        annotations.insert(KEY.to_owned(), options.join(","));
    }
}

// Keep the cross-target checks together so every generated Argo CD identity is
// audited in one pass over each target's effective configuration.
#[allow(clippy::too_many_lines)]
fn validate_same_instance_catalog_collisions(inventory: &GitOpsInventory, instance_count: usize) -> Result<()> {
    let targets = inventory
        .resources
        .values()
        .filter_map(|resource| match &resource.resource {
            Some(GitOpsResource::GitOpsTarget(target)) => Some(target),
            _ => None,
        })
        .collect::<Vec<_>>();
    let groups = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == GitOpsResourceKind::ApplicationGroup)
        .collect::<Vec<_>>();
    let mut parent_owners = BTreeMap::<(String, String, String), String>::new();
    let mut project_owners = BTreeMap::<(String, String, String), (String, String, bool, String)>::new();
    let mut default_application_owners = BTreeMap::<(String, String, String), String>::new();

    for target in targets {
        let argocd = resolve_argocd_instance(inventory, target, instance_count)?;
        if target.spec.catalog_application.enabled {
            let parent_name = target
                .spec
                .catalog_application
                .name
                .clone()
                .unwrap_or_else(|| format!("{}-catalog", target.metadata.name));
            let key = (
                argocd.identity.clone(),
                argocd.resource.spec.namespace.clone(),
                parent_name.clone(),
            );
            if let Some(previous) = parent_owners.insert(key, target.metadata.name.clone()) {
                return Err(NylError::config(format!(
                    "GitOpsTargets {previous:?} and {:?} generate the same catalog Application {}/{} on ArgoCDInstance {:?}; customize spec.catalogApplication.name",
                    target.metadata.name, argocd.resource.spec.namespace, parent_name, argocd.identity
                )));
            }
        }
        let (cluster, _) = resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
        let session = RenderSession::for_target(&inventory.project_root, &inventory.project_config, target, &cluster)?;
        for discovered in &groups {
            if !target_selects_group(target, &discovered.static_labels) {
                continue;
            }
            let Some(GitOpsResource::ApplicationGroup(group)) = render_effective_control(discovered, &session)? else {
                continue;
            };
            if !group.spec.enabled {
                continue;
            }
            if group.spec.application_name_template.is_none() {
                let key = (
                    argocd.identity.clone(),
                    group.spec.application_namespace.clone(),
                    group.metadata.name.clone(),
                );
                if let Some(previous) = default_application_owners.insert(key, target.metadata.name.clone()) {
                    return Err(NylError::config(format!(
                        "GitOpsTargets {previous:?} and {:?} select ApplicationGroup {:?} on ArgoCDInstance {:?} while using the default Release Application names; set ApplicationGroup.spec.applicationNameTemplate, for example \"{{{{ target.metadata.name }}}}-{{{{ release.metadata.name }}}}\"",
                        target.metadata.name, group.metadata.name, argocd.identity
                    )));
                }
            }
            let (catalog_id, project_name, rendered, referenced) = if let Some(reference) = &group.spec.project_ref {
                let (_, _, project) = resolve_effective_project(inventory, reference, &session)?;
                (
                    reference.clone(),
                    app_project_name(&project)?,
                    project.spec.management == AppProjectManagement::Rendered,
                    true,
                )
            } else {
                let template = group
                    .spec
                    .project_template
                    .as_ref()
                    .expect("validated project template");
                (
                    group.metadata.name.clone(),
                    template.name.clone().unwrap_or_else(|| group.metadata.name.clone()),
                    true,
                    false,
                )
            };
            if rendered {
                let key = (
                    argocd.identity.clone(),
                    argocd.resource.spec.namespace.clone(),
                    project_name.clone(),
                );
                let owner = (
                    target.metadata.name.clone(),
                    group.metadata.name.clone(),
                    referenced,
                    catalog_id.clone(),
                );
                if let Some(previous) = project_owners.get(&key) {
                    if referenced && previous.2 && previous.0 == target.metadata.name && previous.3 == catalog_id {
                        continue;
                    }
                    let field = if group.spec.project_template.is_some() {
                        "ApplicationGroup.spec.projectTemplate.name"
                    } else {
                        "a target-specific AppProjectDefinition name or management: External"
                    };
                    return Err(NylError::config(format!(
                        "{}/{} and {}/{} generate the same AppProject {}/{} on ArgoCDInstance {:?} (catalog id {catalog_id:?}); customize {field}",
                        previous.0,
                        previous.1,
                        target.metadata.name,
                        group.metadata.name,
                        argocd.resource.spec.namespace,
                        project_name,
                        argocd.identity
                    )));
                }
                project_owners.insert(key, owner);
            }
        }
    }
    Ok(())
}

fn resolve_git_publication(
    inventory: &GitOpsInventory,
    publication: &GitPublication,
) -> Result<(Option<String>, InlineGitRepository, Option<PathBuf>)> {
    if let Some(repository) = &publication.repository {
        return Ok((None, repository.clone(), None));
    }
    let reference = publication
        .repository_ref
        .as_ref()
        .expect("validated publication has a repository reference or inline repository");
    let discovered = inventory
        .get(GitOpsResourceKind::GitRepository, &reference.name)
        .ok_or_else(|| NylError::config(format!("GitRepository {:?} was not found", reference.name)))?;
    let Some(GitOpsResource::GitRepository(repository)) = &discovered.resource else {
        unreachable!("inventory kind key and resource variant must agree");
    };
    Ok((
        Some(reference.name.clone()),
        InlineGitRepository {
            repo_url: repository.spec.repo_url.clone(),
            publish_url: repository.spec.publish_url.clone(),
        },
        Some(discovered.source_path.clone()),
    ))
}

fn resolve_cluster(inventory: &GitOpsInventory, name: &str) -> Result<(Cluster, PathBuf)> {
    let discovered = inventory
        .get(GitOpsResourceKind::Cluster, name)
        .ok_or_else(|| NylError::config(format!("Cluster {name:?} was not found")))?;
    let Some(GitOpsResource::Cluster(cluster)) = &discovered.resource else {
        return Err(NylError::config(format!("Cluster {name:?} must be static")));
    };
    Ok((cluster.clone(), discovered.source_path.clone()))
}

fn resolve_project(inventory: &GitOpsInventory, project_ref: &str) -> Result<()> {
    inventory
        .get(GitOpsResourceKind::AppProjectDefinition, project_ref)
        .ok_or_else(|| NylError::config(format!("AppProjectDefinition {project_ref:?} was not found")))?;
    Ok(())
}

fn resolve_effective_project(
    inventory: &GitOpsInventory,
    project_ref: &str,
    session: &RenderSession,
) -> Result<(String, PathBuf, AppProjectDefinition)> {
    let discovered = inventory
        .get(GitOpsResourceKind::AppProjectDefinition, project_ref)
        .ok_or_else(|| NylError::config(format!("AppProjectDefinition {project_ref:?} was not found")))?;
    let Some(GitOpsResource::AppProjectDefinition(project)) = render_effective_control(discovered, session)? else {
        return Err(NylError::config(format!(
            "AppProjectDefinition {project_ref:?} is omitted for target {}",
            session.target_name()
        )));
    };
    Ok((project_ref.to_string(), discovered.source_path.clone(), project))
}

fn resolve_effective_group_project(
    inventory: &GitOpsInventory,
    group: &ApplicationGroup,
    session: &RenderSession,
    publication_repository: &InlineGitRepository,
    target_cluster: &Cluster,
    argocd: &ArgoCDInstance,
) -> Result<EffectiveProject> {
    if let Some(project_ref) = &group.spec.project_ref {
        let (project_id, project_path, project) = resolve_effective_project(inventory, project_ref, session)?;
        let name = app_project_name(&project)?;
        let manifest = if project.spec.management == AppProjectManagement::Rendered {
            let mut manifest = project.spec.manifest;
            manifest["metadata"]["namespace"] = argocd.spec.namespace.clone().into();
            Some(manifest)
        } else {
            None
        };
        return Ok(EffectiveProject {
            catalog_id: project_id,
            source_path: Some(project_path),
            name,
            manifest,
            destination_namespaces: None,
        });
    }

    let template = group
        .spec
        .project_template
        .as_ref()
        .expect("ApplicationGroup validation requires a projectRef or projectTemplate");
    let name = template.name.clone().unwrap_or_else(|| group.metadata.name.clone());
    let mut destination_namespaces = template.destination_namespaces.clone();
    if let Some(namespace) = &group.spec.destination_namespace {
        if !namespace_matches_any(namespace, &destination_namespaces) {
            destination_namespaces.push(namespace.clone());
        }
    }
    if group.spec.destination_namespace.is_none() && destination_namespaces.is_empty() {
        return Err(NylError::config(format!(
            "ApplicationGroup {:?} projectTemplate requires destinationNamespaces when spec.destinationNamespace is absent",
            group.metadata.name
        )));
    }
    destination_namespaces.sort();
    destination_namespaces.dedup();

    let mut cluster_resources = template
        .cluster_resource_whitelist
        .iter()
        .map(|pattern| {
            let mut value = serde_json::Map::from_iter([
                ("group".to_owned(), pattern.group.clone().into()),
                ("kind".to_owned(), pattern.kind.clone().into()),
            ]);
            if let Some(name) = &pattern.name {
                value.insert("name".to_owned(), name.clone().into());
            }
            Value::Object(value)
        })
        .collect::<Vec<_>>();
    if group.spec.namespace.create {
        for namespace in &destination_namespaces {
            let permission = serde_json::json!({"group": "", "kind": "Namespace", "name": namespace});
            if !cluster_resources.contains(&permission) {
                cluster_resources.push(permission);
            }
        }
    }

    let destinations = destination_namespaces
        .iter()
        .map(|namespace| {
            let mut destination = serde_json::Map::from_iter([("namespace".to_owned(), namespace.clone().into())]);
            if let Some(server) = &target_cluster.spec.destination.server {
                destination.insert("server".to_owned(), server.clone().into());
            }
            if let Some(cluster_name) = &target_cluster.spec.destination.name {
                destination.insert("name".to_owned(), cluster_name.clone().into());
            }
            Value::Object(destination)
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "apiVersion": "argoproj.io/v1alpha1",
        "kind": "AppProject",
        "metadata": {"name": name, "namespace": argocd.spec.namespace},
        "spec": {
            "sourceRepos": [publication_repository.repo_url],
            "sourceNamespaces": [group.spec.application_namespace],
            "destinations": destinations,
            "clusterResourceWhitelist": cluster_resources,
        }
    });
    Ok(EffectiveProject {
        catalog_id: group.metadata.name.clone(),
        source_path: None,
        name,
        manifest: Some(manifest),
        destination_namespaces: Some(destination_namespaces),
    })
}

fn namespace_matches_any(namespace: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| Pattern::new(pattern).is_ok_and(|pattern| pattern.matches(namespace)))
}

fn validate_project_namespace_scope(
    group: &ApplicationGroup,
    release: &crate::resources::Release,
    destination_namespace: &str,
    patterns: &[String],
) -> Result<()> {
    for namespace in
        std::iter::once(destination_namespace).chain(release.spec.additional_namespaces.iter().map(String::as_str))
    {
        if !namespace_matches_any(namespace, patterns) {
            return Err(NylError::config(format!(
                "Release {}/{} uses namespace {namespace:?}, which is outside ApplicationGroup.spec.projectTemplate.destinationNamespaces [{}]",
                group.metadata.name,
                release.metadata.name,
                patterns.iter().map(|value| format!("{value:?}")).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(())
}

fn app_project_name(project: &AppProjectDefinition) -> Result<String> {
    project
        .spec
        .manifest
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| NylError::config("AppProjectDefinition spec.manifest.metadata.name must be a string"))
}

fn render_effective_control(
    discovered: &super::DiscoveredGitOpsResource,
    session: &RenderSession,
) -> Result<Option<GitOpsResource>> {
    let rendered = TemplateEngine::new().render_named(
        &discovered.source_path.display().to_string(),
        &discovered.raw_document,
        &session.template_context().to_json(),
    )?;
    let documents = SourceContext::new(discovered.source_path.clone()).parse_yaml_documents(&rendered)?;
    if documents.is_empty() {
        return Ok(None);
    }
    if documents.len() != 1 {
        return Err(NylError::config(format!(
            "GitOps resource {} document {} renders more than one document",
            discovered.source_path.display(),
            discovered.document_index
        )));
    }
    let effective = crate::resources::parse_gitops_resource(&documents[0])?.ok_or_else(|| {
        NylError::config(format!(
            "GitOps resource {} document {} does not retain its static envelope after target rendering",
            discovered.source_path.display(),
            discovered.document_index
        ))
    })?;
    let effective_identity = match &effective {
        GitOpsResource::GitRepository(resource) => (GitOpsResourceKind::GitRepository, &resource.metadata.name),
        GitOpsResource::Cluster(resource) => (GitOpsResourceKind::Cluster, &resource.metadata.name),
        GitOpsResource::ArgoCDInstance(resource) => (GitOpsResourceKind::ArgoCDInstance, &resource.metadata.name),
        GitOpsResource::GitOpsTarget(resource) => (GitOpsResourceKind::GitOpsTarget, &resource.metadata.name),
        GitOpsResource::AppProjectDefinition(resource) => {
            (GitOpsResourceKind::AppProjectDefinition, &resource.metadata.name)
        }
        GitOpsResource::ApplicationGroup(resource) => (GitOpsResourceKind::ApplicationGroup, &resource.metadata.name),
    };
    if effective_identity.0 != discovered.identity.kind || effective_identity.1 != &discovered.identity.name {
        return Err(NylError::config(format!(
            "GitOps resource {} document {} must retain static identity {}/{} after target rendering",
            discovered.source_path.display(),
            discovered.document_index,
            discovered.identity.kind.as_str(),
            discovered.identity.name
        )));
    }
    Ok(Some(effective))
}

struct ResolvedGroupSource {
    root: PathBuf,
    candidate_files: Vec<PathBuf>,
    files: Vec<StaticReleaseFile>,
    renderer_mode: RendererConfigMode,
    source_session: Option<RenderSession>,
    remote: bool,
    provenance_inputs: Vec<PathBuf>,
}

struct StaticReleaseFile {
    path: PathBuf,
    name: Option<String>,
}

fn resolve_group_source(
    inventory: &GitOpsInventory,
    group_resource_path: &Path,
    group: &ApplicationGroup,
    target: &GitOpsTarget,
    git_manager: &mut Option<GitManager>,
    cache: Option<&GitOpsCache>,
) -> Result<ResolvedGroupSource> {
    let mut provenance_inputs = Vec::new();
    let (root, source, remote_root) = match &group.spec.source {
        Some(source) if source.is_remote() => {
            let (repository, repository_path) = resolve_source_repository(inventory, source)?;
            if let Some(repository_path) = repository_path {
                provenance_inputs.push(repository_path);
            }
            let manager = match git_manager {
                Some(manager) => manager,
                None => git_manager.insert(
                    if let Some(cache_root) = cache.and_then(GitOpsCache::external_cache_root) {
                        GitManager::with_cache_dir(cache_root)
                    } else {
                        GitManager::new().map_err(NylError::Git)?
                    },
                ),
            };
            let commit = source.commit.as_deref().expect("validated remote source has commit");
            let checkout = manager
                .resolve_ref(&repository.repo_url, Some(commit), None)
                .map_err(NylError::Git)?;
            let selected = checked_checkout_subpath(&checkout, &source.path, "ApplicationGroup source.path")?;
            (selected, source.clone(), Some(checkout))
        }
        Some(source) => (inventory.project_root.join(&source.path), source.clone(), None),
        None => {
            let root =
                if group_resource_path.file_name().and_then(|name| name.to_str()) == Some("_application-group.yaml") {
                    inventory
                        .project_root
                        .join(group_resource_path.parent().unwrap_or_else(|| Path::new("")))
                } else {
                    inventory.project_root.join("applications").join(&group.metadata.name)
                };
            (
                root,
                ApplicationGroupSource {
                    repository_ref: None,
                    repository: None,
                    revision: None,
                    commit: None,
                    path: String::new(),
                    include: vec!["*.yaml".to_string(), "*.yml".to_string()],
                    exclude: Vec::new(),
                    recursive: true,
                    renderer_config: RendererConfig::default(),
                },
                None,
            )
        }
    };
    if !root.is_dir() {
        return Err(NylError::config(format!(
            "ApplicationGroup {:?} source directory does not exist: {}",
            group.metadata.name,
            root.display()
        )));
    }

    let mut files = if remote_root.is_some() {
        collect_checkout_yaml(&root)?
    } else {
        inventory
            .yaml_files
            .iter()
            .map(|path| inventory.project_root.join(path))
            .filter(|path| path.starts_with(&root))
            .collect()
    };
    let control_files = inventory
        .resources
        .values()
        .map(|resource| inventory.project_root.join(&resource.source_path))
        .collect::<BTreeSet<_>>();
    files.retain(|path| !control_files.contains(path) && source_matches(&root, path, &source));
    files.sort();
    let candidate_files = files;
    let files = static_release_files(&candidate_files)?;

    let source_session = build_group_source_session(inventory, target, remote_root.as_deref(), &source)?;

    Ok(ResolvedGroupSource {
        root,
        candidate_files,
        files,
        renderer_mode: source.renderer_config.mode,
        source_session,
        remote: remote_root.is_some(),
        provenance_inputs,
    })
}

fn build_group_source_session(
    inventory: &GitOpsInventory,
    target: &GitOpsTarget,
    remote_root: Option<&Path>,
    source: &ApplicationGroupSource,
) -> Result<Option<RenderSession>> {
    Ok(match (remote_root, source.renderer_config.mode) {
        (Some(remote_root), RendererConfigMode::Remote) => {
            let project_root = checked_checkout_subpath(
                remote_root,
                source.renderer_config.project_path.as_deref().unwrap_or("."),
                "ApplicationGroup source.rendererConfig.projectPath",
            )?;
            let (cluster, _) = resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
            Some(RenderSession::for_remote_target(&project_root, target, &cluster)?)
        }
        (Some(_), RendererConfigMode::Central) => {
            let (cluster, _) = resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
            Some(RenderSession::for_untrusted_source(
                &inventory.project_root,
                &inventory.project_config,
                target,
                &cluster,
            )?)
        }
        (None, _) => None,
    })
}

fn static_release_files(files: &[PathBuf]) -> Result<Vec<StaticReleaseFile>> {
    let mut release_files = Vec::new();
    for path in files {
        if let Some(envelope) = crate::render::static_release_envelope(path)? {
            release_files.push(StaticReleaseFile {
                path: path.clone(),
                name: envelope.name,
            });
        }
    }
    Ok(release_files)
}

fn manifest_path_identity(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn unclaimed_group_manifests<'a>(
    source: &'a ResolvedGroupSource,
    claimed_source_files: &BTreeSet<PathBuf>,
) -> Vec<&'a Path> {
    source
        .candidate_files
        .iter()
        .filter(|path| !claimed_source_files.contains(&manifest_path_identity(path)))
        .map(PathBuf::as_path)
        .collect()
}

fn warn_unclaimed_group_manifests(
    group: &ApplicationGroup,
    source: &ResolvedGroupSource,
    claimed_source_files: &BTreeSet<PathBuf>,
) {
    for path in unclaimed_group_manifests(source, claimed_source_files) {
        let display_path = path.strip_prefix(&source.root).unwrap_or(path);
        tracing::warn!(
            application_group = %group.metadata.name,
            manifest = %display_path.display(),
            "ApplicationGroup ignored manifest because it contains no literal gitops.nyl/v1 Release and is not included by another Release"
        );
    }
}

fn checked_checkout_subpath(checkout: &Path, relative: &str, field: &str) -> Result<PathBuf> {
    crate::resources::validate_relative_path(field, relative, true, true)?;
    let canonical_checkout = checkout.canonicalize().map_err(|error| {
        NylError::config(format!(
            "Failed to resolve remote checkout {}: {error}",
            checkout.display()
        ))
    })?;
    let selected = checkout.join(relative);
    let relative_path = selected
        .strip_prefix(checkout)
        .map_err(|error| NylError::config(format!("{field} {relative:?} escapes remote checkout: {error}")))?;
    let mut current = checkout.to_path_buf();
    for component in relative_path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(NylError::config(format!(
                    "{field} {relative:?} traverses symbolic link {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let canonical_selected = selected
        .canonicalize()
        .map_err(|error| NylError::config(format!("Failed to resolve {field} {relative:?}: {error}")))?;
    if !canonical_selected.starts_with(&canonical_checkout) {
        return Err(NylError::config(format!(
            "{field} {relative:?} resolves outside remote checkout {}",
            checkout.display()
        )));
    }
    Ok(canonical_selected)
}

fn resolve_source_repository(
    inventory: &GitOpsInventory,
    source: &ApplicationGroupSource,
) -> Result<(InlineGitRepository, Option<PathBuf>)> {
    if let Some(repository) = &source.repository {
        return Ok((repository.clone(), None));
    }
    let reference = source
        .repository_ref
        .as_ref()
        .expect("validated remote source has repositoryRef or repository");
    let discovered = inventory
        .get(GitOpsResourceKind::GitRepository, &reference.name)
        .ok_or_else(|| NylError::config(format!("GitRepository {:?} was not found", reference.name)))?;
    let Some(GitOpsResource::GitRepository(repository)) = &discovered.resource else {
        unreachable!("inventory kind key and resource variant must agree");
    };
    Ok((
        InlineGitRepository {
            repo_url: repository.spec.repo_url.clone(),
            publish_url: repository.spec.publish_url.clone(),
        },
        Some(discovered.source_path.clone()),
    ))
}

fn collect_checkout_yaml(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| NylError::config(format!("Failed to scan remote source: {error}")))?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| matches!(value, "yaml" | "yml"))
        {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn source_matches(root: &Path, path: &Path, source: &ApplicationGroupSource) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if !source.recursive && relative.parent().is_some_and(|parent| !parent.as_os_str().is_empty()) {
        return false;
    }
    let options = MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: true,
    };
    let included = source
        .include
        .iter()
        .any(|pattern| Pattern::new(pattern).is_ok_and(|pattern| pattern.matches_path_with(relative, options)));
    included
        && !source
            .exclude
            .iter()
            .any(|pattern| Pattern::new(pattern).is_ok_and(|pattern| pattern.matches_path_with(relative, options)))
}

fn render_application_name(
    session: &RenderSession,
    group: &ApplicationGroup,
    release: &crate::resources::Release,
) -> Result<String> {
    let Some(template) = &group.spec.application_name_template else {
        return Ok(release.metadata.name.clone());
    };
    let mut context = session
        .template_context()
        .to_json()
        .as_object()
        .cloned()
        .expect("template context is an object");
    context.insert("release".to_string(), serde_json::to_value(release)?);
    TemplateEngine::new().render(template, &Value::Object(context))
}

fn apply_release_application_override(
    application: &mut Value,
    release: &crate::resources::Release,
    group: &ApplicationGroup,
) -> Result<()> {
    let Some(mut override_value) = release
        .spec
        .argocd
        .as_ref()
        .and_then(|argocd| argocd.application_override.clone())
    else {
        return Ok(());
    };
    let sync_options = take_release_sync_option_additions(&mut override_value, release, group)?;
    let mut paths = Vec::new();
    if !override_value.is_empty() {
        collect_leaf_paths(&Value::Object(override_value.clone()), &mut Vec::new(), &mut paths);
    }
    for path in &paths {
        const IMMUTABLE_PATHS: &[&str] = &[
            "apiVersion",
            "kind",
            "metadata.name",
            "metadata.namespace",
            "metadata.finalizers.**",
            "spec.project",
            "spec.source.**",
            "spec.sources.**",
            "spec.destination.**",
            "spec.syncPolicy.**",
        ];
        if !is_supported_application_field_path(path) {
            return Err(NylError::config(format!(
                "Release {:?} attempts to customize unsupported Argo CD Application field {path:?}",
                release.metadata.name
            )));
        }
        if IMMUTABLE_PATHS
            .iter()
            .any(|pattern| path_matches_glob(path, pattern).unwrap_or(false))
        {
            return Err(NylError::config(format!(
                "Release {:?} cannot customize platform-owned Argo CD Application field {path:?}",
                release.metadata.name
            )));
        }
        if group
            .spec
            .release_customization
            .denied_paths
            .iter()
            .any(|pattern| path_matches_glob(path, pattern).unwrap_or(false))
            || !group
                .spec
                .release_customization
                .allowed_paths
                .iter()
                .any(|pattern| path_matches_glob(path, pattern).unwrap_or(false))
        {
            return Err(NylError::config(format!(
                "Release {:?} is not allowed to customize Argo CD Application field {path:?}",
                release.metadata.name
            )));
        }
    }
    *application = crate::util::deep_merge_value(Some(application.clone()), Value::Object(override_value));
    merge_release_sync_options(application, sync_options)?;
    Ok(())
}

fn take_release_sync_option_additions(
    application_override: &mut serde_json::Map<String, Value>,
    release: &crate::resources::Release,
    group: &ApplicationGroup,
) -> Result<Vec<String>> {
    let Some(spec) = application_override.get_mut("spec").and_then(Value::as_object_mut) else {
        return Ok(Vec::new());
    };
    let Some(sync_policy) = spec.get_mut("syncPolicy").and_then(Value::as_object_mut) else {
        return Ok(Vec::new());
    };
    let Some(value) = sync_policy.remove("+syncOptions") else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(|| {
        NylError::config(format!(
            "Release {:?} spec.argocd.applicationOverride.spec.syncPolicy.+syncOptions must be an array of strings",
            release.metadata.name
        ))
    })?;
    let mut additions = Vec::new();
    for value in values {
        let option = value.as_str().ok_or_else(|| {
            NylError::config(format!(
                "Release {:?} spec.argocd.applicationOverride.spec.syncPolicy.+syncOptions must be an array of strings",
                release.metadata.name
            ))
        })?;
        if !group
            .spec
            .release_customization
            .allowed_sync_options
            .iter()
            .any(|allowed| allowed == option)
        {
            return Err(NylError::config(format!(
                "Release {:?} is not allowed to add Argo CD sync option {option:?}",
                release.metadata.name
            )));
        }
        if !additions.iter().any(|existing| existing == option) {
            additions.push(option.to_owned());
        }
    }
    if sync_policy.is_empty() {
        spec.remove("syncPolicy");
    }
    if spec.is_empty() {
        application_override.remove("spec");
    }
    Ok(additions)
}

fn merge_release_sync_options(application: &mut Value, additions: Vec<String>) -> Result<()> {
    if additions.is_empty() {
        return Ok(());
    }
    let spec = application
        .get_mut("spec")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| NylError::config("Generated Argo CD Application is missing spec"))?;
    let sync_policy = spec
        .entry("syncPolicy")
        .or_insert_with(|| Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| NylError::config("Generated Argo CD Application spec.syncPolicy is not an object"))?;
    let sync_options = sync_policy
        .entry("syncOptions")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| NylError::config("Generated Argo CD Application spec.syncPolicy.syncOptions is not an array"))?;
    let mut merged = sync_options
        .iter()
        .map(|option| {
            option
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| NylError::config("Generated Argo CD Application sync option is not a string"))
        })
        .collect::<Result<Vec<_>>>()?;
    merge_sync_options(&mut merged, additions);
    *sync_options = merged.into_iter().map(Value::String).collect();
    Ok(())
}

fn collect_leaf_paths(value: &Value, segments: &mut Vec<String>, output: &mut Vec<String>) {
    match value {
        Value::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                segments.push(key.clone());
                collect_leaf_paths(value, segments, output);
                segments.pop();
            }
        }
        Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                segments.push(index.to_string());
                collect_leaf_paths(value, segments, output);
                segments.pop();
            }
        }
        _ => output.push(crate::resources::join_field_path_segments(segments)),
    }
}

fn insert_yaml(files: &mut BTreeMap<PathBuf, Vec<u8>>, path: PathBuf, value: &Value) -> Result<()> {
    let mut yaml = crate::yaml::serialize_yaml_document(value)
        .map_err(|error| NylError::config(format!("Failed to serialize {}: {error}", path.display())))?
        .into_bytes();
    if !yaml.ends_with(b"\n") {
        yaml.push(b'\n');
    }
    insert_file(files, path, yaml)
}

fn insert_file(files: &mut BTreeMap<PathBuf, Vec<u8>>, path: PathBuf, bytes: Vec<u8>) -> Result<()> {
    crate::resources::relative_path_to_posix("rendered path", &path)?;
    if files.insert(path.clone(), bytes).is_some() {
        return Err(NylError::config(format!(
            "More than one rendered resource owns {}",
            path.display()
        )));
    }
    Ok(())
}

fn join_posix(prefix: &str, relative: &Path) -> Result<String> {
    let relative = relative
        .to_str()
        .ok_or_else(|| NylError::config("Rendered path is not valid UTF-8"))?
        .replace('\\', "/");
    if prefix.is_empty() {
        Ok(relative)
    } else {
        Ok(format!("{prefix}/{relative}"))
    }
}

fn validate_path_segment(field: &str, value: &str) -> Result<()> {
    if value.is_empty() || matches!(value, "." | "..") || value.contains(['/', '\\', '\0']) {
        Err(NylError::config(format!(
            "{field} {value:?} is not a safe path segment"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn included_manifests_are_not_reported_as_unclaimed() {
        let temporary = tempfile::TempDir::new().unwrap();
        let entry = temporary.path().join("release.yaml");
        let included = temporary.path().join("deployment.yaml");
        let ignored = temporary.path().join("notes.yaml");
        for path in [&entry, &included, &ignored] {
            std::fs::write(path, "---\n").unwrap();
        }
        let source = ResolvedGroupSource {
            root: temporary.path().to_path_buf(),
            candidate_files: vec![entry.clone(), included.clone(), ignored.clone()],
            files: vec![StaticReleaseFile {
                path: entry.clone(),
                name: Some("test".to_string()),
            }],
            renderer_mode: RendererConfigMode::Central,
            source_session: None,
            remote: false,
            provenance_inputs: Vec::new(),
        };
        let claimed = BTreeSet::from([manifest_path_identity(&entry), manifest_path_identity(&included)]);

        assert_eq!(unclaimed_group_manifests(&source, &claimed), vec![ignored.as_path()]);
    }

    #[cfg(unix)]
    #[test]
    fn remote_checkout_subpaths_cannot_traverse_symlinks() {
        use std::os::unix::fs::symlink;

        let checkout = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), checkout.path().join("linked")).unwrap();

        let error = checked_checkout_subpath(checkout.path(), "linked", "source.path").unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
    }
}
