//! Explicit live reads that refresh committed API contracts.

use super::cluster;
use crate::Result;
use clap::{Args, Subcommand};

/// Capture live cluster facts into local project files.
#[derive(Debug, Args)]
pub struct CaptureArgs {
    #[command(subcommand)]
    command: CaptureCommand,
}

#[derive(Debug, Subcommand)]
enum CaptureCommand {
    /// Read Kubernetes capabilities and optional CRD schemas into the project.
    Cluster(cluster::ClusterCaptureArgs),
}

pub async fn execute(args: CaptureArgs) -> Result<()> {
    match args.command {
        CaptureCommand::Cluster(args) => cluster::capture(args).await,
    }
}
