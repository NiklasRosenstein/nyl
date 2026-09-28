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
    CachedGitBlobSource, InputOrigin, InputSources, PublicationBase, PublicationRead, PublicationScope,
    ResolvedReleaseInputs,
};
use super::GitOpsInventory;
use crate::resources::release_inputs::{InputBinding, InputDeclaration, ReleaseKey};
use crate::resources::ApplicationGroupSource;
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::util::project_path::ProjectPaths;
use crate::{NylError, Result};

/// Which target bindings a direct command applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectInputSelection {
    /// `--application-group`: the group whose bindings apply.
    pub application_group: Option<String>,
    /// `--defaults-only`: apply no target binding.
    pub defaults_only: bool,
    /// `--input` and `--inputs` values, already merged.
    pub overrides: BTreeMap<String, Value>,
}

/// What a direct command has already loaded, shared with input resolution.
pub struct DirectInputContext<'a> {
    pub project_root: &'a Path,
    pub project_config: &'a crate::config::ProjectConfig,
    /// The project inventory and the DeploymentTarget whose bindings apply;
    /// `None` renders defaults and overrides only.
    pub target: Option<(&'a GitOpsInventory, &'a crate::resources::DeploymentTarget)>,
    /// The command's render cache, for `fromGit` reads and cache modes.
    pub cache: Option<&'a crate::render::cache::RenderCache>,
}

/// Resolved inputs of the Release a direct command renders.
#[derive(Debug)]
pub struct DirectInputs {
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

    /// Project files the inputs were read from: `fromFile` files, carry files,
    /// and referenced GitRepository resources.
    pub fn files(&self) -> Vec<PathBuf> {
        self.resolved
            .inputs
            .values()
            .filter_map(|input| match &input.origin {
                InputOrigin::File(path)
                | InputOrigin::Carried { source: path, .. }
                | InputOrigin::Git {
                    repository_source: Some(path),
                    ..
                } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }
}

/// One enabled ApplicationGroup a target selects, with its local source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetGroup {
    pub name: String,
    /// Canonical local source directory and its file selection; `None` for a
    /// remote source, which never contains a local file.
    pub source: Option<(PathBuf, ApplicationGroupSource)>,
}

impl TargetGroup {
    /// Whether this group renders `file`, by the same selection as
    /// `render-tree`: beneath its root and matched by its include, exclude,
    /// and recursion rules.
    fn contains(&self, file: &Path) -> bool {
        self.source
            .as_ref()
            .is_some_and(|(root, source)| super::tree::source_matches(root, file, source))
    }
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
/// `groups` are the target's enabled groups; `disabled` names its selected
/// but disabled ones, whose bindings `render-tree` ignores.
///
/// Contract: [Direct commands](../../../../design/release-inputs.md#direct-commands).
pub fn select_group(
    target: &str,
    groups: &[TargetGroup],
    disabled: &BTreeSet<String>,
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
        } else if disabled.contains(requested) {
            Err(NylError::config(format!(
                "--application-group {requested:?} is disabled on DeploymentTarget {target:?}, so render-tree ignores its bindings; enable it, or pass --defaults-only"
            )))
        } else {
            Err(NylError::config(format!(
                "--application-group {requested:?} is not an ApplicationGroup DeploymentTarget {target:?} selects; selected groups: {}",
                names()
            )))
        };
    }
    let containing = groups
        .iter()
        .filter(|group| group.contains(file))
        .map(|group| group.name.as_str())
        .collect::<Vec<_>>();
    match containing.as_slice() {
        [group] => Ok(Some((*group).to_owned())),
        [] => Err(NylError::config(format!(
            "No enabled ApplicationGroup of DeploymentTarget {target:?} renders {} from a local source; selected groups: {}. \
             Pass --application-group <name> to apply that group's bindings, or --defaults-only to render with defaults and overrides only",
            file.display(),
            names()
        ))),
        several => Err(NylError::config(format!(
            "ApplicationGroups {} of DeploymentTarget {target:?} all render {}; pass --application-group <name> to choose whose bindings apply",
            several.join(", "),
            file.display()
        ))),
    }
}

