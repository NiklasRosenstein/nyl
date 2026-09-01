use clap::Args;
use comfy_table::Cell;

use crate::{
    cli::commands::cluster::load_target_kube_config,
    cli::commands::release::{format, OutputFormat, SortBy},
    kubernetes::{KubernetesReleaseStorage, ReleaseStatus, ReleaseStorage},
    Result,
};

/// List all releases
#[derive(Args, Debug)]
pub struct ListArgs {
    /// deployment target whose cluster stores the releases
    #[arg(long)]
    pub target: String,

    /// Filter by namespace (default: all namespaces)
    #[arg(short, long)]
    pub namespace: Option<String>,

    /// Output format
    #[arg(short, long, default_value = "table")]
    pub output: OutputFormat,

    /// Sort by field
    #[arg(long, default_value = "name")]
    pub sort_by: SortBy,

    /// Reverse sort order
    #[arg(short, long)]
    pub reverse: bool,

    /// Show all statuses (default: deployed and failed only)
    #[arg(short, long)]
    pub all: bool,

    /// Kubernetes context to use
    #[arg(long)]
    pub context: Option<String>,
}

pub async fn execute(args: ListArgs) -> Result<()> {
    // Create Kubernetes client
    let config = load_target_kube_config(&args.target, args.context.as_deref()).await?;
    let client = kube::Client::try_from(config)?;

    let storage = KubernetesReleaseStorage::new(client);

    // List releases
    let mut releases: Vec<crate::kubernetes::ReleaseInfo> = storage.list_releases(args.namespace.as_deref()).await?;

    // Filter by status if not --all
    if !args.all {
        releases.retain(|r| matches!(r.status, ReleaseStatus::Deployed | ReleaseStatus::Failed));
    }

    // Sort releases
    releases.sort_by(|a, b| {
        let cmp = match args.sort_by {
            SortBy::Name => a.release_name.cmp(&b.release_name),
            SortBy::Namespace => a.release_namespace.cmp(&b.release_namespace),
            SortBy::Revision => a.latest_revision.cmp(&b.latest_revision),
            SortBy::Age => a.rendered_at.cmp(&b.rendered_at),
        };

        if args.reverse {
            cmp.reverse()
        } else {
            cmp
        }
    });

    // Output based on format
    match args.output {
        OutputFormat::Table | OutputFormat::Wide => {
            if releases.is_empty() {
                println!("No releases found");
                return Ok(());
            }

            let mut table = format::create_table();

            // Add headers
            if matches!(args.output, OutputFormat::Wide) {
                table.set_header(vec![
                    "NAME",
                    "NAMESPACE",
                    "REVISION",
                    "STATUS",
                    "AGE",
                    "RESOURCES",
                    "RENDERED",
                    "APPLIED",
                ]);
            } else {
                table.set_header(vec!["NAME", "NAMESPACE", "REVISION", "STATUS", "AGE", "RESOURCES"]);
            }

            // Add rows
            for release in releases {
                let mut row = vec![
                    Cell::new(&release.release_name),
                    Cell::new(&release.release_namespace),
                    Cell::new(release.latest_revision),
                    format::color_status(&release.status),
                    Cell::new(format::format_age(&release.rendered_at)),
                    Cell::new(release.resource_count),
                ];

                if matches!(args.output, OutputFormat::Wide) {
                    row.push(Cell::new(format::format_timestamp(&release.rendered_at)));
                    row.push(Cell::new(
                        release
                            .applied_at
                            .map_or_else(|| "-".to_string(), |t| format::format_timestamp(&t)),
                    ));
                }

                table.add_row(row);
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
