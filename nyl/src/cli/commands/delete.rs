use std::fs;
use std::path::Path;

use clap::{Args, Subcommand};

use crate::cli::resource_file::{atomic_replace, document_count, remove_document};
use crate::gitops::{discover_gitops_inventory, validate_gitops_inventory, GitOpsInventoryKey};
use crate::resources::GitOpsResourceKind;
use crate::util::path_for_display;
use crate::{NylError, Result};

/// Remove a GitOps resource from project source.
#[derive(Args, Debug)]
pub struct DeleteArgs {
    #[command(subcommand)]
    command: DeleteCommand,
}

#[derive(Subcommand, Debug)]
enum DeleteCommand {
    /// Delete a Git repository declaration.
    Repository(DeleteResourceArgs),
    /// Delete a cluster declaration.
    Cluster(DeleteResourceArgs),
    /// Delete an Argo CD instance declaration.
    #[command(name = "argocd-instance", alias = "argocd")]
    ArgoCDInstance(DeleteResourceArgs),
    /// Delete a deployment target declaration.
    Target(DeleteResourceArgs),
    /// Delete an AppProject definition.
    #[command(name = "app-project")]
    AppProject(DeleteResourceArgs),
    /// Delete an application group declaration.
    ApplicationGroup(DeleteResourceArgs),
}

#[derive(Args, Debug)]
struct DeleteResourceArgs {
    name: String,
    /// Show the source document without modifying it.
    #[arg(long)]
    dry_run: bool,
}

pub fn execute(args: DeleteArgs) -> Result<()> {
    let (kind, args) = match args.command {
        DeleteCommand::Repository(args) => (GitOpsResourceKind::GitRepository, args),
        DeleteCommand::Cluster(args) => (GitOpsResourceKind::Cluster, args),
        DeleteCommand::ArgoCDInstance(args) => (GitOpsResourceKind::ArgoCDInstance, args),
        DeleteCommand::Target(args) => (GitOpsResourceKind::DeploymentTarget, args),
        DeleteCommand::AppProject(args) => (GitOpsResourceKind::AppProjectDefinition, args),
        DeleteCommand::ApplicationGroup(args) => (GitOpsResourceKind::ApplicationGroup, args),
    };
    delete_resource(kind, &args.name, args.dry_run)
}

fn delete_resource(kind: GitOpsResourceKind, name: &str, dry_run: bool) -> Result<()> {
    let inventory = discover_gitops_inventory(Path::new("."), None)?;
    let key = GitOpsInventoryKey::new(kind, name);
    let discovered = inventory
        .resources
        .get(&key)
        .ok_or_else(|| NylError::config(format!("{} {name:?} was not found", kind.as_str())))?;
    let path = inventory.project_root.join(&discovered.source_path);
    let mut remaining = inventory.clone();
    remaining.resources.remove(&key);
    validate_gitops_inventory(&remaining).map_err(|error| {
        NylError::config(format!(
            "Cannot delete {} {name:?} because the remaining project is invalid: {error}",
            kind.as_str()
        ))
    })?;
    if dry_run {
        println!(
            "Would delete {} {:?} from {} document {}",
            kind.as_str(),
            name,
            path_for_display(&path).display(),
            discovered.document_index
        );
        return Ok(());
    }

    let contents = fs::read_to_string(&path)?;
    let count = document_count(&contents);
    let primary = inventory.project_root.join("gitops.yaml");
    let updated = remove_document(&contents, discovered.document_index, &discovered.raw_document)?;
    atomic_replace(&path, &contents, &updated)?;
    if count == 1 && path != primary {
        fs::remove_file(&path)?;
    }
    println!(
        "✓ Deleted {} {:?} from {}",
        kind.as_str(),
        name,
        path_for_display(&path).display()
    );
    Ok(())
}
