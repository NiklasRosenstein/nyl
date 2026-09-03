//! Discovery of Kubernetes-shaped GitOps compiler resources.
//!
//! Discovery follows Git visibility: tracked files and non-ignored untracked
//! files are eligible, while ignored files, deleted worktree entries,
//! symlinks, and submodules are not. Only documents with a literal top-level
//! GitOps API envelope are parsed, so unrelated manifests may contain
//! MiniJinja syntax that is not valid YAML before rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use git2::{Repository, Status, StatusOptions};

use crate::config::ProjectConfig;
use crate::constants::API_VERSION_GITOPS;
use crate::resources::{
    parse_gitops_resource, parse_gitops_resource_identity, GitOpsResource, GitOpsResourceIdentity, GitOpsResourceKind,
};
use crate::util::SourceContext;
use crate::{NylError, Result};

/// A stable key for a discovered compiler resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitOpsInventoryKey {
    pub kind: String,
    pub name: String,
}

impl GitOpsInventoryKey {
    pub fn new(kind: GitOpsResourceKind, name: impl Into<String>) -> Self {
        Self {
            kind: kind.as_str().to_owned(),
            name: name.into(),
        }
    }
}

/// A strictly parsed GitOps resource and its location in the source project.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredGitOpsResource {
    /// Path relative to the directory containing `nyl.toml`.
    pub source_path: PathBuf,
    /// One-based document index within the source file.
    pub document_index: usize,
    /// Original document text, rendered again with the effective target before compilation.
    pub raw_document: String,
    pub identity: GitOpsResourceIdentity,
    /// Literal labels from the static resource envelope. Selection never
    /// depends on target rendering.
    pub static_labels: BTreeMap<String, String>,
    /// Present when the source document is already a complete static resource.
    /// Templatable ApplicationGroup and AppProjectDefinition specs are parsed
    /// only after a target context is selected.
    pub resource: Option<GitOpsResource>,
}

/// Deterministic inventory of Git-visible YAML and GitOps resources.
#[derive(Debug, Clone, PartialEq)]
pub struct GitOpsInventory {
    pub project_root: PathBuf,
    /// Parsed project configuration shared by discovery and central render sessions.
    pub project_config: ProjectConfig,
    /// All eligible YAML files, relative to `project_root` and sorted.
    pub yaml_files: Vec<PathBuf>,
    /// Compiler resources keyed by their static kind and local name.
    pub resources: BTreeMap<GitOpsInventoryKey, DiscoveredGitOpsResource>,
}

impl GitOpsInventory {
    pub fn get(&self, kind: GitOpsResourceKind, name: &str) -> Option<&DiscoveredGitOpsResource> {
        self.resources.get(&GitOpsInventoryKey::new(kind, name))
    }

    /// Repeat discovery with a different output exclusion without re-reading
    /// the project configuration.
    pub fn rediscover(&self, output_subtree: Option<&Path>) -> Result<Self> {
        discover_project_inventory(self.project_root.clone(), self.project_config.clone(), output_subtree)
    }
}

/// Resolve an explicitly selected DeploymentTarget, or infer the sole target.
pub fn resolve_deployment_target_name(inventory: &GitOpsInventory, requested: Option<&str>) -> Result<String> {
    if let Some(name) = requested {
        let discovered = inventory
            .get(GitOpsResourceKind::DeploymentTarget, name)
            .ok_or_else(|| NylError::config(format!("DeploymentTarget {name:?} was not found")))?;
        if discovered.resource.is_none() {
            return Err(NylError::config(format!("DeploymentTarget {name:?} must be static")));
        }
        return Ok(name.to_owned());
    }

    let names = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == GitOpsResourceKind::DeploymentTarget)
        .map(|resource| resource.identity.name.as_str())
        .collect::<Vec<_>>();
    match names.as_slice() {
        [] => Err(NylError::config(
            "This operation requires a DeploymentTarget, but none are configured",
        )),
        [name] => Ok((*name).to_owned()),
        _ => Err(NylError::config(format!(
            "--target is required because multiple DeploymentTargets are configured: {}",
            names.join(", ")
        ))),
    }
}

