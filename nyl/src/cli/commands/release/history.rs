use clap::Args;
use comfy_table::Cell;

use crate::{
    cli::commands::cluster::load_target_kube_config,
    cli::commands::release::{format, OutputFormat},
    kubernetes::{KubernetesReleaseStorage, ReleaseStorage},
    NylError, Result,
};

/// Show revision history for a release
#[derive(Args, Debug)]
pub struct HistoryArgs {
    /// GitOps target whose cluster stores the release
    #[arg(long)]
    pub target: String,

    /// Release name
    pub name: String,

    /// Release namespace
    #[arg(short, long)]
    pub namespace: String,

    /// Maximum revisions to show
    #[arg(long, default_value = "10")]
    pub max: usize,

    /// Output format
    #[arg(short, long, default_value = "table")]
    pub output: OutputFormat,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

pub async fn execute(args: HistoryArgs) -> Result<()> {
    // Create Kubernetes client
    let config = load_target_kube_config(&args.target, args.context.as_deref()).await?;
    let client = kube::Client::try_from(config)?;

    let storage = KubernetesReleaseStorage::new(client);

    // Get all revisions
    let revisions: Vec<u32> = storage.list_revisions(&args.name, &args.namespace).await?;

    if revisions.is_empty() {
        return Err(NylError::Config(format!(
            "Release '{}' not found in namespace '{}'",
            args.name, args.namespace
        )));
    }

    // Fetch release states for each revision (in reverse order, limited by max)
    let mut releases = Vec::new();
    for revision in revisions.iter().rev().take(args.max) {
        if let Some(release) = storage.get_release(&args.name, &args.namespace, *revision).await? {
            releases.push(release);
        }
    }

    // Output based on format
    match args.output {
        OutputFormat::Table | OutputFormat::Wide => {
            let mut table = format::create_table();

            table.set_header(vec!["REVISION", "STATUS", "RENDERED", "APPLIED", "RESOURCES"]);

            for release in releases {
                table.add_row(vec![
                    Cell::new(release.revision),
                    format::color_status(&release.status),
                    Cell::new(format::format_timestamp(&release.rendered_at)),
                    Cell::new(
                        release
                            .applied_at
                            .map_or_else(|| "-".to_string(), |t| format::format_timestamp(&t)),
                    ),
                    Cell::new(release.resource_keys.len()),
                ]);
            }

            println!("{}", table);
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&releases)?;
            println!("{}", json);
        }
        OutputFormat::Yaml => {
            let yaml = serde_norway::to_string(&releases)?;
            print!("{}", yaml);
        }
    }

    Ok(())
}
