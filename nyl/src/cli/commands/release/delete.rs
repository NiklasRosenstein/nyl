use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;

use crate::cli::commands::cluster::load_command_kube_config;
use nyl_render::{
    kubernetes::{
        fence::{admit, ReleaseClaim, Requester},
        KubernetesReleaseStorage, ReleaseStatus, ReleaseStorage,
    },
    NylError, Result,
};

/// Delete release(s)
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent CLI switches compose without hidden state.
pub struct DeleteArgs {
    /// Deployment target whose cluster stores the release. Optional when exactly one target is configured.
    #[arg(long)]
    pub target: Option<String>,

    /// Release name(s) to delete
    pub names: Vec<String>,

    /// Release namespace
    #[arg(short, long)]
    pub namespace: String,

    /// Delete specific revision (default: all revisions)
    #[arg(short, long)]
    pub revision: Option<u32>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    pub yes: bool,

    /// Show what would be deleted without actually deleting
    #[arg(long)]
    pub dry_run: bool,

    /// Skip deleting resources from cluster (default: purge resources)
    #[arg(long)]
    pub no_purge: bool,

    /// Also purge resources whose Argo CD sync option is `Delete=confirm`, such as managed Namespaces.
    #[arg(long, conflicts_with = "no_purge")]
    pub confirm_delete: bool,

    /// Delete a release an environment owns anyway (see `nyl apply --take-over`).
    #[arg(long)]
    pub take_over: bool,

    /// Kubernetes context: overrides the target Cluster's context, or selects a cluster when no GitOps configuration is declared
    #[arg(long)]
    pub context: Option<String>,
}

