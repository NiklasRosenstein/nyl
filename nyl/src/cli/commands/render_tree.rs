use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Args;
use colored::Colorize;

use crate::gitops::{
    compile_target_tree_cached_with_observer_and_options, discover_gitops_inventory,
    reconcile_rendered_tree_with_options, resolve_deployment_target_name, validate_rendered_tree_owner, GitOpsCache,
    RenderIndex, RenderIndexPublication, TreeCacheArgs, TreeRenderOptions,
};
use crate::resources::{DeploymentTarget, GitOpsResource, GitOpsResourceKind};
use crate::{NylError, Result};

use super::super::tree_progress::{TreeProgressArgs, TreeProgressReporter};

/// Render one deployment target into its owned manifest tree.
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent CLI switches compose without hidden state.
pub struct RenderTreeArgs {
    #[command(flatten)]
    pub validation: crate::validation::ValidationArgs,
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

    /// Allow project secrets and NYL_* environment variables to affect rendered output.
    #[arg(long)]
    pub allow_secret_inputs: bool,

    /// Resolve Git charts from cached refs and read fromPublication state at the cached publication branch head.
    #[arg(long)]
    pub offline: bool,
}

#[allow(clippy::too_many_lines)] // One linear command flow from arguments to the rendered tree.
pub async fn execute(args: RenderTreeArgs) -> Result<()> {
    args.validation
        .validate_outputs(true, &[], std::slice::from_ref(&args.output_dir))?;
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
    args.validation.enabled(&inventory.project_config.config.validation)?;

    if !args.check {
        validate_rendered_tree_owner(&output_root, &render_index_owner(&target_name, &target))?;
    }

    let cache = GitOpsCache::new(&inventory.project_root, args.cache.mode())?;
    let _cache_reporter = cache.reporter();
    let mut progress = TreeProgressReporter::new(args.progress, None);
    let compiled = compile_target_tree_cached_with_observer_and_options(
        &inventory,
        &target_name,
        &cache,
        &mut progress,
        TreeRenderOptions {
            allow_secret_inputs: args.allow_secret_inputs,
            publication_read: crate::gitops::inputs::PublicationRead::from_offline(args.offline),
            ..TreeRenderOptions::default()
        },
    )
    .await?;
    if let Some(base) = &compiled.publication_base {
        eprintln!("{}", base.describe());
    }
    let validation = crate::validation::validate_tree(&args.validation, &inventory, &compiled).await;
    if args.check {
        validation?;
        report_checked_target(
            &target_name,
            compiled.files.len(),
            started.elapsed(),
            args.validation.report_stdout(),
        );
        return Ok(());
    }

    let carried = crate::gitops::inputs::carry_paths(&inventory, &target_name)?;
    let crate::gitops::provenance::WorktreeSourceState {
        commit: source_commit,
        dirty,
    } = crate::gitops::provenance::worktree_source_state(&inventory.project_root, &carried)?;
    let inputs = crate::gitops::provenance::hash_inputs(&inventory, &compiled)?;
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
        &compiled.owned_files(),
        index,
        compiled.reconcile_options(args.force),
    )?;
    report_render_result(
        &target_name,
        &output_root,
        compiled.files.len(),
        started.elapsed(),
        validation,
        args.validation.report_stdout(),
    )
}

fn report_checked_target(target: &str, count: usize, elapsed: Duration, report_stdout: bool) {
    let target = target.cyan().bold();
    let message = format!(
        "✓ deployment target {target} is valid ({}, {})",
        format_file_count(count),
        format_elapsed(elapsed)
    );
    if report_stdout {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

fn report_render_result(
    target_name: &str,
    output_root: &Path,
    file_count: usize,
    elapsed: Duration,
    validation: Result<()>,
    report_stdout: bool,
) -> Result<()> {
    let target = target_name.cyan().bold();
    let output = crate::util::path_for_display(output_root)
        .display()
        .to_string()
        .replace(std::path::MAIN_SEPARATOR, "/")
        .green();
    if let Err(error) = validation {
        eprintln!("Deployment target {target} rendered at {output}; validation failed. Inspect the rendered files.");
        return Err(error);
    }
    let message = format!(
        "✓ deployment target {target} ready at {output} ({}, {})",
        format_file_count(file_count),
        format_elapsed(elapsed)
    );
    if report_stdout {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
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
