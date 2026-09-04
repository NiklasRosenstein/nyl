use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::cli::commands::{cluster, source};
use crate::Result;

/// Refresh derived information stored in project resources.
#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[command(subcommand)]
    command: UpdateCommand,
}

#[derive(Subcommand, Debug)]
enum UpdateCommand {
    /// Refresh the capabilities stored for a configured cluster.
    Cluster(cluster::ClusterUpdateArgs),
    /// Resolve mutable remote source revisions and update their commit locks.
    SourceLocks(SourceLockArgs),
}

#[derive(Args, Debug)]
struct SourceLockArgs {
    /// ApplicationGroup name. All remote groups are updated when omitted.
    group: Option<String>,
    /// Project directory or a path beneath it.
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Report stale locks without modifying files.
    #[arg(long)]
    check: bool,
}

pub async fn execute(args: UpdateArgs) -> Result<()> {
    match args.command {
        UpdateCommand::Cluster(args) => cluster::update(args).await,
        UpdateCommand::SourceLocks(args) => source::update_locks(&args.path, args.group.as_deref(), args.check),
    }
}
