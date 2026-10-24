//! The resources a direct command (`render`, `diff`, `apply`) works on.
//!
//! Direct commands take the path `render-tree` takes. With a target, a Release
//! that one of the target's ApplicationGroups renders is taken from the compiled
//! target, so its resources, synthesized Namespaces, and shared-namespace
//! ownership are what `render-tree` publishes for its Application.
//! [`DirectScope::Release`] skips compiling the rest of the target and settles
//! the Release alone with [`settle_standalone_release`], which cannot see
//! conflicts with other Releases.
//!
//! Contract: [One declaration, one path](../../../../nyl/book/src/content/docs/start/introduction.md#one-declaration-one-path).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::tree::{
    compile_target_tree_cached_with_options, compile_target_tree_with_options, resolve_cluster, selected_groups,
    settle_standalone_release, TreeRenderOptions,
};
use super::{GitOpsCache, GitOpsInventory};
use crate::kubernetes::ResourceKey;
use crate::render::{Provenance, RenderSession};
use crate::resources::{ApplicationGroup, DeploymentTarget, Release};
use crate::{NylError, Result};

/// How much of the target a direct command compiles for a Release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectScope {
    /// Compile the target and take the Release's Application from it.
    #[default]
    Target,
    /// Settle the Release alone; faster, but blind to other Releases.
    Release,
}

/// What selects the Release's ApplicationGroup and how it is compiled.
pub struct DirectWorkloadRequest<'a> {
    /// The project and target, when the command has one.
    pub target: Option<(&'a GitOpsInventory, &'a DeploymentTarget)>,
    /// The Release file the command renders.
    pub file: &'a Path,
    /// `--application-group`: the group, when the file's location does not decide.
    pub application_group: Option<&'a str>,
    /// `--defaults-only`: render outside every group, resolving no group source.
    pub defaults_only: bool,
    /// The render departs from the target's, for example by `--input`
    /// overrides, so it cannot be taken from the compiled target.
    pub standalone: bool,
    pub scope: DirectScope,
    pub cache: Option<&'a GitOpsCache>,
    pub tree_options: TreeRenderOptions,
}

/// The rendered file, as the direct command rendered it.
pub struct DirectRender {
    pub manifests: Vec<Value>,
    pub manifest_provenance: HashMap<ResourceKey, Provenance>,
    pub release: Option<Release>,
    pub release_provenance: Option<Provenance>,
}

/// The resources the command works on.
pub struct DirectWorkload {
    pub manifests: Vec<Value>,
    pub manifest_provenance: HashMap<ResourceKey, Provenance>,
    /// The namespace Argo CD's destination gives resources that set none; `None`
    /// without a Release.
    pub destination_namespace: Option<String>,
}

/// Resolve the resources a direct command works on for one rendered file.
///
/// A file without a Release is used as rendered. A Release is taken from the
/// compiled target when an enabled ApplicationGroup of the target renders the
/// file itself, the scope is [`DirectScope::Target`], and the render is not
/// standalone; otherwise it is settled alone, with the group that
/// `--application-group` names or that renders the file.
pub async fn resolve_direct_workload(
    request: DirectWorkloadRequest<'_>,
    render: DirectRender,
) -> Result<DirectWorkload> {
    let Some(release) = render.release else {
        return Ok(DirectWorkload {
            manifests: render.manifests,
            manifest_provenance: render.manifest_provenance,
            destination_namespace: None,
        });
    };
    let selected = match request.target {
        Some((inventory, target)) if !request.defaults_only => {
            rendering_group(inventory, target, request.file, request.application_group)?
        }
        _ => None,
    };
    if let (Some((group, true)), Some((inventory, target)), DirectScope::Target, false) =
        (&selected, request.target, request.scope, request.standalone)
    {
        return from_compiled_target(&request, inventory, target, group, &release).await;
    }
    let group = selected.map(|(group, _)| group);
    let settled = settle_standalone_release(
        group.as_ref(),
        &release,
        render.manifests,
        render.manifest_provenance,
        render.release_provenance.as_ref(),
    )?;
    Ok(DirectWorkload {
        manifests: settled.manifests,
        manifest_provenance: settled.manifest_provenance,
        destination_namespace: Some(settled.destination_namespace),
    })
}

