use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;

use crate::{
    kubernetes::{KubernetesReleaseStorage, ReleaseStatus, ReleaseStorage},
    NylError, Result,
};

/// Delete release(s)
#[derive(Args, Debug)]
pub struct DeleteArgs {
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

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

pub async fn execute(args: DeleteArgs) -> Result<()> {
    if args.names.is_empty() {
        return Err(NylError::Config("No release names specified".to_string()));
    }

    // Create Kubernetes client
    let config = if let Some(ctx) = &args.context {
        let kubeconfig = kube::config::Kubeconfig::read()?;
        kube::Config::from_custom_kubeconfig(
            kubeconfig,
            &kube::config::KubeConfigOptions {
                context: Some(ctx.clone()),
                ..Default::default()
            },
        )
        .await?
    } else {
        kube::Config::infer().await?
    };
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
            !args.no_purge,
        )
        .await?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn delete_release(
    storage: &KubernetesReleaseStorage,
    client: kube::Client,
    name: &str,
    namespace: &str,
    revision: Option<u32>,
    skip_confirm: bool,
    dry_run: bool,
    purge: bool,
) -> Result<()> {
    // Get revisions to delete and the relevant release state
    let (revisions, relevant_release): (Vec<u32>, Option<crate::kubernetes::ReleaseState>) = if let Some(rev) = revision
    {
        // Check if the specified revision exists
        let release_opt: Option<crate::kubernetes::ReleaseState> = storage.get_release(name, namespace, rev).await?;
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
        let latest_release: Option<crate::kubernetes::ReleaseState> =
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

    if purge {
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
    if purge {
        if let Some(release) = &relevant_release {
            purge_resources(client, release).await?;
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

async fn purge_resources(client: kube::Client, release: &crate::kubernetes::ReleaseState) -> Result<()> {
    use crate::kubernetes::{KubeClient, KubeRsClient};

    println!("Purging resources from cluster...");

    // Create KubeRsClient from the provided client
    let kube_client = KubeRsClient::from_client(client).await?;

    let mut deleted = 0;
    let mut failed = 0;

    for key in &release.resource_keys {
        let namespace = key.namespace.as_deref();
        let display_ns = namespace.unwrap_or("default");

        match kube_client.delete_resource(&key.gvk, namespace, &key.name).await {
            Ok(()) => {
                println!("  {} Deleted {} {}/{}", "✓".green(), key.gvk.kind, display_ns, key.name);
                deleted += 1;
            }
            Err(e) => {
                println!(
                    "  {} Failed to delete {} {}/{}: {}",
                    "✗".red(),
                    key.gvk.kind,
                    display_ns,
                    key.name,
                    e
                );
                failed += 1;
            }
        }
    }

    if failed > 0 {
        println!("{} Purged {} resource(s), {} failed", "⚠".yellow(), deleted, failed);
    } else {
        println!("{} Purged {} resource(s)", "✓".green(), deleted);
    }

    Ok(())
}
