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

/// Choose the group whose bindings apply, or `None` for defaults.
///
/// `enabled` and `disabled` name the target's selected groups. `rendering`
/// returns the enabled groups that render the Release file; it runs only when
/// neither `--defaults-only` nor `--application-group` decides, so only then
/// are group sources resolved.
///
/// Contract: [Direct commands](../../../../design/release-inputs.md#direct-commands).
pub fn select_group(
    target: &str,
    enabled: &[String],
    disabled: &BTreeSet<String>,
    file: &Path,
    application_group: Option<&str>,
    rendering: impl FnOnce() -> Result<Vec<String>>,
) -> Result<Option<String>> {
    let names = || enabled.join(", ");
    if let Some(requested) = application_group {
        return if enabled.iter().any(|name| name == requested) {
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
    match rendering()?.as_slice() {
        [group] => Ok(Some(group.clone())),
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

/// The enabled groups of `groups` whose local source renders `file`, by the
/// same file selection as `render-tree`.
fn groups_rendering(
    inventory: &GitOpsInventory,
    groups: &[(PathBuf, crate::resources::ApplicationGroup)],
    file: &Path,
) -> Result<Vec<String>> {
    let mut rendering = Vec::new();
    for (resource_path, group) in groups {
        let Some((root, source)) = super::tree::local_group_source(inventory, resource_path, group)? else {
            continue;
        };
        let renders = super::tree::local_candidate_files(inventory, &root, &source)
            .iter()
            .any(|candidate| candidate.canonicalize().is_ok_and(|candidate| candidate == file));
        if renders {
            rendering.push(group.metadata.name.clone());
        }
    }
    Ok(rendering)
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
    let group = if selection.defaults_only {
        None
    } else {
        let (cluster, _) = super::tree::resolve_cluster(inventory, target.cluster_name())?;
        let session = crate::render::RenderSession::for_target(
            &inventory.project_root,
            &inventory.project_config,
            target,
            &cluster,
        )?;
        let (groups, disabled) = super::tree::selected_groups(inventory, target, &session)?;
        let enabled = groups
            .iter()
            .map(|(_, group)| group.metadata.name.clone())
            .collect::<Vec<_>>();
        select_group(
            target_name,
            &enabled,
            &disabled,
            &file,
            selection.application_group.as_deref(),
            || groups_rendering(inventory, &groups, &file),
        )?
    };
    let key = group.map(|group| ReleaseKey {
        group,
        release: release_name.clone(),
    });
    let bindings = key
        .as_ref()
        .and_then(|key| target.spec.release_inputs.get(&key.to_string()));
    if let (Some(key), Some(bindings)) = (&key, bindings) {
        let issues = super::inputs::undeclared_binding_issues(key, bindings, &declarations);
        if !issues.is_empty() {
            return Err(NylError::config(format!(
                "DeploymentTarget {target_name:?} has invalid Release inputs:\n{}",
                issues
                    .iter()
                    .map(|issue| format!("  - {issue}"))
                    .collect::<Vec<_>>()
                    .join("\n")
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

    fn select(requested: Option<&str>, rendering: Vec<&str>) -> (Result<Option<String>>, bool) {
        let enabled = ["platform", "workloads"].map(ToOwned::to_owned);
        let disabled = BTreeSet::from(["retired".to_owned()]);
        let mut asked = false;
        let selected = select_group("dev", &enabled, &disabled, Path::new("/p/api.yaml"), requested, || {
            asked = true;
            Ok(rendering.into_iter().map(ToOwned::to_owned).collect())
        });
        (selected, asked)
    }

    #[test]
    fn test_select_group_uses_the_one_group_rendering_the_file() {
        let (selected, asked) = select(None, vec!["workloads"]);
        assert_eq!(selected.unwrap(), Some("workloads".to_owned()));
        assert!(asked);
    }

    #[test]
    fn test_select_group_requires_a_choice_when_several_or_no_groups_render_the_file() {
        let several = select(None, vec!["platform", "workloads"]).0.unwrap_err().to_string();
        assert!(
            several.contains("platform, workloads") && several.contains("--application-group"),
            "{several}"
        );
        let none = select(None, vec![]).0.unwrap_err().to_string();
        assert!(
            none.contains("--defaults-only") && none.contains("selected groups: platform, workloads"),
            "{none}"
        );
    }

    #[test]
    fn test_select_group_resolves_no_sources_for_an_explicit_group() {
        let (selected, asked) = select(Some("platform"), vec![]);
        assert_eq!(selected.unwrap(), Some("platform".to_owned()));
        assert!(!asked, "an explicit group needs no source resolution");
        let disabled = select(Some("retired"), vec![]).0.unwrap_err().to_string();
        assert!(disabled.contains("is disabled"), "{disabled}");
        let unknown = select(Some("other"), vec![]).0.unwrap_err().to_string();
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
