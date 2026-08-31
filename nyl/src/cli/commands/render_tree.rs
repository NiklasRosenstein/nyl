use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Args;
use git2::{Repository, StatusOptions};

use crate::gitops::{
    compile_target_tree, discover_gitops_inventory, reconcile_rendered_tree, RenderIndex, RenderIndexDestination,
};
use crate::resources::{GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

/// Render one GitOps target into its owned manifest tree.
#[derive(Args, Debug)]
pub struct RenderTreeArgs {
    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Name of the GitOpsTarget to render.
    #[arg(long)]
    pub target: String,

    /// Root of the destination Git worktree.
    #[arg(long)]
    pub output_dir: PathBuf,

    /// Compile and validate without writing files.
    #[arg(long)]
    pub check: bool,
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
    let output_root = if target.spec.destination.path_prefix.is_empty() {
        output_dir.clone()
    } else {
        output_dir.join(&target.spec.destination.path_prefix)
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
        discover_gitops_inventory(&args.path, Some(excluded))?
    } else {
        initial
    };

    let compiled = compile_target_tree(&inventory, &args.target).await?;
    if args.check {
        println!(
            "✓ GitOps target {} renders {} file(s)",
            args.target,
            compiled.files.len()
        );
        return Ok(());
    }

    let (source_commit, dirty) = source_state(&inventory.project_root)?;
    let inputs = hash_inputs(&inventory.project_root, &compiled.inputs)?;
    let repository_identity = compiled
        .repository_name
        .clone()
        .unwrap_or_else(|| compiled.repository.repo_url.clone());
    let index = RenderIndex::new(
        args.target.clone(),
        RenderIndexDestination {
            repository: repository_identity,
            revision: compiled.target.spec.destination.revision.clone(),
            path_prefix: compiled.target.spec.destination.path_prefix.clone(),
        },
        compiled.target.spec.profile.clone(),
        source_commit,
        dirty,
        inputs,
    );
    reconcile_rendered_tree(&output_root, &compiled.files, index)?;
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

fn source_state(project_root: &Path) -> Result<(Option<String>, bool)> {
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
    let dirty = !repository
        .statuses(Some(&mut options))
        .map_err(|error| NylError::config(format!("Failed to inspect source Git status: {error}")))?
        .is_empty();
    Ok((commit, dirty))
}

fn hash_inputs(project_root: &Path, inputs: &std::collections::BTreeSet<PathBuf>) -> Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    for relative in inputs {
        if relative.starts_with("@remote") {
            continue;
        }
        let path = project_root.join(relative);
        if path.is_file() {
            hashes.insert(
                relative.to_string_lossy().replace('\\', "/"),
                crate::gitops::reconcile::sha256(&std::fs::read(path)?),
            );
        }
    }
    Ok(hashes)
}
