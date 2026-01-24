pub mod commands;
pub mod output;

use clap::{Parser, Subcommand};

use crate::Result;

/// Nyl - Kubernetes manifest generator with Helm integration
#[derive(Parser, Debug)]
#[command(name = "nyl")]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Render Kubernetes manifests to stdout
    Render(commands::render::RenderArgs),

    /// Show diff between rendered manifests and cluster state
    Diff(commands::diff::DiffArgs),

    /// Apply rendered manifests to the cluster
    Apply(commands::apply::ApplyArgs),

    /// Create a new nyl project
    New(commands::new::NewArgs),

    /// Validate project configuration
    Validate(commands::validate::ValidateArgs),
}

impl Cli {
    /// Execute the CLI command
    pub fn execute(self) -> Result<()> {
        match self.command {
            Commands::Render(args) => commands::render::execute(args),
            Commands::Diff(args) => commands::diff::execute(args),
            Commands::Apply(args) => commands::apply::execute(args),
            Commands::New(args) => commands::new::execute(args),
            Commands::Validate(args) => commands::validate::execute(args),
        }
    }
}