/// The enabled ApplicationGroup of the target that renders `file`, by the same
/// file selection as `render-tree`, or the one `--application-group` names,
/// with whether that group renders the file itself.
fn rendering_group(
    inventory: &GitOpsInventory,
    target: &DeploymentTarget,
    file: &Path,
    application_group: Option<&str>,
) -> Result<Option<(ApplicationGroup, bool)>> {
    let (cluster, _) = resolve_cluster(inventory, target.cluster_name())?;
    let session = RenderSession::for_target(&inventory.project_root, &inventory.project_config, target, &cluster)?;
    let (groups, _) = selected_groups(inventory, target, &session)?;
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    if let Some(requested) = application_group {
        let Some((path, group)) = groups.iter().find(|(_, group)| group.metadata.name == requested) else {
            return Err(NylError::config(format!(
                "--application-group {requested:?} is not an enabled ApplicationGroup of DeploymentTarget {:?}",
                target.metadata.name
            )));
        };
        let renders =
            !super::direct_inputs::groups_rendering(inventory, &[(path.clone(), group.clone())], &file)?.is_empty();
        return Ok(Some((group.clone(), renders)));
    }
    let mut rendering = super::direct_inputs::groups_rendering(inventory, &groups, &file)?;
    match rendering.len() {
        0 => Ok(None),
        1 => {
            let name = rendering.remove(0);
            Ok(groups
                .into_iter()
                .map(|(_, group)| group)
                .find(|group| group.metadata.name == name)
                .map(|group| (group, true)))
        }
        _ => Err(NylError::config(format!(
            "ApplicationGroups {} of DeploymentTarget {:?} all render {}; pass --application-group <name> to choose which Application deploys it",
            rendering.join(", "),
            target.metadata.name,
            file.display()
        ))),
    }
}

/// The Release's resources as the compiled target lays them out for its
/// Application.
async fn from_compiled_target(
    request: &DirectWorkloadRequest<'_>,
    inventory: &GitOpsInventory,
    target: &DeploymentTarget,
    group: &ApplicationGroup,
    release: &Release,
) -> Result<DirectWorkload> {
    let target_name = &target.metadata.name;
    // Boxed: the compiler's future is large and would inflate every caller's.
    let compiled = match request.cache {
        Some(cache) => {
            Box::pin(compile_target_tree_cached_with_options(
                inventory,
                target_name,
                cache,
                request.tree_options.clone(),
            ))
            .await?
        }
        None => {
            Box::pin(compile_target_tree_with_options(
                inventory,
                target_name,
                request.tree_options.clone(),
            ))
            .await?
        }
    };
    let group_output = group.spec.output_path.as_deref().unwrap_or(&group.metadata.name);
    let directory = PathBuf::from(group_output).join(&release.metadata.name);
    let mut manifests = Vec::new();
    let mut manifest_provenance = HashMap::new();
    let mut found = false;
    for (path, bytes) in compiled.files.range(directory.clone()..) {
        if !path.starts_with(&directory) {
            break;
        }
        found = true;
        let text = std::str::from_utf8(bytes)
            .map_err(|error| NylError::config(format!("Rendered file {} is not UTF-8: {error}", path.display())))?;
        let documents = serde_saphyr::from_multiple::<Value>(text).map_err(NylError::Yaml)?;
        let provenance = compiled.provenance.get(path);
        for (index, manifest) in documents.into_iter().enumerate() {
            if let Some(origin) = provenance.and_then(|provenance| provenance.get(index)) {
                manifest_provenance.insert(ResourceKey::from_json_value(&manifest)?, origin.clone());
            }
            manifests.push(manifest);
        }
    }
    // A Release may render nothing; it is missing only when the tree did not
    // read the file.
    let file = request
        .file
        .canonicalize()
        .unwrap_or_else(|_| request.file.to_path_buf());
    let rendered_file = inventory
        .paths()
        .key(&file)
        .is_some_and(|key| compiled.inputs.contains(&key));
    if !found && !rendered_file {
        return Err(NylError::config(format!(
            "DeploymentTarget {target_name:?} renders no Release {:?} for ApplicationGroup {:?}; pass --release-only to render the file alone",
            release.metadata.name, group.metadata.name
        )));
    }
    Ok(DirectWorkload {
        manifests,
        manifest_provenance,
        destination_namespace: Some(
            group
                .spec
                .destination_namespace
                .clone()
                .unwrap_or_else(|| release.metadata.namespace.clone()),
        ),
    })
}
