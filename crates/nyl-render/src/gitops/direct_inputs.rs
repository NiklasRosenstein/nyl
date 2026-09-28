//! Release inputs of direct commands (`nyl render`, `diff`, `apply`).
//!
//! Contract: [Direct commands](../../../../design/release-inputs.md#direct-commands).
//!
//! ```text
//! effective input = --input / --inputs override, if present
//!                   otherwise target binding, if a target is selected
//!                   otherwise Release default
//! ```
//!
//! With a target, the binding comes from the selected ApplicationGroup whose
//! local source contains the Release file, or from the group
//! `--application-group` names. A target never silently falls back to
//! defaults: `--defaults-only` says so explicitly. Bindings resolve through the
//! same [`resolve_release_inputs`](super::inputs::resolve_release_inputs) as
//! `render-tree`, so `render --target dev` matches the tree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::inputs::{
    CachedGitBlobSource, InputSources, PublicationBase, PublicationRead, PublicationScope, ResolvedReleaseInputs,
};
use super::GitOpsInventory;
use crate::resources::release_inputs::{InputDeclaration, ReleaseKey};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::util::project_path::ProjectPaths;
use crate::{NylError, Result};

/// Which target bindings a direct command applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectInputSelection {
    /// DeploymentTarget whose bindings apply; `None` renders defaults and
    /// overrides only.
    pub target: Option<String>,
    /// `--application-group`: the group whose bindings apply.
    pub application_group: Option<String>,
    /// `--defaults-only`: apply no target binding.
    pub defaults_only: bool,
    /// `--input` and `--inputs` values, already merged.
    pub overrides: BTreeMap<String, Value>,
}

/// Resolved inputs of the Release a direct command renders.
#[derive(Debug)]
pub struct DirectInputs {
    /// `<group>/<release>` whose target bindings applied, if any.
    pub key: Option<ReleaseKey>,
    pub declarations: BTreeMap<String, InputDeclaration>,
    pub resolved: ResolvedReleaseInputs,
    /// The publication base `fromPublication` bindings read, if any.
    pub publication_base: Option<PublicationBase>,
}

impl DirectInputs {
    /// The `inputs` template variable.
    pub fn values(&self) -> Map<String, Value> {
        self.resolved.values()
    }
}

/// One selected ApplicationGroup of a target and its local source root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetGroup {
    pub name: String,
    /// Canonical local source directory; `None` for a remote source.
    pub root: Option<PathBuf>,
}

/// Merge `--inputs` (a YAML or JSON object) under individual `--input` flags,
/// which win.
///
/// Each flag is `<name>=<json>`; a value that is not JSON is an error, so a
/// string is written as `--input image='"registry/app@sha256:…"'`.
pub fn parse_overrides(inputs_file: Option<&Path>, input_flags: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut overrides = BTreeMap::new();
    if let Some(path) = inputs_file {
        let document = super::inputs::read_single_document(path)
            .map_err(|reason| NylError::config(format!("--inputs {reason}")))?;
        let Value::Object(object) = document else {
            return Err(NylError::config(format!(
                "--inputs {} must hold a YAML or JSON object of input names to values",
                path.display()
            )));
        };
        overrides.extend(object);
    }
    for flag in input_flags {
        let (name, value) = flag
            .split_once('=')
            .ok_or_else(|| NylError::config(format!("--input {flag:?} must have the form <name>=<json>")))?;
        let value: Value = serde_json::from_str(value).map_err(|error| {
            NylError::config(format!(
                "--input {name}: {value:?} is not JSON ({error}); quote strings, for example --input {name}='\"value\"'"
            ))
        })?;
        overrides.insert(name.to_owned(), value);
    }
    Ok(overrides)
}

