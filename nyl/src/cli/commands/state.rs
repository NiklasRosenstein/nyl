//! `nyl state` and `nyl lease`: create, move, forget, and delete
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
use nyl_orchestration::shell::run::{LifecycleOptions, RunMode, TeardownTarget};
use nyl_orchestration::shell::state::{self, Requested};
use nyl_orchestration::shell::Context;
use nyl_state::git::{CommitAuthor, Endpoint};

use crate::gitops::discover_gitops_inventory;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{placeholder_environment, pretty_json, write_finding, RunOptions, Session, Stop};
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
    /// Discard accepted local receipts and follow the configured remote state.
    Reset {
        #[command(flatten)]
        common: Common,
        /// Reset local state (required: reset never changes remote state).
        #[arg(long, required = true)]
        local: bool,
        /// Forget the local receipts of these units, which the remote does not reflect (repeatable, or comma-separated).
        #[arg(long, value_delimiter = ',', conflicts_with = "discard_all")]
        discard: Vec<String>,
        /// Forget every local receipt the remote does not reflect.
        #[arg(long)]
        discard_all: bool,
    },
    /// Remove the environment's state once no unit has an incarnation left;
    /// with --teardown, tear every unit down first.
    Delete {
        #[command(flatten)]
        approval: crate::orchestration::approval::ApprovalArgs,
        #[command(flatten)]
        common: Common,
        /// Run `nyl teardown --all` first and continue to the deletion in the same command.
        #[arg(long)]
        teardown: bool,
        /// With --teardown, tear down units that retained tombstones still cite.
        #[arg(long, requires = "teardown")]
        allow_dependents: bool,
        /// With --teardown, proceed despite unmet readiness requirements or
        /// unconfirmed removals, recording what may remain.
        #[arg(long, requires = "teardown")]
        allow_incomplete: bool,
        /// With --teardown, take the units' source files from this revision
        /// when the recorded source commit cannot be fetched.
        #[arg(long, requires = "teardown")]
        source: Option<String>,
    },
    /// Restore historical state content with forward commits.
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
        /// Break the lease in the local state repository without contacting the remote.
        #[arg(long)]
        local: bool,
    },
}

