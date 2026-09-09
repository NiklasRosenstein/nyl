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

pub fn execute(args: UpdateArgs) -> Result<()> {
    match args.command {
        UpdateCommand::SourceLocks(args) => source::update_locks(&args.path, args.group.as_deref(), args.check),
    }
}
