pub mod commands;
pub mod filter;
pub(crate) mod namespace_resolution;

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

    /// Render a GitOps target into a deterministic manifest tree
    RenderTree(commands::render_tree::RenderTreeArgs),

    /// Render and compare-and-swap publish a GitOps target revision
    PublishTree(commands::publish_tree::PublishTreeArgs),

    /// Inspect rendered GitOps targets
    Target(commands::target::TargetArgs),

    /// Manage remote GitOps source locks
    Source(commands::source::SourceArgs),

    /// Show diff between rendered manifests and cluster state
    Diff(commands::diff::DiffArgs),

    /// Diff a rendered GitOps target against published or source-derived state
    DiffTree(commands::diff_tree::DiffTreeArgs),

    /// Apply rendered manifests to the cluster
    Apply(commands::apply::ApplyArgs),

    /// Generate configurations (ArgoCD, etc.)
    Generate(commands::generate::GenerateArgs),

    /// Create a new nyl project
    New(commands::new::NewArgs),

    /// Validate project configuration
    Validate(commands::validate::ValidateArgs),

    /// Inspect and update configured Kubernetes clusters
    Cluster(commands::cluster::ClusterArgs),

    /// Manage releases
    Release(commands::release::ReleaseArgs),
}

impl Cli {
    /// Return the render input path if this invocation is `nyl render`.
    pub fn render_input_path(&self) -> Option<&str> {
        match &self.command {
            Commands::Render(args) => Some(args.common.path.as_str()),
            _ => None,
        }
    }

    /// Execute the CLI command
    pub async fn execute(self) -> Result<()> {
        match self.command {
            Commands::Render(args) => commands::render::execute(args).await,
            Commands::RenderTree(args) => commands::render_tree::execute(args).await,
            Commands::PublishTree(args) => commands::publish_tree::execute(args).await,
            Commands::Target(args) => commands::target::execute(args),
            Commands::Source(args) => commands::source::execute(args),
            Commands::Diff(args) => commands::diff::execute(args).await,
            Commands::DiffTree(args) => commands::diff_tree::execute(args).await,
            Commands::Apply(args) => commands::apply::execute(args).await,
            Commands::Generate(args) => commands::generate::execute(args),
            Commands::New(args) => commands::new::execute(args).await,
            Commands::Validate(args) => commands::validate::execute(args).await,
            Commands::Cluster(args) => commands::cluster::execute(args).await,
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

    /// Check if ANSI colors should be used based on this choice
    /// This is used for tracing_subscriber configuration
    pub fn should_use_ansi(&self) -> bool {
        match self {
            ColorChoice::Auto => {
                // Check if stderr is a TTY (tracing writes to stderr)
                std::io::IsTerminal::is_terminal(&std::io::stderr())
            }
            ColorChoice::Always => true,
            ColorChoice::Never => false,
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
        // Save current state
        let initial = colored::control::SHOULD_COLORIZE.should_colorize();

        ColorChoice::Always.apply();
        // When set to always, colored output should be enabled
        assert!(colored::control::SHOULD_COLORIZE.should_colorize());

        // Restore to auto to avoid interfering with other tests
        colored::control::unset_override();

        // Best effort restoration - may not be exact if initial was based on TTY
        let _ = initial;
    }

    #[test]
    fn test_color_choice_apply_never() {
        // Save current state
        let initial = colored::control::SHOULD_COLORIZE.should_colorize();

        ColorChoice::Never.apply();
        // When set to never, colored output should be disabled
        assert!(!colored::control::SHOULD_COLORIZE.should_colorize());

        // Restore to auto to avoid interfering with other tests
        colored::control::unset_override();

        // Best effort restoration - may not be exact if initial was based on TTY
        let _ = initial;
    }

    #[test]
    fn test_color_choice_apply_auto() {
        ColorChoice::Auto.apply();
        // When set to auto, the result depends on TTY detection
        // We just verify it doesn't panic and respects the default behavior
        let _ = colored::control::SHOULD_COLORIZE.should_colorize();
    }
}