/// Locate the nearest `nyl.toml` and discover Git-visible GitOps resources.
///
/// A relative `output_subtree` is resolved beneath the project root. The
/// subtree is omitted from both the YAML file list and resource inventory.
pub fn discover_gitops_inventory(start_dir: &Path, output_subtree: Option<&Path>) -> Result<GitOpsInventory> {
    let start_dir = if start_dir.is_file() {
        start_dir.parent().unwrap_or(start_dir)
    } else {
        start_dir
    };
    let config_file =
        ProjectConfig::find(Some(start_dir))?.ok_or_else(|| NylError::ConfigNotFound("nyl.toml".to_owned()))?;
    let project_config = ProjectConfig::load(Some(config_file.clone()))?;
    let project_root = config_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|error| {
            NylError::config(format!(
                "Failed to resolve project root for {}: {error}",
                config_file.display()
            ))
        })?;

    discover_project_inventory(project_root, project_config, output_subtree)
}

fn discover_project_inventory(
    project_root: PathBuf,
    project_config: ProjectConfig,
    output_subtree: Option<&Path>,
) -> Result<GitOpsInventory> {
    let output_subtree = output_subtree.map(|path| {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            project_root.join(path)
        };
        normalize_absolute_path(&path)
    });

    let repository = Repository::discover(&project_root).map_err(|error| {
        NylError::config(format!(
            "GitOps discovery requires a Git worktree containing {}: {error}",
            project_root.display()
        ))
    })?;
    let repository_root = repository
        .workdir()
        .ok_or_else(|| NylError::config("GitOps discovery requires a non-bare Git worktree"))?
        .canonicalize()?;
    if !project_root.starts_with(&repository_root) {
        return Err(NylError::config(format!(
            "Project root {} is outside Git worktree {}",
            project_root.display(),
            repository_root.display()
        )));
    }

    let vendor_subtree = project_config.vendor().map(|settings| {
        let path = project_config
            .file
            .as_deref()
            .and_then(Path::parent)
            .and_then(|config_root| settings.path.strip_prefix(config_root).ok())
            .map_or_else(|| settings.path.clone(), |relative| project_root.join(relative));
        normalize_absolute_path(&path)
    });
    let yaml_files = collect_git_visible_yaml(
        &repository,
        &repository_root,
        &project_root,
        output_subtree.as_deref(),
        vendor_subtree.as_deref(),
    )?;
    let mut resources = BTreeMap::new();

    for relative_path in &yaml_files {
        let absolute_path = project_root.join(relative_path);
        discover_file_resources(&absolute_path, relative_path, &mut resources)?;
    }

    Ok(GitOpsInventory {
        project_root,
        project_config,
        yaml_files,
        resources,
    })
}

fn collect_git_visible_yaml(
    repository: &Repository,
    repository_root: &Path,
    project_root: &Path,
    output_subtree: Option<&Path>,
    vendor_subtree: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut repository_paths = BTreeSet::new();
    let index = repository.index().map_err(crate::git::GitError::from)?;
    for entry in index.iter() {
        // 0160000 is a Git link (submodule), 0120000 is a symbolic link.
        if entry.mode == 0o160_000 || entry.mode == 0o120_000 {
            continue;
        }
        let Ok(path) = std::str::from_utf8(&entry.path) else {
            continue;
        };
        repository_paths.insert(PathBuf::from(path));
    }

    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(crate::git::GitError::from)?;
    for entry in statuses.iter() {
        if !entry.status().contains(Status::WT_NEW) {
            continue;
        }
        if let Ok(path) = entry.path() {
            repository_paths.insert(PathBuf::from(path));
        }
    }

    let mut result = BTreeSet::new();
    for repository_relative in repository_paths {
        if has_dot_git_component(&repository_relative) || !is_yaml_path(&repository_relative) {
            continue;
        }
        let absolute_path = repository_root.join(&repository_relative);
        if !absolute_path.starts_with(project_root)
            || output_subtree.is_some_and(|output| absolute_path.starts_with(output))
            || vendor_subtree.is_some_and(|vendor| absolute_path.starts_with(vendor))
        {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&absolute_path) else {
            // A tracked file deleted from the worktree is not visible.
            continue;
        };
        if !metadata.file_type().is_file() || contains_symlink_component(project_root, &absolute_path)? {
            continue;
        }
        let relative = absolute_path.strip_prefix(project_root).map_err(|error| {
            NylError::config(format!(
                "Failed to make {} relative to {}: {error}",
                absolute_path.display(),
                project_root.display()
            ))
        })?;
        result.insert(relative.to_path_buf());
    }
    Ok(result.into_iter().collect())
}

