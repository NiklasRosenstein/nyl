use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::gitops::discover_gitops_inventory;
use crate::resources::GitOpsResource;
use crate::Result;

/// Inspect rendered GitOps targets.
#[derive(Args, Debug)]
pub struct TargetArgs {
    #[command(subcommand)]
    pub command: TargetSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum TargetSubcommand {
    /// List all discovered GitOpsTarget resources.
    List {
        /// Project directory or a path beneath it.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

pub fn execute(args: TargetArgs) -> Result<()> {
    match args.command {
        TargetSubcommand::List { path } => list(&path),
    }
}

fn list(path: &std::path::Path) -> Result<()> {
    let inventory = discover_gitops_inventory(path, None)?;
    let mut count = 0;
    for discovered in inventory.resources.values() {
        let Some(GitOpsResource::GitOpsTarget(target)) = &discovered.resource else {
            continue;
        };
        let publication = target
            .spec
            .publication
            .repository_ref
            .as_ref()
            .map(|reference| reference.name.as_str())
            .or_else(|| {
                target
                    .spec
                    .publication
                    .repository
                    .as_ref()
                    .map(|repository| repository.repo_url.as_str())
            })
            .expect("validated target has a publication repository");
        println!(
            "{}\t{}\t{}@{}\t{}",
            target.metadata.name,
            target.spec.cluster_ref.name,
            publication,
            target.spec.publication.revision,
            target.spec.publication.path_prefix
        );
        count += 1;
    }
    if count == 0 {
        tracing::warn!(
            "No GitOpsTarget resources found under {}",
            inventory.project_root.display()
        );
    }
    Ok(())
}
