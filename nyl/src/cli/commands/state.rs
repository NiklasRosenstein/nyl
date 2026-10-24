//! `nyl state` and `nyl lease`: create, move, and forget environment state,
//! and declare a lease holder gone.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use colored::Colorize;
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::StateLocation;
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::report::{Outcome, StateOperationReport};
use nyl_orchestration::shell::state::{self, Requested};
use nyl_orchestration::shell::{Context, Settings};
use nyl_state::git::{CommitSigner, Endpoint};

use crate::gitops::discover_gitops_inventory;
use crate::orchestration::clock::process_clock;
use crate::orchestration::ids::UuidV7;
use crate::orchestration::project::{
    commit_identity, finding_error, open_store, requested_by, resolve_environment, runner,
};
use crate::{NylError, Result};

/// Output format of state commands.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub enum Output {
    /// Human-readable lines.
    #[default]
    Text,
    /// The operation's versioned report.
    Json,
}

/// Options every state-writing command shares.
#[derive(Args, Debug)]
pub struct Common {
    /// Environment whose state to change.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Why, recorded as the transition commit's Nyl-Reason.
    #[arg(long)]
    pub reason: Option<String>,

    /// Wait up to this long for another run's lease, such as 10m.
    #[arg(long)]
    pub wait_lease: Option<Duration>,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: Output,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Manage an environment's orchestration state.
#[derive(Args, Debug)]
pub struct StateArgs {
    #[command(subcommand)]
    pub command: StateCommand,
}

#[derive(Subcommand, Debug)]
pub enum StateCommand {
    /// Create state at the environment's configured location.
    Init {
        #[command(flatten)]
        common: Common,
        /// Start over deliberately: every unit becomes a new incarnation and executes again.
        #[arg(long)]
        fresh: bool,
    },
    /// Move state from its old location to the configured one. The options
    /// name the old location; each defaults to the configured value.
    Move {
        #[command(flatten)]
        common: Common,
        /// Repository the state is in: a URL, a GitRepository name, or `local` for this clone's branches.
        #[arg(long)]
        from: Option<String>,
        /// Desired ref the state is on.
        #[arg(long)]
        desired_ref: Option<String>,
        /// Observed ref the state is on.
        #[arg(long)]
        observed_ref: Option<String>,
        /// Directory of the state within its refs.
        #[arg(long = "state-path")]
        state_path: Option<String>,
        /// Coordination ref prefix the leases and runs are under.
        #[arg(long)]
        coordination_refs: Option<String>,
    },
    /// Drop a retained tombstone or a pending-teardown unit without touching its resources.
    Forget {
        #[command(flatten)]
        common: Common,
        /// The unit to forget.
        #[arg(long)]
        unit: String,
    },
}

/// Manage an environment's run lease.
#[derive(Args, Debug)]
pub struct LeaseArgs {
    #[command(subcommand)]
    pub command: LeaseCommand,
}

#[derive(Subcommand, Debug)]
pub enum LeaseCommand {
    /// Declare the lease holder gone, so the next run takes over at once.
    Break {
        #[command(flatten)]
        common: Common,
    },
}

pub fn execute_state(args: StateArgs) -> Result<()> {
    match args.command {
        StateCommand::Init { common, fresh } => run(&common, None, |context, location, resolved| {
            state::init(context, &resolved.name, location, fresh, &requested(&common))
        }),
        StateCommand::Forget { common, unit } => {
            let unit = UnitName::new(&unit).map_err(|error| NylError::config(error.to_string()))?;
            run(&common, None, |context, location, resolved| {
                state::forget(context, &resolved.name, location, &unit, &requested(&common))
            })
        }
        StateCommand::Move {
            common,
            from,
            desired_ref,
            observed_ref,
            state_path,
            coordination_refs,
        } => {
            let inventory = discover_gitops_inventory(&common.path, None)?;
            // The old repository and, unless it is this clone's branches, how to reach it.
            let from = from
                .map(|from| -> Result<_> {
                    Ok(match from.as_str() {
                        StateLocation::LOCAL_REPOSITORY => (from, None),
                        name if !name.contains(':') && !name.contains('/') => {
                            let (repository, _) = inventory.resolve_git_repository(
                                Some(&crate::resources::LocalReference { name: name.to_owned() }),
                                None,
                            )?;
                            (
                                repository.repo_url.clone(),
                                Some(Endpoint {
                                    read: repository.repo_url,
                                    write: repository.publish_url,
                                }),
                            )
                        }
                        url => (
                            crate::util::credential_free_url(url),
                            Some(Endpoint {
                                read: url.to_owned(),
                                write: None,
                            }),
                        ),
                    })
                })
                .transpose()?;
            let extra = from
                .as_ref()
                .and_then(|(identity, endpoint)| endpoint.clone().map(|endpoint| (identity.clone(), endpoint)));
            run(&common, extra, |context, location, resolved| {
                let mut old = location.clone();
                if let Some((repository, _)) = &from {
                    old.repository.clone_from(repository);
                }
                if let Some(desired) = &desired_ref {
                    old.desired_ref.clone_from(desired);
                }
                if let Some(observed) = &observed_ref {
                    old.observed_ref.clone_from(observed);
                }
                if let Some(path) = &state_path {
                    path.trim_matches('/').clone_into(&mut old.path);
                }
                if let Some(prefix) = &coordination_refs {
                    old.coordination_ref_prefix.clone_from(prefix);
                }
                state::relocate(context, &resolved.name, location, &old, &requested(&common))
            })
        }
    }
}

pub fn execute_lease(args: LeaseArgs) -> Result<()> {
    match args.command {
        LeaseCommand::Break { common } => run(&common, None, |context, location, resolved| {
            state::break_lease(context, &resolved.name, location, common.reason.clone())
        }),
    }
}

fn requested(common: &Common) -> Requested {
    Requested {
        reason: common.reason.clone(),
        wait: common.wait_lease,
    }
}

/// Resolve the environment, run one state operation, print its report, and
/// exit with its category.
fn run(
    common: &Common,
    extra_endpoint: Option<(String, Endpoint)>,
    operation: impl FnOnce(
        &Context<'_>,
        &StateLocation,
        &crate::orchestration::project::ResolvedEnvironment,
    ) -> StateOperationReport,
) -> Result<()> {
    let inventory = discover_gitops_inventory(&common.path, None)?;
    let resolved = resolve_environment(&inventory, &common.environment).map_err(|finding| finding_error(&finding))?;
    let clock = process_clock().map_err(NylError::config)?;
    let identity = commit_identity(&resolved.git_dir, None).map_err(|finding| finding_error(&finding))?;
    let mut store = open_store(
        &inventory,
        &resolved,
        CommitSigner {
            identity,
            clock: clock.clone(),
        },
    )?;
    if let Some((identity, endpoint)) = extra_endpoint {
        // The configured location keeps the endpoint the project resolved for it.
        if identity != resolved.location.repository {
            store = store.with_endpoint(identity, endpoint);
        }
    }
    let context = Context {
        store: &store,
        clock: clock.as_ref(),
        ids: &UuidV7,
        runner: runner(),
        requested_by: requested_by(&resolved.git_dir),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        settings: Settings::default(),
    };
    let mut report = operation(&context, &resolved.location, &resolved);
    let mut warnings = resolved.warnings.clone();
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    print_report(&report, common.output)?;
    if report.exit != 0 {
        std::process::exit(i32::from(report.exit));
    }
    Ok(())
}

fn print_report(report: &StateOperationReport, output: Output) -> Result<()> {
    match output {
        Output::Json => println!("{}", serde_json::to_string_pretty(report)?),
        Output::Text => {
            let status = match report.outcome {
                Outcome::Done => "done".green().to_string(),
                Outcome::Unchanged => "unchanged".to_owned(),
                Outcome::Refused => "refused".red().to_string(),
            };
            println!("{} {}: {status}", report.operation, report.environment);
            for (side, commit) in &report.commits {
                println!("  {side}: {commit}");
            }
            for (key, value) in &report.details {
                let value = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                println!("  {key}: {value}");
            }
            for finding in &report.warnings {
                eprintln!(
                    "{} {}: {}\n  fix: {}",
                    "warning".yellow(),
                    finding.code,
                    finding.message,
                    finding.fix
                );
            }
            for finding in &report.errors {
                eprintln!(
                    "{} {}: {}\n  fix: {}",
                    "error".red(),
                    finding.code,
                    finding.message,
                    finding.fix
                );
            }
        }
    }
    Ok(())
}