/// The target's selected ApplicationGroups: the enabled ones with their local
/// sources, and the names of the disabled ones.
pub fn target_groups(
    inventory: &GitOpsInventory,
    target: &crate::resources::DeploymentTarget,
) -> Result<(Vec<TargetGroup>, BTreeSet<String>)> {
    let (cluster, _) = super::tree::resolve_cluster(inventory, target.cluster_name())?;
    let session =
        crate::render::RenderSession::for_target(&inventory.project_root, &inventory.project_config, target, &cluster)?;
    let mut groups = Vec::new();
    let mut disabled = BTreeSet::new();
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
        // Like render-tree, a disabled group is skipped before its source is
        // resolved, so a missing source directory cannot fail the command.
        if !group.spec.enabled {
            disabled.insert(group.metadata.name.clone());
            continue;
        }
        let source = match &group.spec.source {
            Some(source) if source.is_remote() => None,
            Some(source) => Some((
                super::tree::local_group_source_root(inventory, &group.metadata.name, &source.path)?,
                source.clone(),
            )),
            None => Some((
                super::derived_group_source_root(
                    &inventory.project_root,
                    &discovered.source_path,
                    &group.metadata.name,
                ),
                super::tree::default_group_source(),
            )),
        };
        groups.push(TargetGroup {
            name: group.metadata.name.clone(),
            source: source.map(|(root, source)| (root.canonicalize().unwrap_or(root), source)),
        });
    }
    groups.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((groups, disabled))
}