/// Choose the group whose bindings apply to `file`, or `None` for defaults.
///
/// Contract: [Direct commands](../../../../design/release-inputs.md#direct-commands).
pub fn select_group(
    target: &str,
    groups: &[TargetGroup],
    file: &Path,
    application_group: Option<&str>,
    defaults_only: bool,
) -> Result<Option<String>> {
    if defaults_only {
        return Ok(None);
    }
    let names = || {
        groups
            .iter()
            .map(|group| group.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if let Some(requested) = application_group {
        return if groups.iter().any(|group| group.name == requested) {
            Ok(Some(requested.to_owned()))
        } else {
            Err(NylError::config(format!(
                "--application-group {requested:?} is not an ApplicationGroup DeploymentTarget {target:?} selects; selected groups: {}",
                names()
            )))
        };
    }
    let containing = groups
        .iter()
        .filter(|group| group.root.as_ref().is_some_and(|root| file.starts_with(root)))
        .map(|group| group.name.as_str())
        .collect::<Vec<_>>();
    match containing.as_slice() {
        [group] => Ok(Some((*group).to_owned())),
        [] => Err(NylError::config(format!(
            "No ApplicationGroup of DeploymentTarget {target:?} has a local source containing {}; selected groups: {}. \
             Pass --application-group <name> to apply that group's bindings, or --defaults-only to render with defaults and overrides only",
            file.display(),
            names()
        ))),
        several => Err(NylError::config(format!(
            "ApplicationGroups {} of DeploymentTarget {target:?} all contain {}; pass --application-group <name> to choose whose bindings apply",
            several.join(", "),
            file.display()
        ))),
    }
}

/// The target's selected ApplicationGroups with their local source roots.
pub fn target_groups(
    inventory: &GitOpsInventory,
    target: &crate::resources::DeploymentTarget,
) -> Result<Vec<TargetGroup>> {
    let (cluster, _) = super::tree::resolve_cluster(inventory, target.cluster_name())?;
    let session =
        crate::render::RenderSession::for_target(&inventory.project_root, &inventory.project_config, target, &cluster)?;
    let mut groups = Vec::new();
    for discovered in inventory.resources.values() {
        if discovered.identity.kind != GitOpsResourceKind::ApplicationGroup
            || !super::tree::target_selects_group(target, &discovered.static_labels)
        {
            continue;
        }
        let Some(GitOpsResource::ApplicationGroup(group)) =
            super::tree::render_effective_control(discovered, &session)?
        else {
            continue;
        };
        let root = match &group.spec.source {
            Some(source) if source.is_remote() => None,
            Some(source) => Some(super::tree::local_group_source_root(
                inventory,
                &group.metadata.name,
                &source.path,
            )?),
            None => Some(super::derived_group_source_root(
                &inventory.project_root,
                &discovered.source_path,
                &group.metadata.name,
            )),
        };
        groups.push(TargetGroup {
            name: group.metadata.name.clone(),
            root: root.map(|root| root.canonicalize().unwrap_or(root)),
        });
    }
    groups.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(groups)
}

/// Resolve the inputs of the Release in `file` for a direct command.
///
/// Returns `None` when the file holds no Release that declares inputs and no
/// override is given, so such Releases render exactly as before.
pub fn resolve_direct_inputs(
    project_root: &Path,
    project_config: &crate::config::ProjectConfig,
    file: &Path,
    selection: &DirectInputSelection,
) -> Result<Option<DirectInputs>> {
    // The render reports a missing or unreadable file in its own words.
    if !file.is_file() {
        return Ok(None);
    }
    let envelope = crate::render::static_release_envelope(file)?;
    let (name, declarations) = match envelope {
        Some(envelope) if !envelope.inputs.is_empty() => (envelope.name, envelope.inputs),
        _ if selection.overrides.is_empty() => return Ok(None),
        _ => {
            return Err(NylError::config(format!(
                "--input and --inputs set Release inputs, but {} holds no Release that declares spec.inputs",
                file.display()
            )))
        }
    };
    let release_name = name.expect("a Release that declares inputs has a literal name");
    let undeclared = selection
        .overrides
        .keys()
        .filter(|name| !declarations.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !undeclared.is_empty() {
        return Err(NylError::config(format!(
            "--input/--inputs set {} that Release {release_name:?} does not declare; declared inputs: {}",
            undeclared.join(", "),
            declarations.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    let Some(target_name) = &selection.target else {
        return resolve_with(
            &release_name,
            None,
            declarations,
            &selection.overrides,
            &Sources::without_target(project_root, project_config),
        )
        .map(Some);
    };
    let inventory = super::discover_gitops_inventory(project_root, None)?;
    let Some(super::DiscoveredGitOpsResource {
        resource: Some(GitOpsResource::DeploymentTarget(target)),
        ..
    }) = inventory.get(GitOpsResourceKind::DeploymentTarget, target_name)
    else {
        return Err(NylError::config(format!(
            "DeploymentTarget {target_name:?} was not found"
        )));
    };
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let groups = target_groups(&inventory, target)?;
    let group = select_group(
        target_name,
        &groups,
        &file,
        selection.application_group.as_deref(),
        selection.defaults_only,
    )?;
    let key = group.map(|group| ReleaseKey {
        group,
        release: release_name.clone(),
    });
    let sources = Sources::for_target(&inventory, target, key.as_ref())?;
    let mut inputs = resolve_with(
        &release_name,
        key.as_ref().map(|key| (key, target)),
        declarations,
        &selection.overrides,
        &sources,
    )?;
    inputs.key = key;
    Ok(Some(inputs))
}

/// Everything [`InputSources`] borrows, owned for one direct command.
struct Sources {
    paths: ProjectPaths,
    visible_files: BTreeSet<PathBuf>,
    inventory: Option<GitOpsInventory>,
    publication_scope: Option<PublicationScope>,
    git: Box<CachedGitBlobSource>,
    publication_base: Option<PublicationBase>,
}

impl Sources {
    fn without_target(project_root: &Path, project_config: &crate::config::ProjectConfig) -> Self {
        Self {
            paths: ProjectPaths::new(project_root.to_path_buf(), project_root.to_path_buf()),
            visible_files: BTreeSet::new(),
            inventory: None,
            publication_scope: None,
            git: Box::new(CachedGitBlobSource::new(None, project_root, project_config, None)),
            publication_base: None,
        }
    }

    fn for_target(
        inventory: &GitOpsInventory,
        target: &crate::resources::DeploymentTarget,
        key: Option<&ReleaseKey>,
    ) -> Result<Self> {
        let git = Box::new(CachedGitBlobSource::new(
            None,
            &inventory.project_root,
            &inventory.project_config,
            None,
        ));
        let bindings = key.and_then(|key| target.spec.release_inputs.get(&key.to_string()));
        let publication_base =
            if bindings.is_some_and(|bindings| bindings.values().any(|binding| binding.from_publication.is_some())) {
                let (_, repository, _) = super::tree::resolve_git_publication(inventory, &target.spec.publication)?;
                Some(PublicationBase::resolve(
                    git.as_ref(),
                    repository.publish_url.as_deref().unwrap_or(&repository.repo_url),
                    &target.spec.publication.revision,
                    target.publication_path_prefix(),
                    PublicationRead::Fresh,
                )?)
            } else {
                None
            };
        Ok(Self {
            paths: inventory.paths(),
            visible_files: inventory.worktree_data_files.clone(),
            publication_scope: Some(PublicationScope::of(inventory, target)?),
            inventory: Some(inventory.clone()),
            git,
            publication_base,
        })
    }
}

fn resolve_with(
    release_name: &str,
    bound: Option<(&ReleaseKey, &crate::resources::DeploymentTarget)>,
    declarations: BTreeMap<String, InputDeclaration>,
    overrides: &BTreeMap<String, Value>,
    sources: &Sources,
) -> Result<DirectInputs> {
    let inventory = sources.inventory.as_ref();
    let repositories = |reference: &crate::resources::LocalReference| {
        let inventory = inventory.ok_or_else(|| {
            NylError::config(format!(
                "GitRepository {:?} can only be resolved with --target",
                reference.name
            ))
        })?;
        let (repository, source) = inventory.resolve_git_repository(Some(reference), None)?;
        let source = source.expect("a referenced GitRepository has a source file");
        Ok((repository, inventory.project_root.join(source)))
    };
    let input_sources = InputSources {
        paths: &sources.paths,
        visible_files: &sources.visible_files,
        repositories: &repositories,
        publication_scope: sources.publication_scope.as_ref(),
        git: sources.git.as_ref(),
        publication: sources.publication_base.as_ref(),
        pinned_state: None,
    };
    let (label, bindings) = match bound {
        Some((key, target)) => (key.to_string(), target.spec.release_inputs.get(&key.to_string())),
        None => (release_name.to_owned(), None),
    };
    let field_prefix = match bound {
        Some((key, target)) => format!(
            "DeploymentTarget {:?} spec.releaseInputs.{:?}",
            target.metadata.name,
            key.to_string()
        ),
        None => "--input/--inputs".to_owned(),
    };
    let mut issues = Vec::new();
    let resolved = super::inputs::resolve_release_inputs(
        &field_prefix,
        &label,
        &declarations,
        bindings,
        overrides,
        &input_sources,
        &mut issues,
    );
    if !issues.is_empty() {
        return Err(NylError::config(format!(
            "Release {label} has invalid inputs:\n{}",
            issues
                .iter()
                .map(|issue| format!("  - {issue}"))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }
    Ok(DirectInputs {
        key: None,
        declarations,
        resolved,
        publication_base: sources.publication_base.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn groups(root: &Path) -> Vec<TargetGroup> {
        vec![
            TargetGroup {
                name: "platform".to_owned(),
                root: Some(root.join("platform")),
            },
            TargetGroup {
                name: "workloads".to_owned(),
                root: Some(root.join("apps")),
            },
            TargetGroup {
                name: "wide".to_owned(),
                root: Some(root.join("apps/shared")),
            },
            TargetGroup {
                name: "remote".to_owned(),
                root: None,
            },
        ]
    }

    #[test]
    fn test_select_group_uses_the_one_group_containing_the_file() {
        let root = Path::new("/p");
        assert_eq!(
            select_group("dev", &groups(root), &root.join("apps/web.yaml"), None, false).unwrap(),
            Some("workloads".to_owned())
        );
    }

    #[test]
    fn test_select_group_requires_a_choice_when_several_or_none_contain_the_file() {
        let root = Path::new("/p");
        let several = select_group("dev", &groups(root), &root.join("apps/shared/db.yaml"), None, false)
            .unwrap_err()
            .to_string();
        assert!(
            several.contains("workloads, wide") && several.contains("--application-group"),
            "{several}"
        );
        let none = select_group("dev", &groups(root), &root.join("elsewhere/web.yaml"), None, false)
            .unwrap_err()
            .to_string();
        assert!(
            none.contains("--defaults-only") && none.contains("platform, workloads, wide, remote"),
            "{none}"
        );
    }

    #[test]
    fn test_select_group_honours_explicit_choices() {
        let root = Path::new("/p");
        let file = root.join("elsewhere/web.yaml");
        assert_eq!(
            select_group("dev", &groups(root), &file, Some("remote"), false).unwrap(),
            Some("remote".to_owned())
        );
        assert_eq!(select_group("dev", &groups(root), &file, None, true).unwrap(), None);
        let unknown = select_group("dev", &groups(root), &file, Some("other"), false)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("is not an ApplicationGroup"), "{unknown}");
    }

    #[test]
    fn test_parse_overrides_lets_flags_win_over_the_file() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("inputs.yaml");
        std::fs::write(&file, "image: from-file\nreplicas: 2\n").unwrap();
        let overrides = parse_overrides(Some(&file), &["image=\"from-flag\"".to_owned()]).unwrap();
        assert_eq!(
            Value::Object(overrides.into_iter().collect()),
            json!({"image": "from-flag", "replicas": 2})
        );
        let error = parse_overrides(None, &["image=plain".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("quote strings"), "{error}");
    }
}
