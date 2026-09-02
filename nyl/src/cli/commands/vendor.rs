//! Project-global remote artifact vendoring.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use colored::Colorize;

use crate::cli::tree_progress::{TreeProgressArgs, TreeProgressReporter};
use crate::gitops::{compile_target_tree_cached_with_observer, discover_gitops_inventory, CacheMode, GitOpsCache};
use crate::render::artifact::DirectoryVendorWriter;
use crate::resources::GitOpsResourceKind;
use crate::{NylError, Result};

#[derive(Args, Debug)]
pub struct VendorArgs {
    #[command(subcommand)]
    command: VendorCommand,
}

#[derive(Subcommand, Debug)]
enum VendorCommand {
    /// Discover and materialize every remote renderer input.
    Sync(VendorRenderArgs),
    /// Verify that the vendor snapshot completely covers the selected targets.
    Check(VendorCheckArgs),
    /// Remove artifact blobs not referenced by vendor/lock.yaml.
    Prune {
        /// Project directory or a path beneath it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Args, Debug)]
struct VendorRenderArgs {
    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// DeploymentTarget to scan. Repeat to scan a subset; all targets are scanned when omitted.
    #[arg(long)]
    target: Vec<String>,

    /// Retrieve every remote input again instead of using vendor/source-cache entries.
    #[arg(long)]
    refresh: bool,

    #[command(flatten)]
    progress: TreeProgressArgs,
}

#[derive(Args, Debug)]
struct VendorCheckArgs {
    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    path: PathBuf,

    /// DeploymentTarget to check. Repeat to check a subset; all targets are checked when omitted.
    #[arg(long)]
    target: Vec<String>,

    #[command(flatten)]
    progress: TreeProgressArgs,
}

pub async fn execute(args: VendorArgs) -> Result<()> {
    match args.command {
        VendorCommand::Sync(args) => sync(args).await,
        VendorCommand::Check(args) => check(args).await,
        VendorCommand::Prune { path } => prune(&path),
    }
}

async fn sync(args: VendorRenderArgs) -> Result<()> {
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let writer = DirectoryVendorWriter::from_config(&inventory.project_config)?;
    let targets = selected_targets(&inventory, &args.target)?;
    let cache = GitOpsCache::new(&inventory.project_root, CacheMode::Default)?.with_vendor_population(args.refresh);
    let _reporter = cache.reporter();
    compile_targets(&inventory, &targets, &cache, args.progress).await?;
    let result = writer.sync(&cache.observed_artifacts(), !args.target.is_empty())?;
    let count = result.artifacts.to_string().cyan().bold();
    println!(
        "✓ Vendor snapshot contains {count} artifacts ({} written, {} reused)",
        result.written, result.reused
    );
    Ok(())
}

async fn check(args: VendorCheckArgs) -> Result<()> {
    let inventory = discover_gitops_inventory(&args.path, None)?;
    let writer = DirectoryVendorWriter::from_config(&inventory.project_config)?;
    let targets = selected_targets(&inventory, &args.target)?;
    let cache = GitOpsCache::new(&inventory.project_root, CacheMode::Default)?.with_vendor_check();
    compile_targets(&inventory, &targets, &cache, args.progress).await?;
    writer.check(&cache.observed_artifacts(), args.target.is_empty())?;
    let count = cache.observed_artifacts().len().to_string().cyan().bold();
    println!("✓ Vendor snapshot is complete and valid ({count} artifacts)");
    Ok(())
}

fn prune(path: &Path) -> Result<()> {
    let inventory = discover_gitops_inventory(path, None)?;
    let writer = DirectoryVendorWriter::from_config(&inventory.project_config)?;
    let removed = writer.prune()?;
    println!("✓ Pruned {removed} unreferenced vendor artifact(s)");
    Ok(())
}

async fn compile_targets(
    inventory: &crate::gitops::GitOpsInventory,
    targets: &[String],
    cache: &GitOpsCache,
    progress: TreeProgressArgs,
) -> Result<()> {
    for target in targets {
        let mut observer = TreeProgressReporter::new(progress, (targets.len() > 1).then(|| target.clone()));
        compile_target_tree_cached_with_observer(inventory, target, cache, &mut observer).await?;
    }
    Ok(())
}

fn selected_targets(inventory: &crate::gitops::GitOpsInventory, requested: &[String]) -> Result<Vec<String>> {
    let available = inventory
        .resources
        .values()
        .filter(|resource| resource.identity.kind == GitOpsResourceKind::DeploymentTarget)
        .map(|resource| resource.identity.name.clone())
        .collect::<BTreeSet<_>>();
    if available.is_empty() {
        return Err(NylError::config(
            "Vendor discovery requires at least one DeploymentTarget",
        ));
    }
    if requested.is_empty() {
        return Ok(available.into_iter().collect());
    }
    let requested = requested.iter().cloned().collect::<BTreeSet<_>>();
    let missing = requested.difference(&available).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(NylError::config(format!(
            "Unknown DeploymentTarget(s): {}",
            missing.join(", ")
        )));
    }
    Ok(requested.into_iter().collect())
}