/// Resolve the inputs of the Release in `file` for a direct command.
///
/// Returns `None` when the file holds no Release that declares inputs and no
/// override is given, so such Releases render exactly as before.
pub fn resolve_direct_inputs(
    context: &DirectInputContext<'_>,
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
    let undeclared = |names: &mut dyn Iterator<Item = &String>| {
        names
            .filter(|name| !declarations.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>()
    };
    let unknown = undeclared(&mut selection.overrides.keys());
    if !unknown.is_empty() {
        return Err(NylError::config(format!(
            "--input/--inputs set {} that Release {release_name:?} does not declare; declared inputs: {}",
            unknown.join(", "),
            declarations.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    }

    let Some((inventory, target)) = context.target else {
        let sources = Sources::without_target(context);
        return resolve_with(&release_name, None, declarations, &selection.overrides, &sources).map(Some);
    };
    let target_name = &target.metadata.name;
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let (groups, disabled) = target_groups(inventory, target)?;
    let group = select_group(
        target_name,
        &groups,
        &disabled,
        &file,
        selection.application_group.as_deref(),
        selection.defaults_only,
    )?;
    let key = group.map(|group| ReleaseKey {
        group,
        release: release_name.clone(),
    });
    let bindings = key
        .as_ref()
        .and_then(|key| target.spec.release_inputs.get(&key.to_string()));
    if let (Some(key), Some(bindings)) = (&key, bindings) {
        // render-tree rejects a binding for an undeclared input; so does this.
        let unknown = undeclared(&mut bindings.keys());
        if !unknown.is_empty() {
            return Err(NylError::config(format!(
                "DeploymentTarget {target_name:?} spec.releaseInputs.{:?} binds {} that Release {key} does not declare; declared inputs: {}",
                key.to_string(),
                unknown.join(", "),
                declarations.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let sources = Sources::for_target(context, inventory, target, bindings, &selection.overrides)?;
    resolve_with(
        &release_name,
        key.as_ref().map(|key| (key, bindings)),
        declarations,
        &selection.overrides,
        &sources,
    )
    .map(Some)
}

/// Everything [`InputSources`] borrows for one direct command.
struct Sources<'a> {
    paths: ProjectPaths,
    visible_files: std::borrow::Cow<'a, BTreeSet<PathBuf>>,
    inventory: Option<&'a GitOpsInventory>,
    target_name: Option<&'a str>,
    publication_scope: Option<PublicationScope>,
    git: Box<CachedGitBlobSource>,
    publication_base: Option<PublicationBase>,
}

impl<'a> Sources<'a> {
    fn without_target(context: &DirectInputContext<'a>) -> Self {
        Self {
            paths: ProjectPaths::new(context.project_root.to_path_buf(), context.project_root.to_path_buf()),
            visible_files: std::borrow::Cow::Owned(BTreeSet::new()),
            inventory: None,
            target_name: None,
            publication_scope: None,
            git: Box::new(CachedGitBlobSource::new(
                None,
                context.project_root,
                context.project_config,
                context.cache.cloned(),
            )),
            publication_base: None,
        }
    }

    fn for_target(
        context: &DirectInputContext<'a>,
        inventory: &'a GitOpsInventory,
        target: &'a crate::resources::DeploymentTarget,
        bindings: Option<&BTreeMap<String, InputBinding>>,
        overrides: &BTreeMap<String, Value>,
    ) -> Result<Self> {
        let git = Box::new(CachedGitBlobSource::new(
            None,
            &inventory.project_root,
            &inventory.project_config,
            context.cache.cloned(),
        ));
        // The branch is read only when a fromPublication binding decides an
        // input, not when an override replaces every such input.
        let reads_publication = bindings.is_some_and(|bindings| {
            bindings
                .iter()
                .any(|(name, binding)| binding.from_publication.is_some() && !overrides.contains_key(name))
        });
        let publication_base = if reads_publication {
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
            visible_files: std::borrow::Cow::Borrowed(&inventory.worktree_data_files),
            publication_scope: Some(PublicationScope::of(inventory, target)?),
            inventory: Some(inventory),
            target_name: Some(&target.metadata.name),
            git,
            publication_base,
        })
    }
}

fn resolve_with(
    release_name: &str,
    bound: Option<(&ReleaseKey, Option<&BTreeMap<String, InputBinding>>)>,
    declarations: BTreeMap<String, InputDeclaration>,
    overrides: &BTreeMap<String, Value>,
    sources: &Sources<'_>,
) -> Result<DirectInputs> {
    let inventory = sources.inventory;
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
    let (label, bindings, field_prefix) = match bound {
        Some((key, bindings)) => (
            key.to_string(),
            bindings,
            format!(
                "DeploymentTarget {:?} spec.releaseInputs.{:?}",
                sources.target_name.unwrap_or_default(),
                key.to_string()
            ),
        ),
        None => (release_name.to_owned(), None, "--input/--inputs".to_owned()),
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

    fn local(root: &Path, dir: &str) -> Option<(PathBuf, ApplicationGroupSource)> {
        Some((root.join(dir), super::super::tree::default_group_source()))
    }

    fn groups(root: &Path) -> Vec<TargetGroup> {
        let mut flat = super::super::tree::default_group_source();
        flat.recursive = false;
        flat.exclude = vec!["legacy-*".to_owned()];
        vec![
            TargetGroup {
                name: "platform".to_owned(),
                source: local(root, "platform"),
            },
            TargetGroup {
                name: "workloads".to_owned(),
                source: Some((root.join("apps"), flat)),
            },
            TargetGroup {
                name: "wide".to_owned(),
                source: local(root, "apps/shared"),
            },
            TargetGroup {
                name: "remote".to_owned(),
                source: None,
            },
        ]
    }

    fn select(file: &str, requested: Option<&str>, defaults_only: bool) -> Result<Option<String>> {
        let root = Path::new("/p");
        let disabled = BTreeSet::from(["retired".to_owned()]);
        select_group(
            "dev",
            &groups(root),
            &disabled,
            &root.join(file),
            requested,
            defaults_only,
        )
    }

    #[test]
    fn test_select_group_uses_the_group_render_tree_renders_the_file_in() {
        assert_eq!(
            select("apps/web.yaml", None, false).unwrap(),
            Some("workloads".to_owned())
        );
        // workloads is not recursive, so only wide renders a nested file.
        assert_eq!(
            select("apps/shared/db.yaml", None, false).unwrap(),
            Some("wide".to_owned())
        );
    }

    #[test]
    fn test_select_group_requires_a_choice_when_no_group_renders_the_file() {
        // Excluded by workloads' source, so no group renders it.
        let excluded = select("apps/legacy-web.yaml", None, false).unwrap_err().to_string();
        assert!(excluded.contains("--defaults-only"), "{excluded}");
        let none = select("elsewhere/web.yaml", None, false).unwrap_err().to_string();
        assert!(none.contains("platform, workloads, wide, remote"), "{none}");
    }

    #[test]
    fn test_select_group_requires_a_choice_when_several_groups_render_the_file() {
        let root = Path::new("/p");
        let groups = vec![
            TargetGroup {
                name: "a".to_owned(),
                source: local(root, "apps"),
            },
            TargetGroup {
                name: "b".to_owned(),
                source: local(root, "apps/shared"),
            },
        ];
        let error = select_group(
            "dev",
            &groups,
            &BTreeSet::new(),
            &root.join("apps/shared/db.yaml"),
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("a, b") && error.contains("--application-group"),
            "{error}"
        );
    }

    #[test]
    fn test_select_group_honours_explicit_choices() {
        assert_eq!(
            select("elsewhere/web.yaml", Some("remote"), false).unwrap(),
            Some("remote".to_owned())
        );
        assert_eq!(select("elsewhere/web.yaml", None, true).unwrap(), None);
        let disabled = select("elsewhere/web.yaml", Some("retired"), false)
            .unwrap_err()
            .to_string();
        assert!(disabled.contains("is disabled"), "{disabled}");
        let unknown = select("elsewhere/web.yaml", Some("other"), false)
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
