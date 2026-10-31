//! `nyl reconcile -e <env>`: execute an environment's ready units in waves
//! and record the results in one transition commit per state ref.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution).
//! Exit categories: 0 all selected units current; 2 blocked, held, awaiting
//! approval, or the lease is held; 3 an execution failed; 4 uncertain, the
//! lease was lost, or the run was cancelled by SIGINT or SIGTERM; 1 error; the most severe wins (4 > 3 > 1 > 2 > 0).

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;

use nyl_core::orchestration::ports::StateStore;
use nyl_orchestration::run_report::ReportKind;
use nyl_state::git::CommitAuthor;

use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use crate::Result;

/// Output format of orchestration commands.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum RunOutput {
    /// A table.
    #[default]
    Table,
    /// The typed report.
    Json,
}

/// Execute an environment's ready units.
#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct ReconcileArgs {
    /// The environment to reconcile.
    #[arg(short = 'e', long = "environment", required_unless_present_any = ["labels", "group"], conflicts_with_all = ["labels", "group"])]
    pub environment: Vec<String>,

    /// Select environments whose literal labels match key=value pairs.
    #[arg(short = 'l', long, conflicts_with_all = ["environment", "group"])]
    pub labels: Option<String>,

    /// Select members of this EnvironmentGroup.
    #[arg(short = 'g', long, conflicts_with_all = ["environment", "labels"])]
    pub group: Option<String>,

    /// Execute only these units (repeatable, or comma-separated with --units);
    /// resolution still covers the whole environment.
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    #[arg(skip)]
    pub unit_filter: bool,

    /// Units a wave runs at once.
    #[arg(long, default_value_t = nyl_orchestration::shell::run::DEFAULT_CONCURRENCY)]
    pub concurrency: usize,

    /// Render units at this revision or commit instead of the Environment's source, for this run only.
    #[arg(long)]
    pub source: Option<String>,

    /// Reconcile an immutable snapshot of Git-visible working files, including uncommitted changes.
    #[arg(long, conflicts_with_all = ["source", "labels", "group"])]
    pub worktree: bool,

    /// Wait up to this long for another run's lease, such as 10m.
    #[arg(long)]
    pub wait_lease: Option<Duration>,

    /// Operate on the configured state repository without network access.
    #[arg(long)]
    pub local: bool,

    /// Manual execution approvals and their audit identity.
    #[command(flatten)]
    pub approval: crate::orchestration::approval::ApprovalArgs,

    /// Tear down units that left the ownership set (omission) or whose kind
    /// changed, instead of leaving them pending teardown.
    #[arg(long)]
    pub allow_teardown: bool,

    /// Tear down units that retained tombstones still cite.
    #[arg(long)]
    pub allow_dependents: bool,

    /// Proceed with teardowns whose readiness requirements are unmet, or
    /// whose removal is not confirmed, recording what may remain.
    #[arg(long)]
    pub allow_incomplete: bool,

    /// Act only on units outside the ownership set: newly leaving, deleting,
    /// pending teardown, or tearing down.
    #[arg(long, conflicts_with = "units")]
    pub leaving: bool,

    /// Execute the --unit units again although their receipts are current,
    /// such as to repair drift; needs --unit and --reason.
    #[arg(long, requires_all = ["units", "reason"])]
    pub reapply: bool,

    /// Why, recorded as the transition commit's Nyl-Reason for --reapply.
    #[arg(long)]
    pub reason: Option<String>,

    /// Start no further unit after the first failure, uncertain outcome, or
    /// approval wait; what finished is still recorded.
    #[arg(long)]
    pub fail_fast: bool,

    /// Author of state commits, as "Name <email>"; defaults to Git's user.name and user.email.
    #[arg(long)]
    pub author: Option<String>,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &ReconcileArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    use crate::orchestration::multi_environment::{dispatch, select};
    let snapshot = if args.worktree {
        let capture = || -> std::result::Result<_, Stop> {
            if args.environment.len() != 1 {
                return Err(nyl_core::orchestration::codes::Finding::new(
                    nyl_core::orchestration::codes::Code::SourceUnavailable,
                    "--worktree requires exactly one explicitly named environment",
                    "use -e <environment> without a label or group selector",
                )
                .into());
            }
            Session::open(&args.path, &args.environment[0])?.snapshot_worktree(
                wiring,
                args.author.as_deref(),
                args.local,
            )
        };
        match capture() {
            Ok(snapshot) => Some(snapshot),
            Err(stop) => {
                return Ok(stopped(
                    ReportKind::ReconcileReport,
                    args.environment.first().map_or("local", String::as_str),
                    args.output,
                    stop,
                ))
            }
        }
    } else {
        None
    };
    let preflight_source = snapshot
        .as_ref()
        .map(|source| source.commit.as_str())
        .or(args.source.as_deref());
    Ok(dispatch(
        ReportKind::ReconcileReport,
        args.output,
        select(
            &args.path,
            &args.environment,
            args.labels.as_deref(),
            args.group.as_deref(),
            true,
            preflight_source,
            args.local,
            wiring.drivers,
        ),
        |environment, selected, _planned, unavailable| {
            let mut args = args.clone();
            args.environment = vec![environment.to_owned()];
            args.unit_filter = !args.units.is_empty();
            let mut prepare = || -> std::result::Result<_, Stop> {
                args.units = nyl_orchestration::rules::multi_environment::units(
                    "--unit",
                    &args.units,
                    environment,
                    selected,
                    selected.len() > 1,
                )?;
                args.approval.approve = nyl_orchestration::rules::multi_environment::units(
                    "--approve",
                    &args.approval.approve,
                    environment,
                    selected,
                    selected.len() > 1,
                )?;
                crate::orchestration::multi_environment::single_source(args.source.as_deref(), selected)?;
                reconcile(&args, wiring, unavailable, snapshot.as_ref())
            };
            (
                prepare().unwrap_or_else(|stop| stopped(ReportKind::ReconcileReport, environment, args.output, stop)),
                std::collections::BTreeSet::default(),
            )
        },
    ))
}