fn discover_file_resources(
    absolute_path: &Path,
    relative_path: &Path,
    resources: &mut BTreeMap<GitOpsInventoryKey, DiscoveredGitOpsResource>,
) -> Result<()> {
    let bytes = std::fs::read(absolute_path)?;
    let contents = String::from_utf8_lossy(&bytes);

    for (document_index, document) in split_yaml_documents(&contents).into_iter().enumerate() {
        if !advertises_gitops_api(document) {
            continue;
        }
        if advertises_release_kind(document) {
            continue;
        }

        let contextual_path = PathBuf::from(format!("{}#document-{}", relative_path.display(), document_index + 1));
        let identity = scan_static_identity(document)?.ok_or_else(|| {
            NylError::config(format!(
                "Document {} in {} advertises {API_VERSION_GITOPS} but has no valid static envelope",
                document_index + 1,
                relative_path.display()
            ))
        })?;
        let scanned_labels = scan_static_metadata_labels(document)?;
        let templatable = matches!(
            identity.kind,
            GitOpsResourceKind::ApplicationGroup | GitOpsResourceKind::AppProjectDefinition
        );
        let has_template = document.contains("{{") || document.contains("{%") || document.contains("{#");
        let resource = match parse_complete_resource(document, &contextual_path) {
            Ok(resource) => Some(resource),
            Err(_) if templatable && has_template => None,
            Err(error) => return Err(error),
        };
        let static_labels = resource.as_ref().map_or(scanned_labels, resource_metadata_labels);
        let key = GitOpsInventoryKey::new(identity.kind, identity.name.clone());
        let discovered = DiscoveredGitOpsResource {
            source_path: relative_path.to_path_buf(),
            document_index: document_index + 1,
            raw_document: document.to_string(),
            identity,
            static_labels,
            resource,
        };
        if let Some(previous) = resources.insert(key.clone(), discovered) {
            return Err(NylError::config(format!(
                "Duplicate GitOps resource {}/{} in {} document {} and {} document {}",
                key.kind,
                key.name,
                previous.source_path.display(),
                previous.document_index,
                relative_path.display(),
                document_index + 1
            )));
        }
    }
    Ok(())
}

fn resource_metadata_labels(resource: &GitOpsResource) -> BTreeMap<String, String> {
    match resource {
        GitOpsResource::GitRepository(resource) => resource.metadata.labels.clone(),
        GitOpsResource::Cluster(resource) => resource.metadata.labels.clone(),
        GitOpsResource::ArgoCDInstance(resource) => resource.metadata.labels.clone(),
        GitOpsResource::DeploymentTarget(resource) => resource.metadata.labels.clone(),
        GitOpsResource::AppProjectDefinition(resource) => resource.metadata.labels.clone(),
        GitOpsResource::ApplicationGroup(resource) => resource.metadata.labels.clone(),
    }
}