pub async fn execute(args: DeleteArgs) -> Result<()> {
    if args.names.is_empty() {
        return Err(NylError::Config("No release names specified".to_string()));
    }

    // Create Kubernetes client
    let config = load_command_kube_config(args.target.as_deref(), args.context.as_deref()).await?;
    let client = kube::Client::try_from(config)?;

    let storage = KubernetesReleaseStorage::new(client.clone());

    // Process each release
    for name in &args.names {
        delete_release(
            &storage,
            client.clone(),
            name,
            &args.namespace,
            args.revision,
            args.yes,
            args.dry_run,
            (!args.no_purge).then_some(args.confirm_delete),
            args.take_over,
        )
        .await?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn delete_release(
    storage: &KubernetesReleaseStorage,
    client: kube::Client,
    name: &str,
    namespace: &str,
    revision: Option<u32>,
    skip_confirm: bool,
    dry_run: bool,
    // `Some(confirm_delete)` purges the release's resources.
    purge: Option<bool>,
    take_over: bool,
) -> Result<()> {
    // A release an environment owns is that unit's to remove.
    admit(
        storage,
        namespace,
        name,
        &ReleaseClaim {
            requester: Requester::Manual { take_over },
            publication: None,
        },
    )
    .await?;
    // Get revisions to delete and the relevant release state
    let (revisions, relevant_release): (Vec<u32>, Option<nyl_render::kubernetes::ReleaseState>) =
        if let Some(rev) = revision {
            // Check if the specified revision exists
            let release_opt: Option<nyl_render::kubernetes::ReleaseState> =
                storage.get_release(name, namespace, rev).await?;
            if release_opt.is_none() {
                return Err(NylError::Config(format!(
                    "Release '{}' revision {} not found in namespace '{}'",
                    name, rev, namespace
                )));
            }
            (vec![rev], release_opt)
        } else {
            // Delete all revisions; ensure the release exists and get the latest state
            let all_revisions: Vec<u32> = storage.list_revisions(name, namespace).await?;
            if all_revisions.is_empty() {
                return Err(NylError::Config(format!(
                    "Release '{}' not found in namespace '{}'",
                    name, namespace
                )));
            }
            let latest_release: Option<nyl_render::kubernetes::ReleaseState> =
                storage.get_latest_release(name, namespace).await?;
            (all_revisions, latest_release)
        };

    // Determine if the relevant release is currently deployed (for warnings and purge)
    let is_deployed = relevant_release
        .as_ref()
        .is_some_and(|r| r.status == ReleaseStatus::Deployed);

    // Show summary
    println!("Deleting release '{}' in namespace '{}'", name.bold(), namespace.bold());

    if let Some(rev) = revision {
        println!("This will delete {} revision(s) and their metadata.", 1);
        println!("  Revision: {}", rev);
    } else {
        println!("This will delete {} revision(s) and their metadata.", revisions.len());
        if revisions.len() <= 5 {
            println!(
                "  Revisions: {}",
                revisions.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", ")
            );
        } else {
            println!("  Revisions: {} to {}", revisions[0], revisions[revisions.len() - 1]);
        }
    }

    if is_deployed {
        println!("{}", "⚠ Warning: This release is currently deployed!".yellow());
    }

    if purge.is_some() {
        if let Some(release) = &relevant_release {
            println!(
                "{} This will also delete {} resource(s) from the cluster.",
                "⚠".yellow(),
                release.resource_keys.len()
            );
        }
    }

    println!();

    // Dry run - just show what would happen
    if dry_run {
        println!("{}", "[DRY RUN] No changes will be made".cyan());
        return Ok(());
    }

    // Confirm unless --yes
    if !skip_confirm {
        let prompt = if is_deployed {
            format!(
                "{} Are you sure you want to delete this deployed release?",
                "⚠".yellow()
            )
        } else {
            "Are you sure?".to_string()
        };

        let confirmed = Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()
            .map_err(|e| NylError::Other(format!("Confirmation prompt failed: {}", e)))?;

        if !confirmed {
            println!("Cancelled");
            return Ok(());
        }
    }

    // Purge resources first if requested
    if let Some(confirm_delete) = purge {
        if let Some(release) = &relevant_release {
            purge_resources(client, release, confirm_delete).await?;
        }
    }

    // Delete revisions
    let mut deleted_count = 0;
    for rev in &revisions {
        storage.delete_release(name, namespace, *rev).await?;
        println!("{} Deleted revision {}", "✓".green(), rev);
        deleted_count += 1;
    }

    println!();
    println!(
        "{} Release '{}' deleted ({} revision(s) removed)",
        "✓".green(),
        name.bold(),
        deleted_count
    );

    Ok(())
}

/// Delete the release's resources, keeping those whose Argo CD `Delete` sync
/// option retains them or awaits `--confirm-delete`, in reverse apply order
/// with waits for termination.
async fn purge_resources(
    client: kube::Client,
    release: &nyl_render::kubernetes::ReleaseState,
    confirm_delete: bool,
) -> Result<()> {
    use nyl_render::kubernetes::lifecycle::{purge_resources, ApplyEvent};
    use nyl_render::kubernetes::KubeRsClient;

    println!("Purging resources from cluster...");

    let kube_client = KubeRsClient::from_client(client).await?;
    let purged = purge_resources(
        &kube_client,
        &release.resource_keys,
        &|_| false,
        crate::cli::commands::apply::DEFAULT_APPLY_CONCURRENCY.into(),
        confirm_delete,
        &mut |event| match event {
            ApplyEvent::Pruned { key, result: Ok(()) } => println!("  {} Deleted {key}", "✓".green()),
            ApplyEvent::Pruned { key, result: Err(e) } => println!("  {} Failed to delete {key}: {e}", "✗".red()),
            ApplyEvent::PruneKept { key, decision, option } => {
                let why = if decision == nyl_render::kubernetes::prune::PruneDecision::Retain {
                    format!("its sync option {option}=false retains it")
                } else {
                    format!("its sync option {option}=confirm needs --confirm-delete to delete it")
                };
                println!("  {} Kept {key}: {why}", "!".yellow());
            }
            ApplyEvent::PruneWaiting { count } => {
                println!("  … waiting for {count} deleted resource(s) to terminate");
            }
            ApplyEvent::PruneWaitTimedOut { keys } => {
                for key in keys {
                    println!("  {} {key} is still terminating; continuing", "!".yellow());
                }
            }
            _ => {}
        },
    )
    .await;

    if purged.failed.is_empty() {
        println!("{} Purged {} resource(s)", "✓".green(), purged.deleted.len());
    } else {
        println!(
            "{} Purged {} resource(s), {} failed",
            "⚠".yellow(),
            purged.deleted.len(),
            purged.failed.len()
        );
    }

    Ok(())
}