/// Run a `nyl state` command in-process with `wiring`.
pub fn run_state(args: &StateArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    match &args.command {
        StateCommand::Init { common, fresh } => {
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateInit.as_str(), None, false, |ctx| {
                state::init(ctx.context, &ctx.environment, ctx.location, *fresh, &requested)
            })
        }
        StateCommand::Forget { common, unit } => {
            let unit = UnitName::new(unit).map_err(|error| NylError::config(error.to_string()))?;
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateForget.as_str(), None, false, |ctx| {
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
            run(common, wiring, "state-reset", None, false, |ctx| {
                state::reset(ctx.context, &ctx.environment, ctx.location, discard.as_ref())
            })
        }
        StateCommand::Delete {
            approval,
            common,
            teardown,
            allow_dependents,
            allow_incomplete,
            source,
        } => run_delete(
            common,
            wiring,
            *teardown,
            (*allow_dependents, *allow_incomplete),
            source.as_deref(),
            approval,
        ),
        StateCommand::Restore { common, to } => {
            let to = CommitId::new(to.trim().to_ascii_lowercase()).map_err(|error| {
                NylError::config(format!(
                    "--to {to:?} is not a full commit ID, as `git log` of the state ref prints it: {error}"
                ))
            })?;
            let requested = requested(common, wiring);
            run(common, wiring, Operation::StateRestore.as_str(), None, false, |ctx| {
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
        } => run_move(
            common,
            wiring,
            from.as_ref(),
            [
                desired_ref.as_ref(),
                observed_ref.as_ref(),
                state_path.as_ref(),
                coordination_refs.as_ref(),
            ],
        ),
    }
}

/// `nyl state move`: `old` holds `--desired-ref`, `--observed-ref`,
/// `--state-path`, and `--coordination-refs`.
fn run_move(
    common: &Common,
    wiring: &Wiring<'_>,
    from: Option<&String>,
    [desired_ref, observed_ref, state_path, coordination_refs]: [Option<&String>; 4],
) -> Result<CommandOutcome> {
    let inventory = discover_gitops_inventory(&common.path, None)?;
    // The old repository and, unless it is this clone's branches, how to reach it.
    let from = from
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
    run(common, wiring, Operation::StateMove.as_str(), extra, false, |ctx| {
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

/// Run a `nyl lease` command in-process with `wiring`.
pub fn run_lease(args: &LeaseArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    match &args.command {
        LeaseCommand::Break { common, local } => run(common, wiring, "lease-break", None, *local, |ctx| {
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
pub(crate) fn requested(common: &Common, wiring: &Wiring<'_>) -> Requested {
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
    offline: bool,
    operation: impl FnOnce(&OperationContext<'_>) -> StateOperationReport,
) -> Result<CommandOutcome> {
    let refused = |finding: Finding| {
        let environment = placeholder_environment(&common.environment);
        render(
            &StateOperationReport::new(environment, operation_name).refused(finding),
            common.output,
        )
    };
    let session = match Session::open(&common.path, &common.environment) {
        Ok(session) => session,
        Err(Stop::Finding(finding)) => return Ok(refused(finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let resolved = &session.resolved;
    let identity = match wiring.commit_identity(&resolved.git_dir, common.author.as_deref()) {
        Ok(identity) => identity,
        Err(finding) => return Ok(refused(finding)),
    };
    let author = Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    });
    let mut store = if offline {
        session.offline_store(author)?
    } else {
        session.store(author)?
    };
    if operation_name == "state-reset" {
        let path = store.repository_path(&resolved.location);
        store = store.with_local_view(path).allow_pending();
    }
    if operation_name == "state-init" {
        let path = store.repository_path(&resolved.location);
        let direct = resolved.endpoint.as_ref().is_some_and(|endpoint| {
            endpoint.read.starts_with("file://")
                || std::path::Path::new(&endpoint.read).is_absolute()
                || std::path::Path::new(&endpoint.read).starts_with(".")
        });
        if direct && !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            git2::Repository::init_bare(&path).map_err(|error| {
                NylError::config(format!(
                    "cannot initialize state repository {}: {error}",
                    path.display()
                ))
            })?;
        }
    }
    if let Some((repository, endpoint)) = extra_endpoint {
        // The configured location keeps the endpoint the project resolved for it.
        if repository != resolved.location.repository {
            store = crate::orchestration::project::configure_repository(
                &session.inventory,
                resolved,
                store,
                &repository,
                endpoint,
            )?;
        }
    }
    let context = session.context(&store, wiring, Some(identity.to_string()));
    let mut report = operation(&OperationContext {
        context: &context,
        environment: resolved.name.clone(),
        location: &resolved.location,
    });
    if operation_name == "state-init" {
        report.details.insert(
            "stateRepository".to_owned(),
            serde_json::json!(resolved.location.repository),
        );
        report.details.insert(
            "stateLocalPath".to_owned(),
            serde_json::json!(store.repository_path(&resolved.location).display().to_string()),
        );
    }
    let mut warnings = resolved.warnings.clone();
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    Ok(render(&report, common.output))
}

/// `nyl state delete [--teardown]`: on the run path, through
/// [`Session::run_with`], since `--teardown` runs `teardown --all` first.
fn run_delete(
    common: &Common,
    wiring: &Wiring<'_>,
    teardown: bool,
    (allow_dependents, allow_incomplete): (bool, bool),
    source: Option<&str>,
    approval: &crate::orchestration::approval::ApprovalArgs,
) -> Result<CommandOutcome> {
    // `teardown --all` holds what stays in the ownership set.
    let lifecycle = LifecycleOptions {
        allow_dependents,
        allow_incomplete,
        ..LifecycleOptions::default()
    };
    run_lifecycle(
        common,
        wiring,
        Operation::StateDelete.as_str(),
        RunMode::Teardown {
            target: TeardownTarget::All,
        },
        lifecycle,
        false,
        source,
        Some(approval),
        |context, request| nyl_orchestration::shell::teardown::state_delete(context, request, teardown),
    )
}

/// Run a state operation that needs source access and the shared run context.
#[allow(clippy::too_many_arguments)] // Shared state command wiring, with optional invocation approvals.
pub(crate) fn run_lifecycle(
    common: &Common,
    wiring: &Wiring<'_>,
    operation_name: &'static str,
    mode: RunMode,
    lifecycle: LifecycleOptions,
    offline: bool,
    source: Option<&str>,
    approval: Option<&crate::orchestration::approval::ApprovalArgs>,
    operation: impl FnOnce(
        &nyl_orchestration::shell::run::RunContext<'_>,
        &nyl_orchestration::shell::run::RunRequest,
    ) -> StateOperationReport,
) -> Result<CommandOutcome> {
    let refused = |finding: Finding| {
        let environment = placeholder_environment(&common.environment);
        render(
            &StateOperationReport::new(environment, operation_name).refused(finding),
            common.output,
        )
    };
    let session = match Session::open(&common.path, &common.environment) {
        Ok(session) => session,
        Err(Stop::Finding(finding)) => return Ok(refused(finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let identity = match wiring.commit_identity(&session.resolved.git_dir, common.author.as_deref()) {
        Ok(identity) => identity,
        Err(finding) => return Ok(refused(finding)),
    };
    let mut request = match session.request(&RunOptions {
        units: &[],
        concurrency: nyl_orchestration::shell::run::DEFAULT_CONCURRENCY,
        source,
        wait: common.wait_lease,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode,
        lifecycle,
        reapply: false,
        fail_fast: false,
        reason: common.reason.as_deref(),
    }) {
        Ok(request) => request,
        Err(finding) => return Ok(refused(finding)),
    };
    if let Some(approval) = approval {
        request.approvals = match approval.resolve(&session, wiring, &identity, None) {
            Ok(approvals) => approvals,
            Err(finding) => return Ok(refused(finding)),
        };
    }
    let author = Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    });
    let store = if offline {
        session.offline_store(author)?
    } else {
        session.store(author)?
    };
    let mut report = match session.run_with(wiring, &store, Some(&identity), &request, operation) {
        Ok(report) => report,
        Err(Stop::Finding(finding)) => return Ok(refused(finding)),
        Err(Stop::Error(error)) => return Err(error),
    };
    let mut warnings = session.resolved.warnings.clone();
    warnings.append(&mut report.warnings);
    report.warnings = warnings;
    Ok(render(&report, common.output))
}

/// Render a state operation's report as the command's outcome.
pub(crate) fn render(report: &StateOperationReport, output: Output) -> CommandOutcome {
    let json = serde_json::to_value(report).expect("reports serialize");
    let mut stdout = String::new();
    let mut stderr = String::new();
    match output {
        Output::Json => {
            stdout = pretty_json(&json);
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
                if key == "teardown" {
                    // The teardown's own report: its exit and headline, in full with -o json.
                    let exit = &value["environments"][0]["exit"];
                    let _ = writeln!(stdout, "  teardown: exit {exit}");
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// Catches `state delete` options that only mean something for the
    /// teardown being accepted without it.
    #[test]
    fn test_state_delete_teardown_options_require_teardown() {
        assert!(parses(&["state", "delete", "-e", "dev"]));
        assert!(!parses(&["state", "delete", "-e", "dev", "--allow-dependents"]));
        assert!(parses(&[
            "state",
            "delete",
            "-e",
            "dev",
            "--teardown",
            "--allow-dependents",
            "--allow-incomplete"
        ]));
    }
}
