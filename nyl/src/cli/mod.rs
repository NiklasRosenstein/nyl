pub mod commands;
pub mod filter;

use clap::{Parser, Subcommand, ValueEnum};

use crate::Result;

/// When to use colored output
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum ColorChoice {
    /// Automatically detect if colors should be used (based on TTY detection)
    #[default]
    Auto,
    /// Always use colors
    Always,
    /// Never use colors
    Never,
}

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

    /// When to use colored output
    #[arg(long, value_enum, default_value = "auto", global = true)]
    pub color: ColorChoice,
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

    /// Display Kubernetes cluster version information
    ClusterInfo(commands::cluster_info::ClusterInfoArgs),

    /// Manage releases
    Release(commands::release::ReleaseArgs),
}

impl Cli {
    /// Execute the CLI command
    pub async fn execute(self) -> Result<()> {
        match self.command {
            Commands::Render(args) => commands::render::execute(args).await,
            Commands::Diff(args) => commands::diff::execute(args).await,
            Commands::Apply(args) => commands::apply::execute(args).await,
            Commands::Generate(args) => commands::generate::execute(args),
            Commands::New(args) => commands::new::execute(args),
            Commands::Validate(args) => commands::validate::execute(args),
            Commands::ClusterInfo(args) => commands::cluster_info::execute(args).await,
            Commands::Release(args) => commands::release::execute(args).await,
        }
    }
}

impl ColorChoice {
    /// Apply the color choice to the colored crate
    pub fn apply(&self) {
        match self {
            ColorChoice::Auto => {
                // Use default TTY detection from colored crate
                colored::control::unset_override();
            }
            ColorChoice::Always => {
                colored::control::set_override(true);
            }
            ColorChoice::Never => {
                colored::control::set_override(false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_choice_default() {
        let choice = ColorChoice::default();
        assert!(matches!(choice, ColorChoice::Auto));
    }

    #[test]
    fn test_color_choice_apply_always() {
        ColorChoice::Always.apply();
        // When set to always, colored output should be enabled
        assert!(colored::control::SHOULD_COLORIZE.should_colorize());
    }

    #[test]
    fn test_color_choice_apply_never() {
        ColorChoice::Never.apply();
        // When set to never, colored output should be disabled
        assert!(!colored::control::SHOULD_COLORIZE.should_colorize());
    }

    #[test]
    fn test_color_choice_apply_auto() {
        ColorChoice::Auto.apply();
        // When set to auto, the result depends on TTY detection
        // We just verify it doesn't panic and respects the default behavior
        let _ = colored::control::SHOULD_COLORIZE.should_colorize();
    }
}