fn scan_static_metadata_labels(document: &str) -> Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    let mut metadata_indent = None;
    let mut labels_indent = None;
    for line in document.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            metadata_indent = trimmed
                .strip_prefix("metadata:")
                .is_some_and(|tail| tail.trim().is_empty())
                .then_some(indent);
            labels_indent = None;
            continue;
        }
        if metadata_indent.is_some_and(|parent| indent > parent) {
            if labels_indent.is_none()
                && trimmed
                    .strip_prefix("labels:")
                    .is_some_and(|tail| tail.trim().is_empty())
            {
                labels_indent = Some(indent);
                continue;
            }
            if labels_indent.is_some_and(|parent| indent > parent) {
                if let Some((key, value)) = trimmed.split_once(':') {
                    let key = parse_static_scalar(key);
                    let value = parse_static_scalar(value);
                    if key.contains("{{")
                        || value.contains("{{")
                        || key.contains("{%")
                        || value.contains("{%")
                        || key.contains("{#")
                        || value.contains("{#")
                    {
                        return Err(NylError::config(
                            "GitOps resource metadata.labels must be static because targets select them before rendering",
                        ));
                    }
                    if !key.is_empty() && !value.is_empty() {
                        labels.insert(key.to_owned(), value.to_owned());
                    }
                }
            } else if labels_indent.is_some() {
                labels_indent = None;
            }
        }
    }
    Ok(labels)
}

fn parse_complete_resource(document: &str, contextual_path: &Path) -> Result<GitOpsResource> {
    let source_context = SourceContext::new(contextual_path.to_path_buf());
    let parsed_documents = source_context.parse_yaml_documents(document)?;
    if parsed_documents.len() != 1 {
        return Err(NylError::config(format!(
            "GitOps compiler resource in {} must contain exactly one YAML document",
            contextual_path.display()
        )));
    }
    parse_gitops_resource(&parsed_documents[0])?.ok_or_else(|| {
        NylError::config(format!(
            "Document {} advertises {API_VERSION_GITOPS} but is not a GitOps resource",
            contextual_path.display()
        ))
    })
}

fn scan_static_identity(document: &str) -> Result<Option<GitOpsResourceIdentity>> {
    if let Ok(documents) = crate::yaml::parse_yaml_documents_k8s_compatible(document) {
        if documents.len() == 1 {
            return parse_gitops_resource_identity(&documents[0]);
        }
    }
    let mut api_version = None;
    let mut kind = None;
    let mut name = None;
    let mut metadata_indent = None;
    let mut metadata_child_indent = None;
    for line in document.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("{%") || trimmed.starts_with("{#") {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            metadata_indent = None;
            metadata_child_indent = None;
            if let Some(value) = static_mapping_value(trimmed, "apiVersion") {
                api_version = Some(value.to_owned());
            } else if let Some(value) = static_mapping_value(trimmed, "kind") {
                kind = Some(value.to_owned());
            } else if trimmed
                .strip_prefix("metadata:")
                .is_some_and(|tail| tail.trim().is_empty())
            {
                metadata_indent = Some(indent);
            }
        } else if metadata_indent.is_some_and(|parent| indent > parent) {
            let child_indent = *metadata_child_indent.get_or_insert(indent);
            if indent == child_indent {
                if let Some(value) = static_mapping_value(trimmed, "name") {
                    name = Some(value.to_owned());
                }
            }
        }
    }
    if api_version.as_deref() != Some(API_VERSION_GITOPS) {
        return Ok(None);
    }
    let kind_text = kind.ok_or_else(|| NylError::config("GitOps resource kind must be a static string"))?;
    let kind = GitOpsResourceKind::parse(&kind_text)
        .ok_or_else(|| NylError::config(format!("Unsupported {API_VERSION_GITOPS} kind {kind_text:?}")))?;
    let name = name.ok_or_else(|| NylError::config(format!("{kind_text} metadata.name must be a static string")))?;
    parse_gitops_resource_identity(&serde_json::json!({
        "apiVersion": API_VERSION_GITOPS,
        "kind": kind.as_str(),
        "metadata": {"name": name},
    }))
}

fn static_mapping_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (actual_key, value) = line.split_once(':')?;
    if actual_key.trim() != key {
        return None;
    }
    let value = parse_static_scalar(value);
    (!value.is_empty() && !value.contains("{{") && !value.contains("{%") && !value.contains("{#")).then_some(value)
}

