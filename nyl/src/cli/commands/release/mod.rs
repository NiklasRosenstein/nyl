mod delete;
mod format;
mod history;
mod list;
mod rollback;
mod show;

use clap::{Args, Subcommand, ValueEnum};

use crate::Result;

/// Manage releases
#[derive(Args, Debug)]
pub struct ReleaseArgs {
    #[command(subcommand)]
    pub command: ReleaseSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum ReleaseSubcommand {
    /// List all releases
    List(list::ListArgs),

    /// Show details of a specific release
    Show(show::ShowArgs),

    /// Show revision history for a release
    History(history::HistoryArgs),

    /// Delete release(s)
    Delete(delete::DeleteArgs),

    /// Roll back a release to a previous revision
    Rollback(rollback::RollbackArgs),
}

/// Output format for commands
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table format
    Table,
    /// JSON format
    Json,
    /// YAML format
    Yaml,
    /// Wide table format with additional columns
    Wide,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Table
    }
}

/// Sort field for list command
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortBy {
    /// Sort by release name
    Name,
    /// Sort by namespace
    Namespace,
    /// Sort by revision number
    Revision,
    /// Sort by age (rendered timestamp)
    Age,
}

impl Default for SortBy {
    fn default() -> Self {
        Self::Name
    }
}

pub async fn execute(args: ReleaseArgs) -> Result<()> {
    match args.command {
        ReleaseSubcommand::List(args) => list::execute(args).await,
        ReleaseSubcommand::Show(args) => show::execute(args).await,
        ReleaseSubcommand::History(args) => history::execute(args).await,
        ReleaseSubcommand::Delete(args) => delete::execute(args).await,
        ReleaseSubcommand::Rollback(args) => rollback::execute(args).await,
    }
}
