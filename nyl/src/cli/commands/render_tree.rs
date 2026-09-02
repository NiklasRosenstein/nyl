use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Args;
use colored::Colorize;
use git2::{Repository, StatusOptions};

use crate::gitops::{
    compile_target_tree_cached_with_observer, discover_gitops_inventory, reconcile_rendered_tree_with_options,
    resolve_deployment_target_name, validate_rendered_tree_owner, GitOpsCache, ReconcileOptions, RenderIndex,
    RenderIndexPublication, TreeCacheArgs,
};
use crate::resources::{DeploymentTarget, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

use super::super::tree_progress::{TreeProgressArgs, TreeProgressReporter};

/// Render one deployment target into its owned manifest tree.
#[derive(Args, Debug)]
pub struct RenderTreeArgs {
    #[command(flatten)]
    pub cache: TreeCacheArgs,

    #[command(flatten)]
    pub progress: TreeProgressArgs,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// DeploymentTarget to render. Defaults to the sole configured target.
    #[arg(long)]
    pub target: Option<String>,

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
    let started = Instant::now();
    let initial = discover_gitops_inventory(&args.path, None)?;
    let target_name = resolve_deployment_target_name(&initial, args.target.as_deref())?;
    let target = initial
        .get(GitOpsResourceKind::DeploymentTarget, &target_name)
        .expect("resolved DeploymentTarget exists")
        .resource
        .as_ref()
        .and_then(|resource| match resource {
            GitOpsResource::DeploymentTarget(target) => Some(target.clone()),
            _ => None,
        })
        .expect("inventory kind key and resource variant must agree");
    let output_dir = absolute_path(&args.output_dir)?;
    let output_root = if target.publication_path_prefix().is_empty() {
        output_dir.clone()
    } else {
        output_dir.join(target.publication_path_prefix())
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

    if !args.check {
        validate_rendered_tree_owner(&output_root, &render_index_owner(&target_name, &target))?;
    }

    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let _cache_reporter = cache.reporter();
    let mut progress = TreeProgressReporter::new(args.progress, None);
    let compiled = compile_target_tree_cached_with_observer(&inventory, &target_name, &cache, &mut progress).await?;
    if args.check {
        let target = target_name.as_str().cyan().bold();
        println!(
            "✓ deployment target {target} is valid ({}, {})",
            format_file_count(compiled.files.len()),
            format_elapsed(started.elapsed())
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
        target_name.clone(),
        compiled.cluster.metadata.name.clone(),
        RenderIndexPublication {
            repository: repository_identity,
            revision: compiled.target.spec.publication.revision.clone(),
            path_prefix: compiled.target.publication_path_prefix().to_owned(),
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
    let target = target_name.as_str().cyan().bold();
    let output = crate::util::path_for_display(&output_root)
        .display()
        .to_string()
        .green();
    println!(
        "✓ deployment target {target} ready at {output} ({}, {})",
        format_file_count(compiled.files.len()),
        format_elapsed(started.elapsed())
    );
    Ok(())
}

fn render_index_owner(target_name: &str, target: &DeploymentTarget) -> RenderIndex {
    let repository = target
        .spec
        .publication
        .repository_ref
        .as_ref()
        .map(|reference| reference.name.clone())
        .or_else(|| {
            target
                .spec
                .publication
                .repository
                .as_ref()
                .map(|repository| repository.repo_url.clone())
        })
        .expect("validated DeploymentTarget has a publication repository");
    RenderIndex::new(
        target_name.to_owned(),
        target.cluster_name().to_owned(),
        RenderIndexPublication {
            repository,
            revision: target.spec.publication.revision.clone(),
            path_prefix: target.publication_path_prefix().to_owned(),
        },
        None,
        false,
        BTreeMap::new(),
    )
}

fn format_file_count(count: usize) -> String {
    format!("{count} {}", if count == 1 { "file" } else { "files" })
}

fn format_elapsed(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{}ms", elapsed.as_millis())
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
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
    let Ok(worktree) = worktree.canonicalize() else {
        return false;
    };
    let Ok(project_root) = project_root.canonicalize() else {
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
