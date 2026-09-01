use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Args;
use git2::{Repository, StatusOptions};

use crate::gitops::{
    compile_target_tree_cached, discover_gitops_inventory, reconcile_rendered_tree_with_options, GitOpsCache,
    ReconcileOptions, RenderIndex, RenderIndexPublication, TreeCacheArgs,
};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// Render one GitOps target into its owned manifest tree.
#[derive(Args, Debug)]
pub struct RenderTreeArgs {
    #[command(flatten)]
    pub cache: TreeCacheArgs,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Name of the GitOpsTarget to render.
    #[arg(long)]
    pub target: String,

    /// Root of the publication Git worktree.
    #[arg(long)]
    pub output_dir: PathBuf,

    /// Compile and validate without writing files.
    #[arg(long)]
    pub check: bool,

    /// Recreate missing owned files and replace owned files modified outside Nyl.
    #[arg(long, conflicts_with = "check")]
    pub force: bool,
}

pub async fn execute(args: RenderTreeArgs) -> Result<()> {
    let initial = discover_gitops_inventory(&args.path, None)?;
    let GitOpsResource::GitOpsTarget(target) = &initial
        .get(GitOpsResourceKind::GitOpsTarget, &args.target)
        .ok_or_else(|| NylError::config(format!("GitOpsTarget {:?} was not found", args.target)))?
        .resource
        .as_ref()
        .ok_or_else(|| NylError::config(format!("GitOpsTarget {:?} must be static", args.target)))?
    else {
        unreachable!("inventory key and resource variant must agree");
    };
    let output_dir = absolute_path(&args.output_dir)?;
    let output_root = if target.spec.publication.path_prefix.is_empty() {
        output_dir.clone()
    } else {
        output_dir.join(&target.spec.publication.path_prefix)
    };
    if output_root == initial.project_root {
        return Err(NylError::config(
            "The rendered target prefix must not be the project root",
        ));
    }
    let excluded = output_root
        .strip_prefix(&initial.project_root)
        .ok()
        .map(Path::to_path_buf);
    let inventory = if let Some(excluded) = excluded.as_deref() {
        initial.rediscover(Some(excluded))?
    } else {
        initial
    };

    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let compiled = compile_target_tree_cached(&inventory, &args.target, &cache, excluded.as_deref()).await?;
    if args.check {
        println!(
            "✓ GitOps target {} renders {} file(s)",
            args.target,
            compiled.files.len()
        );
        return Ok(());
    }

    let (source_commit, dirty) = source_state(&inventory.project_root)?;
    let inputs = hash_inputs(&inventory, &compiled)?;
    let repository_identity = compiled
        .repository_name
        .clone()
        .unwrap_or_else(|| compiled.repository.repo_url.clone());
    let index = RenderIndex::new(
        args.target.clone(),
        compiled.cluster.metadata.name.clone(),
        RenderIndexPublication {
            repository: repository_identity,
            revision: compiled.target.spec.publication.revision.clone(),
            path_prefix: compiled.target.spec.publication.path_prefix.clone(),
        },
        source_commit,
        dirty,
        inputs,
    );
    reconcile_rendered_tree_with_options(
        &output_root,
        &compiled.files,
        index,
        ReconcileOptions {
            force_owned: args.force,
        },
    )?;
    println!(
        "✓ Rendered GitOps target {} to {} ({} file(s))",
        args.target,
        output_root.display(),
        compiled.files.len()
    );
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

pub(super) fn source_state(project_root: &Path) -> Result<(Option<String>, bool)> {
    let repository = Repository::discover(project_root)
        .map_err(|error| NylError::config(format!("Failed to inspect source Git repository: {error}")))?;
    let commit = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string());
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(|error| NylError::config(format!("Failed to inspect source Git status: {error}")))?;
    let dirty = statuses
        .iter()
        .any(|entry| !is_project_cache_status(&repository, project_root, entry.path()));
    Ok((commit, dirty))
}

fn is_project_cache_status(repository: &Repository, project_root: &Path, status_path: Option<&str>) -> bool {
    let Some(worktree) = repository.workdir() else {
        return false;
    };
    let Ok(project_relative) = project_root.strip_prefix(worktree) else {
        return false;
    };
    status_path.is_some_and(|path| Path::new(path).starts_with(project_relative.join(".nyl/cache")))
}

pub(super) fn hash_inputs(
    inventory: &crate::gitops::GitOpsInventory,
    compiled: &crate::gitops::CompiledTargetTree,
) -> Result<BTreeMap<String, String>> {
    let cluster_resource = inventory
        .get(GitOpsResourceKind::Cluster, &compiled.cluster.metadata.name)
        .ok_or_else(|| NylError::config("Compiled Cluster is missing from the GitOps inventory"))?;
    let cluster_path = &cluster_resource.source_path;
    let mut render_cluster = compiled.cluster.clone();
    render_cluster.spec.live = None;
    let cluster_bytes = serde_json::to_vec(&render_cluster)?;
    let mut hashes = BTreeMap::new();
    for relative in &compiled.inputs {
        if relative.starts_with("@remote") {
            continue;
        }
        let path = inventory.project_root.join(relative);
        if path.is_file() {
            let bytes = if relative == cluster_path {
                let contents = std::fs::read_to_string(&path)?;
                if contents.matches(&cluster_resource.raw_document).count() != 1 {
                    return Err(NylError::config(format!(
                        "Cannot safely locate Cluster '{}' in {} for provenance hashing",
                        compiled.cluster.metadata.name,
                        path.display()
                    )));
                }
                contents
                    .replacen(
                        &cluster_resource.raw_document,
                        std::str::from_utf8(&cluster_bytes).expect("JSON serialization is UTF-8"),
                        1,
                    )
                    .into_bytes()
            } else {
                std::fs::read(path)?
            };
            hashes.insert(
                relative.to_string_lossy().replace('\\', "/"),
                crate::gitops::reconcile::sha256(&bytes),
            );
        }
    }
    Ok(hashes)
}
