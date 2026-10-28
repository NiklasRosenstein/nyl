pub(crate) mod apply_output;
pub mod commands;
pub mod filter;
pub mod git_auth;
pub(crate) mod namespace_resolution;
pub(crate) mod resource_file;
mod signals;
pub(crate) mod table;
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
    /// Build a unit artifact without orchestration state writes
    Build(commands::build::BuildArgs),
    /// Run the native operator tool of a held unit
    Exec(commands::exec::ExecArgs),
    /// Maintain sticky pull request and merge request comments
    Comment(commands::comment::CommentArgs),

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

    /// Capture live cluster API contracts into project source
    Capture(commands::capture::CaptureArgs),

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

    /// Create, move, and forget an environment's orchestration state
    State(commands::state::StateArgs),

    /// Manage an environment's run lease
    Lease(commands::state::LeaseArgs),

    /// Show what a reconcile of an environment would do
    Plan(commands::plan::PlanArgs),

    /// Execute an environment's ready units
    Reconcile(commands::reconcile::ReconcileArgs),

    /// Show an environment's units, lease, and runs
    Status(commands::status::StatusArgs),

    /// Verify an environment's current receipts with their drivers
    Verify(commands::verify::VerifyArgs),

    /// Clear a unit's uncertain or failed condition, or accept a checkpointed result
    Recover(commands::recover::RecoverArgs),

    /// Tear units down, replacing a unit that is still selected
    Teardown(commands::teardown::TeardownArgs),

    /// Freeze a unit: reconcile changes nothing about it until resumed
    Hold(commands::hold::HoldArgs),

    /// Lift a unit's hold
    Resume(commands::hold::ResumeArgs),
}

impl Cli {
    /// Execute the CLI command
    pub async fn execute(self) -> Result<()> {
        match self.command {
            Commands::Comment(args) => commands::comment::execute(args).await,
            Commands::Render(args) => commands::render::execute(args).await,
            Commands::RenderTree(args) => commands::render_tree::execute(args).await,
            Commands::PublishTree(args) => commands::publish_tree::execute(args).await,
            Commands::Create(args) => commands::create::execute(args),
            Commands::Capture(args) => commands::capture::execute(args).await,
            Commands::Update(args) => commands::update::execute(args),
            Commands::Delete(args) => commands::delete::execute(args),
            Commands::Diff(args) => commands::diff::execute(args).await,
            Commands::DiffTree(args) => Box::pin(commands::diff_tree::execute_with_color(args, self.color)).await,
            Commands::Apply(args) => commands::apply::execute(args).await,
            Commands::Schema(args) => commands::schema::execute(args).await,
            Commands::Init(args) => commands::init::execute(*args).await,
            Commands::Validate(args) => commands::validate::execute(args).await,
            Commands::Release(args) => commands::release::execute(args).await,
            Commands::Vendor(args) => commands::vendor::execute(args).await,
            Commands::Exec(_)
            | Commands::Build(_)
            | Commands::Get(_)
            | Commands::State(_)
            | Commands::Lease(_)
            | Commands::Plan(_)
            | Commands::Reconcile(_)
            | Commands::Status(_)
            | Commands::Verify(_)
            | Commands::Recover(_)
            | Commands::Teardown(_)
            | Commands::Hold(_)
            | Commands::Resume(_) => {
                let cancellation = nyl_orchestration::shell::run::Cancellation::new();
                // reconcile, verify, and recover end cleanly on SIGINT or
                // SIGTERM instead of dying with the lease held.
                let handler = self
                    .cancels_on_signal()
                    .then(|| tokio::spawn(signals::cancel_on_signal(cancellation.clone())));
                // Drivers may block on runtimes of their own, so orchestration
                // runs on a thread outside this command line's async runtime,
                // which keeps handling signals meanwhile.
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let worker = std::thread::spawn(move || {
                    let outcome = (|| -> Result<crate::orchestration::run::CommandOutcome> {
                        let owned = crate::orchestration::run::ProcessWiring::cancelled_by(cancellation);
                        let wiring = crate::orchestration::run::Wiring::from_process(&owned)?;
                        self.execute_in_process(&wiring)
                    })();
                    let _ = sender.send(outcome);
                });
                let Ok(outcome) = receiver.await else {
                    // The worker dropped the sender without sending: it panicked.
                    match worker.join() {
                        Err(panic) => std::panic::resume_unwind(panic),
                        Ok(()) => unreachable!("the worker sends its outcome before it returns"),
                    }
                };
                if let Some(handler) = handler {
                    handler.abort();
                }
                let outcome = outcome?;
                print!("{}", outcome.stdout);
                eprint!("{}", outcome.stderr);
                if outcome.exit != 0 {
                    std::process::exit(i32::from(outcome.exit));
                }
                Ok(())
            }
        }
    }

    /// Whether the command cancels on SIGINT and SIGTERM ("Runs and leases"
    /// item 7): the commands that take the lease to execute, verify, or
    /// recover units, and the state operations, whose `--wait-lease` a
    /// signal stops. `lease break` waits for nothing; other commands keep the
    /// default disposition.
    fn cancels_on_signal(&self) -> bool {
        matches!(
            self.command,
            Commands::Exec(_)
                | Commands::Build(_)
                | Commands::Reconcile(_)
                | Commands::Verify(_)
                | Commands::Recover(_)
                | Commands::State(_)
                | Commands::Teardown(_)
                | Commands::Hold(_)
                | Commands::Resume(_)
        )
    }

    /// Run an orchestration command (`plan`, `reconcile`, `status`,
    /// `verify`, `recover`, `get`, `state`, `lease`) with injected dependencies, returning its outcome
    /// instead of printing and exiting. The tier-1 scenario harness calls
    /// this. An orchestration command that fails before producing its report,
    /// such as on a project that cannot be discovered, is an outcome with
    /// exit 1; other commands are an error.
    pub fn execute_in_process(
        self,
        wiring: &crate::orchestration::run::Wiring<'_>,
    ) -> Result<crate::orchestration::run::CommandOutcome> {
        let result = match self.command {
            Commands::Build(args) => commands::build::run(&args, wiring),
            Commands::Exec(args) => commands::exec::run(&args, wiring),
            Commands::Plan(args) => commands::plan::run(&args, wiring),
            Commands::Reconcile(args) => commands::reconcile::run(&args, wiring),
            Commands::Status(args) => commands::status::run(&args, wiring),
            Commands::Get(args) => commands::get::run(&args, wiring),
            Commands::State(args) => commands::state::run_state(&args, wiring),
            Commands::Lease(args) => commands::state::run_lease(&args, wiring),
            Commands::Verify(args) => commands::verify::run(&args, wiring),
            Commands::Recover(args) => commands::recover::run(&args, wiring),
            Commands::Teardown(args) => commands::teardown::run(&args, wiring),
            Commands::Hold(args) => commands::hold::run_hold(&args, wiring),
            Commands::Resume(args) => commands::hold::run_resume(&args, wiring),
            _ => {
                return Err(crate::NylError::config(
                    "only plan, reconcile, status, verify, recover, teardown, hold, resume, get, state, and lease run in-process",
                ))
            }
        };
        Ok(result.unwrap_or_else(crate::orchestration::run::CommandOutcome::error))
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