fn reconcile(
    args: &ReconcileArgs,
    wiring: &Wiring<'_>,
    unavailable: &std::collections::BTreeSet<nyl_core::orchestration::ids::EnvironmentName>,
    snapshot: Option<&nyl_core::orchestration::ports::WorktreeSnapshot>,
) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment[0])?;
    // Reconcile writes state, so it alone of the run commands needs an identity.
    let identity = wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?;
    let mut request = session.request(&RunOptions {
        units: &args.units,
        concurrency: args.concurrency,
        source: args.source.as_deref(),
        wait: args.wait_lease,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode: nyl_orchestration::shell::run::RunMode::Reconcile,
        lifecycle: nyl_orchestration::shell::run::LifecycleOptions {
            allow_teardown: args.allow_teardown,
            allow_dependents: args.allow_dependents,
            allow_incomplete: args.allow_incomplete,
            leaving: args.leaving,
            ..nyl_orchestration::shell::run::LifecycleOptions::default()
        },
        reapply: args.reapply,
        fail_fast: args.fail_fast,
        reason: args.reason.as_deref(),
    })?;
    if args.unit_filter && args.units.is_empty() {
        request.units = Some(std::collections::BTreeSet::default());
    }
    request.unavailable.clone_from(unavailable);
    request.worktree_source = snapshot.cloned();
    request.approvals = args
        .approval
        .resolve(&session, wiring, &identity, request.units.as_ref())?;
    let author = Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    });
    let store = if args.local {
        session.offline_store(author)?
    } else {
        session.store(author)?
    };
    let state = &store as &dyn StateStore;
    let report = session.run(
        wiring,
        state,
        Some(&identity),
        &request,
        nyl_orchestration::shell::run::reconcile,
    )?;
    let exit = report.exit().code();
    Ok(outcome(&report, args.output, exit))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// Drift decisions: `--reapply` forces named units only, and records why.
    #[test]
    fn test_reconcile_reapply_needs_unit_and_reason() {
        assert!(!parses(&["reconcile", "-e", "dev", "--reapply"]));
        assert!(!parses(&["reconcile", "-e", "dev", "--reapply", "--unit", "api"]));
        assert!(parses(&[
            "reconcile",
            "-e",
            "dev",
            "--reapply",
            "--unit",
            "api",
            "--reason",
            "repair"
        ]));
    }
}