fn split_yaml_documents(contents: &str) -> Vec<&str> {
    let mut documents = Vec::new();
    let mut start = 0;
    let mut offset = 0;

    for line in contents.split_inclusive('\n') {
        if is_yaml_document_marker(line, "---") || is_yaml_document_marker(line, "...") {
            if offset > start {
                documents.push(&contents[start..offset]);
            }
            start = offset + line.len();
        }
        offset += line.len();
    }
    if start < contents.len() {
        documents.push(&contents[start..]);
    }
    documents
}

fn is_yaml_document_marker(line: &str, marker: &str) -> bool {
    if line.starts_with([' ', '\t']) {
        return false;
    }
    let line = line.trim_end_matches(['\r', '\n']).trim_end();
    line == marker
        || line
            .strip_prefix(marker)
            .is_some_and(|tail| tail.starts_with([' ', '#']))
}

fn advertises_gitops_api(document: &str) -> bool {
    document.lines().any(|line| {
        if line.starts_with([' ', '\t']) {
            return false;
        }
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        key.trim() == "apiVersion" && parse_static_scalar(value) == API_VERSION_GITOPS
    })
}

fn advertises_release_kind(document: &str) -> bool {
    document.lines().any(|line| {
        if line.starts_with([' ', '\t']) {
            return false;
        }
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        key.trim() == "kind" && parse_static_scalar(value) == crate::resources::KIND_RELEASE
    })
}

