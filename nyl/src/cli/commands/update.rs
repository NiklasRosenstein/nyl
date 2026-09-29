use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::cli::commands::source;
use crate::Result;

/// Refresh derived information stored in project resources.
#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[command(subcommand)]
    command: UpdateCommand,
}

#[derive(Subcommand, Debug)]
enum UpdateCommand {
    /// Resolve mutable revisions and update the commit locks of remote
    /// ApplicationGroup sources and `fromGit` Release input bindings.
    SourceLocks(SourceLockArgs),
}

#[derive(Args, Debug)]
struct SourceLockArgs {
    /// ApplicationGroup name. Without it and without `--target`, every lock is updated.
    group: Option<String>,
    /// Update the `fromGit` locks of this DeploymentTarget's Release input bindings.
    #[arg(long)]
    target: Option<String>,
    /// Project directory or a path beneath it.
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Report stale locks without modifying files.
    #[arg(long)]
    check: bool,
}

pub fn execute(args: UpdateArgs) -> Result<()> {
    match args.command {
        UpdateCommand::SourceLocks(args) => {
            source::update_locks(&args.path, args.group.as_deref(), args.target.as_deref(), args.check)
        }
    }
}
