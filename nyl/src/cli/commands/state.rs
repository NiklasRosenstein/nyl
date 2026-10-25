//! `nyl state` and `nyl lease`: create, move, forget, reset, and restore
//! environment state, and declare a lease holder gone.

use std::collections::BTreeSet;
use std::path::PathBuf;

use std::fmt::Write as _;

use clap::{Args, Subcommand, ValueEnum};
use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::state::{Operation, StateLocation};
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::report::{Outcome, StateOperationReport};
use nyl_orchestration::rules::local::Discard;
use nyl_orchestration::shell::state::{self, Requested};
use nyl_orchestration::shell::{Context, Settings};
use nyl_state::git::{CommitAuthor, Endpoint};

use crate::gitops::discover_gitops_inventory;
use crate::orchestration::project::{open_store, resolve_environment};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::write_finding;
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

    /// Author of state commits, as "Name <email>"; defaults to Git's user.name and user.email.
    #[arg(long)]
    pub author: Option<String>,

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
    /// Drop this clone's local transitions and follow the remote state again.
    Reset {
        #[command(flatten)]
        common: Common,
        /// Reset the clone's local state branches (required: reset never changes remote state).
        #[arg(long, required = true)]
        local: bool,
        /// Forget the local receipts of these units, which the remote does not reflect (repeatable, or comma-separated).
        #[arg(long, value_delimiter = ',', conflicts_with = "discard_all")]
        discard: Vec<String>,
        /// Forget every local receipt the remote does not reflect.
        #[arg(long)]
        discard_all: bool,
    },
    /// Restore remote state to an earlier commit's content with forward commits,
    /// after local transitions were pushed by hand.
    Restore {
        #[command(flatten)]
        common: Common,
        /// The state commit whose content to restore.
        #[arg(long)]
        to: String,
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

/// Run a `nyl state` command in-process with `wiring`.
pub fn run_state(args: &StateArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    match &args.command {
        StateCommand::Init { common, fresh } => {
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateInit.as_str(), None, |ctx| {
                state::init(ctx.context, &ctx.environment, ctx.location, *fresh, &requested)
            })
        }
        StateCommand::Forget { common, unit } => {
            let unit = UnitName::new(unit).map_err(|error| NylError::config(error.to_string()))?;
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateForget.as_str(), None, |ctx| {
                state::forget(ctx.context, &ctx.environment, ctx.location, &unit, &requested)
            })
        }
        StateCommand::Reset {
            common,
            local: _,
            discard,
            discard_all,
        } => {
            let discard = discard_option(discard, *discard_all)?;
            run(common, wiring, "state-reset", None, |ctx| {
                state::reset(ctx.context, &ctx.environment, ctx.location, discard.as_ref())
            })
        }
        StateCommand::Restore { common, to } => {
            let to = CommitId::new(to.trim().to_ascii_lowercase()).map_err(|error| {
                NylError::config(format!(
                    "--to {to:?} is not a full commit ID, as `git log` of the state ref prints it: {error}"
                ))
            })?;
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateRestore.as_str(), None, |ctx| {
                state::restore(ctx.context, &ctx.environment, ctx.location, &to, &requested)
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
                .as_ref()
                .map(|from| -> Result<_> {
                    Ok(match from.as_str() {
                        StateLocation::LOCAL_REPOSITORY => (from.clone(), None),
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
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateMove.as_str(), extra, |ctx| {
                let location = ctx.location;
                let mut old = location.clone();
                if let Some((repository, _)) = &from {
                    old.repository.clone_from(repository);
                }
                if let Some(desired) = desired_ref {
                    old.desired_ref.clone_from(desired);
                }
                if let Some(observed) = observed_ref {
                    old.observed_ref.clone_from(observed);
                }
                if let Some(path) = state_path {
                    path.trim_matches('/').clone_into(&mut old.path);
                }
                if let Some(prefix) = coordination_refs {
                    old.coordination_ref_prefix.clone_from(prefix);
                }
                state::relocate(ctx.context, &ctx.environment, location, &old, &requested)
            })
        }
    }
}

/// Run a `nyl lease` command in-process with `wiring`.
pub fn run_lease(args: &LeaseArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    match &args.command {
        LeaseCommand::Break { common } => run(common, wiring, "lease-break", None, |ctx| {
            state::break_lease(ctx.context, &ctx.environment, ctx.location, common.reason.clone())
        }),
    }
}

/// `--discard <unit>…` or `--discard-all` of `state reset`; `None` for neither.
fn discard_option(units: &[String], all: bool) -> Result<Option<Discard>> {
    if all {
        return Ok(Some(Discard::All));
    }
    if units.is_empty() {
        return Ok(None);
    }
    let units = units
        .iter()
        .map(|unit| UnitName::new(unit).map_err(|error| NylError::config(error.to_string())))
        .collect::<Result<BTreeSet<_>>>()?;
    Ok(Some(Discard::Units(units)))
}

/// The request of a state operation: SIGINT or SIGTERM stops its
/// `--wait-lease` ("Runs and leases" item 7); once it holds the lease it
/// finishes its single commit.
fn requested(common: &Common, wiring: &Wiring<'_>) -> Requested {
    Requested {
        reason: common.reason.clone(),
        wait: common.wait_lease,
        cancellation: wiring.cancellation.clone(),
    }
}

/// What one state operation runs against.
pub(crate) struct OperationContext<'a> {
    pub(crate) context: &'a Context<'a>,
    pub(crate) environment: EnvironmentName,
    pub(crate) location: &'a StateLocation,
}

/// Resolve the environment and the commit identity, run one state
/// operation, and render its report. Findings before the operation, such as
/// an unknown environment or a missing identity, are refusals in the same
/// report. `nyl recover` shares it.
pub(crate) fn run(
    common: &Common,
    wiring: &Wiring<'_>,
    operation_name: &'static str,
    extra_endpoint: Option<(String, Endpoint)>,
    operation: impl FnOnce(&OperationContext<'_>) -> StateOperationReport,
) -> Result<CommandOutcome> {
    let refused = |finding: Finding| {
        let environment = EnvironmentName::new(&common.environment)
            .or_else(|_| EnvironmentName::new("unknown"))
            .expect("a valid placeholder name");
        render(
            &StateOperationReport::new(environment, operation_name).refused(finding),
            common.output,
        )
    };
    let inventory = discover_gitops_inventory(&common.path, None)?;
    let resolved = match resolve_environment(&inventory, &common.environment) {
        Ok(resolved) => resolved,
        Err(finding) => return Ok(refused(finding)),
    };
    let identity = match wiring.commit_identity(&resolved.git_dir, common.author.as_deref()) {
        Ok(identity) => identity,
        Err(finding) => return Ok(refused(finding)),
    };
    let mut store = open_store(
        &inventory,
        &resolved,
        Some(CommitAuthor {
            identity: identity.clone(),
            clock: wiring.clock.clone(),
        }),
    )?;
    if let Some((repository, endpoint)) = extra_endpoint {
        // The configured location keeps the endpoint the project resolved for it.
        if repository != resolved.location.repository {
            store = store.with_endpoint(repository, endpoint);
        }
    }
    let context = Context {
        store: &store,
        clock: wiring.clock.as_ref(),
        ids: wiring.ids,
        faults: wiring.faults,
        runner: wiring.runner.clone(),
        requested_by: identity.to_string(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        settings: Settings::default(),
    };
    let mut report = operation(&OperationContext {
        context: &context,
        environment: resolved.name.clone(),
        location: &resolved.location,
    });
    let mut warnings = resolved.warnings.clone();
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    Ok(render(&report, common.output))
}

/// Render a state operation's report as the command's outcome.
fn render(report: &StateOperationReport, output: Output) -> CommandOutcome {
    let json = serde_json::to_value(report).expect("reports serialize");
    let mut stdout = String::new();
    let mut stderr = String::new();
    match output {
        Output::Json => {
            stdout = format!("{}\n", serde_json::to_string_pretty(&json).expect("JSON prints"));
        }
        Output::Text => {
            let status = match report.outcome {
                Outcome::Done => "done",
                Outcome::Unchanged => "unchanged",
                Outcome::Refused => "refused",
            };
            let _ = writeln!(stdout, "{} {}: {status}", report.operation, report.environment);
            for (side, commit) in &report.commits {
                let _ = writeln!(stdout, "  {side}: {commit}");
            }
            for (key, value) in &report.details {
                let value = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
                let _ = writeln!(stdout, "  {key}: {value}");
            }
        }
    }
    // Diagnostics go to stderr in both formats.
    for finding in &report.warnings {
        write_finding(&mut stderr, "warning", None, finding);
    }
    for finding in &report.errors {
        write_finding(&mut stderr, "error", None, finding);
    }
    CommandOutcome {
        exit: report.exit,
        report: json,
        stdout,
        stderr,
    }
}