fn parse_static_scalar(value: &str) -> &str {
    let value = strip_yaml_comment(value).trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn strip_yaml_comment(value: &str) -> &str {
    let mut quote = None;
    let mut previous_whitespace = true;
    for (index, character) in value.char_indices() {
        match character {
            '\'' | '"' if quote == Some(character) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(character),
            '#' if quote.is_none() && previous_whitespace => return &value[..index],
            _ => {}
        }
        previous_whitespace = character.is_whitespace();
    }
    value
}

fn contains_symlink_component(root: &Path, path: &Path) -> Result<bool> {
    let relative = path.strip_prefix(root).map_err(|error| {
        NylError::config(format!(
            "Failed to inspect path {} beneath {}: {error}",
            path.display(),
            root.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if std::fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_dot_git_component(path: &Path) -> bool {
    path.components().any(|component| component.as_os_str() == ".git")
}

fn is_yaml_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "yaml" | "yml"))
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::fs;

    use git2::Repository;
    use tempfile::TempDir;

    use super::*;
    use crate::resources::{GitOpsResource, GitOpsResourceKind};

    const TARGET: &str = r"apiVersion: gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
spec:
  clusterRef:
    name: kasoku
  publication:
    repository:
      repoURL: ssh://git@example/deploy.git
    revision: deploy/production
";

    fn project() -> (TempDir, Repository) {
        let temporary = TempDir::new().unwrap();
        let repository = Repository::init(temporary.path()).unwrap();
        fs::write(temporary.path().join("nyl.toml"), "[project]\n").unwrap();
        (temporary, repository)
    }

    fn track(repository: &Repository, path: &str) {
        let mut index = repository.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
    }

    #[test]
    fn discovers_tracked_and_nonignored_untracked_resources_deterministically() {
        let (temporary, repository) = project();
        fs::create_dir_all(temporary.path().join("config/targets")).unwrap();
        fs::write(temporary.path().join("config/targets/tracked.yaml"), TARGET).unwrap();
        track(&repository, "config/targets/tracked.yaml");

        let staging = TARGET.replace("production", "staging");
        fs::write(temporary.path().join("config/targets/untracked.yml"), staging).unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        assert_eq!(
            inventory.yaml_files,
            vec![
                PathBuf::from("config/targets/tracked.yaml"),
                PathBuf::from("config/targets/untracked.yml")
            ]
        );
        let production = inventory
            .get(GitOpsResourceKind::DeploymentTarget, "production")
            .unwrap();
        assert_eq!(production.source_path, Path::new("config/targets/tracked.yaml"));
        assert!(matches!(production.resource, Some(GitOpsResource::DeploymentTarget(_))));
        assert!(inventory.get(GitOpsResourceKind::DeploymentTarget, "staging").is_some());
    }

    #[test]
    fn ignores_deleted_ignored_output_and_unrelated_templated_yaml() {
        let (temporary, repository) = project();
        fs::create_dir_all(temporary.path().join("config/targets")).unwrap();
        fs::write(temporary.path().join("config/targets/deleted.yaml"), TARGET).unwrap();
        track(&repository, "config/targets/deleted.yaml");
        fs::remove_file(temporary.path().join("config/targets/deleted.yaml")).unwrap();

        fs::write(temporary.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(temporary.path().join("ignored")).unwrap();
        fs::write(temporary.path().join("ignored/target.yaml"), TARGET).unwrap();
        fs::create_dir_all(temporary.path().join("rendered")).unwrap();
        fs::write(temporary.path().join("rendered/target.yaml"), TARGET).unwrap();
        fs::write(
            temporary.path().join("applications.yaml"),
            "{% if values.enabled %}\napiVersion: v1\nkind: ConfigMap\n{% endif %}\n",
        )
        .unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), Some(Path::new("rendered"))).unwrap();
        assert_eq!(inventory.yaml_files, vec![PathBuf::from("applications.yaml")]);
        assert!(inventory.resources.is_empty());
    }

    #[test]
    fn excludes_the_configured_vendor_tree_from_yaml_discovery() {
        let (temporary, repository) = project();
        fs::write(
            temporary.path().join("nyl.toml"),
            "[vendor]\nmode = \"preferred\"\npath = \"third-party\"\n",
        )
        .unwrap();
        fs::create_dir_all(temporary.path().join("third-party/artifacts/manifests/example.com")).unwrap();
        fs::write(
            temporary
                .path()
                .join("third-party/artifacts/manifests/example.com/target.yaml"),
            TARGET,
        )
        .unwrap();
        track(&repository, "third-party/artifacts/manifests/example.com/target.yaml");

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();

        assert!(inventory.yaml_files.is_empty(), "{:?}", inventory.yaml_files);
        assert!(inventory.resources.is_empty());
    }

    #[test]
    fn defers_structurally_templated_control_resources_after_static_discovery() {
        let (temporary, _repository) = project();
        fs::write(
            temporary.path().join("group.yaml"),
            r"{% if values.enabled %}
apiVersion: gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: optional
  labels:
    name: nested-label-must-not-replace-metadata-name
spec:
  enabled: {{ values.enabled }}
  projectRef: workloads
  applicationNamespace: argocd
  destinationNamespace: workloads
{% endif %}
",
        )
        .unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        let group = inventory.get(GitOpsResourceKind::ApplicationGroup, "optional").unwrap();
        assert_eq!(group.identity.name, "optional");
        assert!(inventory
            .get(
                GitOpsResourceKind::ApplicationGroup,
                "nested-label-must-not-replace-metadata-name"
            )
            .is_none());
        assert!(group.resource.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinked_yaml() {
        use std::os::unix::fs::symlink;

        let (temporary, _repository) = project();
        fs::write(temporary.path().join("outside"), TARGET).unwrap();
        symlink(temporary.path().join("outside"), temporary.path().join("linked.yaml")).unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        assert!(inventory.yaml_files.is_empty());
    }

    #[test]
    fn does_not_descend_into_submodules() {
        let (temporary, repository) = project();
        fs::create_dir_all(temporary.path().join("vendor/module")).unwrap();
        fs::write(temporary.path().join("vendor/module/target.yaml"), TARGET).unwrap();

        let mut index = repository.index().unwrap();
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o160_000,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap(),
                flags: 0,
                flags_extended: 0,
                path: b"vendor/module".to_vec(),
            })
            .unwrap();
        index.write().unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        assert!(inventory.yaml_files.is_empty());
    }

    #[test]
    fn strictly_parses_advertised_resources() {
        let (temporary, _repository) = project();
        fs::write(
            temporary.path().join("invalid.yaml"),
            TARGET.replace(
                "  clusterRef:\n    name: kasoku",
                "  clusterRef:\n    name: kasoku\n  unknown: true",
            ),
        )
        .unwrap();

        let error = discover_gitops_inventory(temporary.path(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field"));
        assert!(error.contains("DeploymentTarget"));
    }

    #[test]
    fn discovers_cluster_as_a_strict_static_resource() {
        let (temporary, _repository) = project();
        fs::write(
            temporary.path().join("cluster.yaml"),
            r"apiVersion: gitops.nyl/v1
kind: Cluster
metadata:
  name: kasoku
spec:
  destination:
    server: https://kubernetes.default.svc
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions: [v1, apps/v1]
",
        )
        .unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        let cluster = inventory.get(GitOpsResourceKind::Cluster, "kasoku").unwrap();
        assert!(matches!(cluster.resource, Some(GitOpsResource::Cluster(_))));
    }

    #[test]
    fn rejects_structurally_templated_cluster() {
        let (temporary, _repository) = project();
        fs::write(
            temporary.path().join("cluster.yaml"),
            r"apiVersion: gitops.nyl/v1
kind: Cluster
metadata:
  name: kasoku
spec:
  destination:
    server: https://kubernetes.default.svc
  kubernetes:
    apiVersions:
{% for api_version in values.apiVersions %}
      - {{ api_version }}
{% endfor %}
",
        )
        .unwrap();

        assert!(discover_gitops_inventory(temporary.path(), None).is_err());
    }

    #[test]
    fn rejects_duplicate_kind_and_name_with_both_locations() {
        let (temporary, _repository) = project();
        fs::write(temporary.path().join("one.yaml"), TARGET).unwrap();
        fs::write(temporary.path().join("two.yaml"), TARGET).unwrap();

        let error = discover_gitops_inventory(temporary.path(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Duplicate GitOps resource DeploymentTarget/production"));
        assert!(error.contains("one.yaml"));
        assert!(error.contains("two.yaml"));
    }

    #[test]
    fn handles_multiple_documents_and_quoted_static_api_version() {
        let (temporary, _repository) = project();
        let content = format!(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: unrelated\n---\n{}",
            TARGET.replacen("apiVersion: gitops.nyl/v1", "apiVersion: \"gitops.nyl/v1\" # static", 1)
        );
        fs::write(temporary.path().join("resources.yaml"), content).unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        let resource = inventory
            .get(GitOpsResourceKind::DeploymentTarget, "production")
            .unwrap();
        assert_eq!(resource.document_index, 2);
    }

    #[test]
    fn locates_project_from_nested_directory() {
        let (temporary, _repository) = project();
        fs::write(temporary.path().join("target.yaml"), TARGET).unwrap();
        fs::create_dir_all(temporary.path().join("nested/deep")).unwrap();

        let inventory = discover_gitops_inventory(&temporary.path().join("nested/deep"), None).unwrap();
        assert_eq!(inventory.project_root, temporary.path().canonicalize().unwrap());
        assert!(inventory
            .get(GitOpsResourceKind::DeploymentTarget, "production")
            .is_some());
    }

    #[test]
    fn rediscovery_reuses_the_parsed_project_configuration() {
        let (temporary, _repository) = project();
        fs::write(temporary.path().join("target.yaml"), TARGET).unwrap();

        let inventory = discover_gitops_inventory(temporary.path(), None).unwrap();
        fs::write(temporary.path().join("nyl.toml"), "not valid TOML = [").unwrap();

        let rediscovered = inventory.rediscover(None).unwrap();
        assert_eq!(rediscovered.project_config, inventory.project_config);
        assert!(rediscovered
            .get(GitOpsResourceKind::DeploymentTarget, "production")
            .is_some());
    }
}
