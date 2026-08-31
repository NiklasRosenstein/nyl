//! Compilation of one effective GitOps target into a rendered file tree.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use glob::{MatchOptions, Pattern};
use serde_json::Value;
use walkdir::WalkDir;

use crate::git::GitManager;
use crate::resources::{
    is_supported_application_field_path, path_matches_glob, AppProjectDefinition, AppProjectManagement,
    ApplicationGroup, ApplicationGroupSource, Cluster, ClusterDestination, GitOpsResource, GitOpsResourceKind,
    GitOpsTarget, GitPublication, InlineGitRepository, RendererConfig, RendererConfigMode,
};
use crate::template::TemplateEngine;
use crate::util::SourceContext;
use crate::{NylError, Result};

use super::{
    build_directory_application, render_manifest_layout, take_managed_namespace, DirectoryApplicationInput,
    GitOpsInventory, RenderSession,
};

/// Pure output of compiling one target. Paths are relative to the target prefix.
#[derive(Debug)]
pub struct CompiledTargetTree {
    pub target: GitOpsTarget,
    pub cluster: Cluster,
    pub repository_name: Option<String>,
    pub repository: InlineGitRepository,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    pub inputs: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
struct ManagedNamespaceOwner {
    manifest: Value,
    application_namespace: String,
    project: String,
    destination: ClusterDestination,
    sync_policy: Option<crate::resources::GitOpsSyncPolicy>,
    deletion_policy: crate::resources::ApplicationDeletionPolicy,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
}

/// Validate cross-resource references and target-prefix ownership without rendering releases.
pub fn validate_gitops_inventory(inventory: &GitOpsInventory) -> Result<()> {
    let mut publications = Vec::new();
    for discovered in inventory.resources.values() {
        match discovered.resource.as_ref() {
            Some(GitOpsResource::GitOpsTarget(target)) => {
                resolve_cluster(inventory, &target.spec.cluster_ref.name)?;
                let (_, repository, _) = resolve_git_publication(inventory, &target.spec.publication)?;
                for project in &target.spec.projects {
                    resolve_project(inventory, project)?;
                }
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
            Some(GitOpsResource::ApplicationGroup(group)) => {
                resolve_project(inventory, &group.spec.project_ref)?;
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
    let (repository_name, repository, repository_path) = resolve_git_publication(inventory, &target.spec.publication)?;
    let central_session = RenderSession::for_target(&inventory.project_root, &target, &cluster)?;
    let mut files = BTreeMap::new();
    let mut inputs = BTreeSet::from([target_discovered.source_path.clone(), cluster_path]);
    if let Some(repository_path) = repository_path {
        inputs.insert(repository_path);
    }
    let mut emitted_projects = BTreeSet::new();
    let mut git_manager = None;
    let mut namespace_owners = BTreeMap::<(String, String), ManagedNamespaceOwner>::new();
    let mut workload_owners = HashMap::new();

    let mut groups = Vec::new();
    for discovered in inventory.resources.values() {
        if discovered.identity.kind != GitOpsResourceKind::ApplicationGroup {
            continue;
        }
        let Some(GitOpsResource::ApplicationGroup(group)) = render_effective_control(discovered, &central_session)?
        else {
            continue;
        };
        if group_applies(&group, &target) {
            groups.push((discovered.source_path.clone(), *group));
        }
    }
    groups.sort_by(|left, right| left.1.metadata.name.cmp(&right.1.metadata.name));

    for (group_resource_path, group) in groups {
        inputs.insert(group_resource_path.clone());
        let (project_id, project_path, project) =
            resolve_effective_project(inventory, &group.spec.project_ref, &central_session)?;
        inputs.insert(project_path);
        let argocd_project_name = app_project_name(&project)?;
        if project.spec.management == AppProjectManagement::Rendered && emitted_projects.insert(project_id.clone()) {
            insert_yaml(
                &mut files,
                PathBuf::from("_nyl/catalog/projects").join(format!("{project_id}.yaml")),
                &project.spec.manifest,
            )?;
        }

        let source = resolve_group_source(inventory, &group_resource_path, &group, &target, &mut git_manager)?;
        inputs.extend(source.provenance_inputs.iter().cloned());
        let session = match source.renderer_mode {
            RendererConfigMode::Central if !source.remote => &central_session,
            _ => source
                .source_session
                .as_ref()
                .expect("remote source must carry a restricted rendering session"),
        };

        for source_file in &source.files {
            let input_path = if source.remote {
                PathBuf::from("@remote").join(source_file.strip_prefix(&source.root).unwrap_or(source_file))
            } else {
                source_file
                    .strip_prefix(&inventory.project_root)
                    .unwrap_or(source_file)
                    .to_path_buf()
            };
            inputs.insert(input_path);
            let mut rendered = session.render_release_file(source_file).await?;
            let Some(release) = rendered.release.take() else {
                continue;
            };
            let destination_namespace = group
                .spec
                .destination_namespace
                .clone()
                .unwrap_or_else(|| release.metadata.namespace.clone());
            if let Some(manifest) =
                take_managed_namespace(&mut rendered.manifests, &destination_namespace, &group.spec.namespace)?
            {
                let key = (cluster.metadata.name.clone(), destination_namespace.clone());
                let owner = ManagedNamespaceOwner {
                    manifest,
                    application_namespace: group.spec.application_namespace.clone(),
                    project: argocd_project_name.clone(),
                    destination: cluster.spec.destination.clone(),
                    sync_policy: group.spec.sync_policy.clone(),
                    deletion_policy: group.spec.application_deletion_policy,
                    labels: group.spec.labels.clone(),
                    annotations: group.spec.annotations.clone(),
                };
                if let Some(existing) = namespace_owners.get(&key) {
                    if existing != &owner {
                        return Err(NylError::config(format!(
                            "Namespace {:?} has conflicting ownership policy across ApplicationGroups",
                            destination_namespace
                        )));
                    }
                } else {
                    namespace_owners.insert(key, owner);
                }
            }
            if let Some(namespace) = rendered.manifests.iter().find(|manifest| {
                manifest.get("apiVersion").and_then(Value::as_str) == Some("v1")
                    && manifest.get("kind").and_then(Value::as_str) == Some("Namespace")
            }) {
                let name = namespace
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                return Err(NylError::config(format!(
                    "NylRelease {:?} renders Namespace {name:?} outside its destination namespace {destination_namespace:?}; Namespace ownership must be assigned through ApplicationGroup destinationNamespace and namespace policy",
                    release.metadata.name
                )));
            }

            let group_output = group.spec.output_path.as_deref().unwrap_or(&group.metadata.name);
            crate::resources::validate_relative_path("ApplicationGroup outputPath", group_output, false, false)?;
            validate_path_segment("NylRelease metadata.name", &release.metadata.name)?;
            if release.metadata.name == "_namespaces" {
                return Err(NylError::config(
                    "NylRelease metadata.name \"_namespaces\" is reserved by the rendered GitOps layout",
                ));
            }
            let release_directory = PathBuf::from(group_output).join(&release.metadata.name);
            for manifest in &rendered.manifests {
                let key = crate::kubernetes::ResourceKey::from_json_value(manifest)?;
                if let Some(previous) = workload_owners.insert(
                    (cluster.metadata.name.clone(), key.clone()),
                    application_name_hint(&group, &release),
                ) {
                    return Err(NylError::config(format!(
                        "Rendered resource {key} is owned by more than one workload Application ({previous:?} and {:?})",
                        release.metadata.name
                    )));
                }
            }
            for (relative, bytes) in render_manifest_layout(&rendered.manifests)? {
                insert_file(&mut files, release_directory.join(relative), bytes)?;
            }

            let application_name = render_application_name(session, &group, &release)?;
            validate_path_segment("rendered Application name", &application_name)?;
            let rendered_path = join_posix(target.spec.publication.path_prefix.as_str(), &release_directory)?;
            let mut application = build_directory_application(&DirectoryApplicationInput {
                name: application_name.clone(),
                application_namespace: group.spec.application_namespace.clone(),
                project: argocd_project_name.clone(),
                repo_url: repository.repo_url.clone(),
                revision: target.spec.publication.revision.clone(),
                rendered_path,
                destination: cluster.spec.destination.clone(),
                destination_namespace,
                sync_policy: group.spec.sync_policy.clone(),
                deletion_policy: group.spec.application_deletion_policy,
                labels: group.spec.labels.clone(),
                annotations: group.spec.annotations.clone(),
            })?;
            apply_release_application_override(&mut application, &release, &group)?;
            insert_yaml(
                &mut files,
                PathBuf::from("_nyl/catalog/applications")
                    .join(&group.spec.application_namespace)
                    .join(format!("{application_name}.yaml")),
                &application,
            )?;
        }
    }

    for ((cluster, namespace), owner) in namespace_owners {
        let digest = crate::gitops::reconcile::sha256(format!("{cluster}\0{namespace}").as_bytes());
        let suffix = &digest[..20];
        let application_name = format!("nyl-namespace-{suffix}");
        let namespace_directory = PathBuf::from("_nyl/namespaces").join(suffix);
        for (relative, bytes) in render_manifest_layout(&[owner.manifest])? {
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

    Ok(CompiledTargetTree {
        target,
        cluster,
        repository_name,
        repository,
        files,
        inputs,
    })
}

fn application_name_hint(group: &ApplicationGroup, release: &crate::resources::NylRelease) -> String {
    format!("{}/{}", group.metadata.name, release.metadata.name)
}

fn group_applies(group: &ApplicationGroup, target: &GitOpsTarget) -> bool {
    group.spec.enabled
        && (target.spec.projects.is_empty() || target.spec.projects.contains(&group.spec.project_ref))
        && group.spec.target_selector.as_ref().is_none_or(|selector| {
            selector
                .match_labels
                .iter()
                .all(|(key, value)| target.metadata.labels.get(key) == Some(value))
        })
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
    files: Vec<PathBuf>,
    renderer_mode: RendererConfigMode,
    source_session: Option<RenderSession>,
    remote: bool,
    provenance_inputs: Vec<PathBuf>,
}

fn resolve_group_source(
    inventory: &GitOpsInventory,
    group_resource_path: &Path,
    group: &ApplicationGroup,
    target: &GitOpsTarget,
    git_manager: &mut Option<GitManager>,
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
                None => git_manager.insert(GitManager::new().map_err(NylError::Git)?),
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

    let source_session = match (&remote_root, source.renderer_config.mode) {
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
                target,
                &cluster,
            )?)
        }
        (None, _) => None,
    };

    Ok(ResolvedGroupSource {
        root,
        files,
        renderer_mode: source.renderer_config.mode,
        source_session,
        remote: remote_root.is_some(),
        provenance_inputs,
    })
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
    release: &crate::resources::NylRelease,
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
    release: &crate::resources::NylRelease,
    group: &ApplicationGroup,
) -> Result<()> {
    let Some(override_value) = release
        .spec
        .argocd
        .as_ref()
        .and_then(|argocd| argocd.application_override.as_ref())
    else {
        return Ok(());
    };
    let mut paths = Vec::new();
    collect_leaf_paths(&Value::Object(override_value.clone()), &mut Vec::new(), &mut paths);
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
                "NylRelease {:?} attempts to customize unsupported Argo CD Application field {path:?}",
                release.metadata.name
            )));
        }
        if IMMUTABLE_PATHS
            .iter()
            .any(|pattern| path_matches_glob(path, pattern).unwrap_or(false))
        {
            return Err(NylError::config(format!(
                "NylRelease {:?} cannot customize platform-owned Argo CD Application field {path:?}",
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
                "NylRelease {:?} is not allowed to customize Argo CD Application field {path:?}",
                release.metadata.name
            )));
        }
    }
    *application = crate::util::deep_merge_value(Some(application.clone()), Value::Object(override_value.clone()));
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
    let text = path
        .to_str()
        .ok_or_else(|| NylError::config(format!("Rendered path is not valid UTF-8: {}", path.display())))?;
    crate::resources::validate_relative_path("rendered path", text, false, false)?;
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
