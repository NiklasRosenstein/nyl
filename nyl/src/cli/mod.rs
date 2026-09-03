pub mod commands;
pub mod filter;
pub(crate) mod namespace_resolution;
pub(crate) mod resource_file;
pub(crate) mod tree_progress;

use clap::{Parser, Subcommand, ValueEnum};

use crate::Result;

/// When to use colored output
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum ColorChoice {
    /// Use colors for terminals and conventional CI environments.
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

    /// Render a deployment target into a deterministic manifest tree
    RenderTree(commands::render_tree::RenderTreeArgs),

    /// Render and compare-and-swap publish a deployment target revision
    PublishTree(commands::publish_tree::PublishTreeArgs),

    /// Create a component or GitOps resource
    Create(commands::create::CreateArgs),

    /// Inspect GitOps resources declared in the project
    Get(commands::get::GetArgs),

    /// Refresh derived information stored in project resources
    Update(commands::update::UpdateArgs),

    /// Remove a GitOps resource from project source
    Delete(commands::delete::DeleteArgs),

    /// Show diff between rendered manifests and cluster state
    Diff(commands::diff::DiffArgs),

    /// Diff a rendered deployment target against published or source-derived state
    DiffTree(commands::diff_tree::DiffTreeArgs),

    /// Apply rendered manifests to the cluster
    Apply(commands::apply::ApplyArgs),

    /// Generate project and resource schemas
    Schema(commands::schema::SchemaArgs),

    /// Initialize a Nyl project
    Init(Box<commands::init::InitArgs>),

    /// Validate project configuration
    Validate(commands::validate::ValidateArgs),

    /// Manage releases
    Release(commands::release::ReleaseArgs),

    /// Manage project-global vendored renderer inputs
    Vendor(commands::vendor::VendorArgs),
}

impl Cli {
    /// Execute the CLI command
    pub async fn execute(self) -> Result<()> {
        match self.command {
            Commands::Render(args) => commands::render::execute(args).await,
            Commands::RenderTree(args) => commands::render_tree::execute(args).await,
            Commands::PublishTree(args) => commands::publish_tree::execute(args).await,
            Commands::Create(args) => commands::create::execute(args),
            Commands::Get(args) => commands::get::execute(args),
            Commands::Update(args) => commands::update::execute(args).await,
            Commands::Delete(args) => commands::delete::execute(args),
            Commands::Diff(args) => commands::diff::execute(args).await,
            Commands::DiffTree(args) => commands::diff_tree::execute(args).await,
            Commands::Apply(args) => commands::apply::execute(args).await,
            Commands::Schema(args) => commands::schema::execute(args),
            Commands::Init(args) => commands::init::execute(*args).await,
            Commands::Validate(args) => commands::validate::execute(args).await,
            Commands::Release(args) => commands::release::execute(args).await,
            Commands::Vendor(args) => commands::vendor::execute(args).await,
        }
    }
}

impl ColorChoice {
    /// Apply the color choice to the colored crate
    pub fn apply(&self) {
        match self {
            ColorChoice::Auto => {
                colored::control::set_override(auto_color_enabled());
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
            ColorChoice::Auto => auto_color_enabled(),
            ColorChoice::Always => true,
            ColorChoice::Never => false,
        }
    }
}

fn auto_color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some()
        || std::env::var("CLICOLOR").is_ok_and(|value| value == "0")
        || std::env::var("TERM").is_ok_and(|value| value == "dumb")
    {
        return false;
    }
    if std::env::var("CLICOLOR_FORCE").is_ok_and(|value| !value.is_empty() && value != "0") {
        return true;
    }
    std::io::IsTerminal::is_terminal(&std::io::stderr())
        || std::env::var("CI").is_ok_and(|value| !value.is_empty() && value != "0" && value != "false")
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
    fn explicit_color_choices_have_deterministic_ansi_policy() {
        assert!(ColorChoice::Always.should_use_ansi());
        assert!(!ColorChoice::Never.should_use_ansi());
    }
}
