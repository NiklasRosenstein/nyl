pub mod commands;

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

    /// Generate configurations (ArgoCD, etc.)
    Generate(commands::generate::GenerateArgs),

    /// Create a new nyl project
    New(commands::new::NewArgs),

    /// Validate project configuration
    Validate(commands::validate::ValidateArgs),
}

impl Cli {
    /// Execute the CLI command
    pub async fn execute(self) -> Result<()> {
        match self.command {
            Commands::Render(args) => commands::render::execute(args),
            Commands::Diff(args) => commands::diff::execute(args).await,
            Commands::Apply(args) => commands::apply::execute(args).await,
            Commands::Generate(args) => commands::generate::execute(args),
            Commands::New(args) => commands::new::execute(args),
            Commands::Validate(args) => commands::validate::execute(args),
        }
    }
}
