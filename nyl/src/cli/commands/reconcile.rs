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
use nyl_orchestration::shell::local::LocalStore;
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
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct ReconcileArgs {
    /// The environment to reconcile.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Execute only these units (repeatable, or comma-separated with --units);
    /// resolution still covers the whole environment.
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    /// Units a wave runs at once.
    #[arg(long, default_value_t = nyl_orchestration::shell::run::DEFAULT_CONCURRENCY)]
    pub concurrency: usize,

    /// Render units at this revision or commit instead of the Environment's source, for this run only.
    #[arg(long)]
    pub source: Option<String>,

    /// Wait up to this long for another run's lease, such as 10m.
    #[arg(long)]
    pub wait_lease: Option<Duration>,

    /// Run next to remote state without publishing it: continue from this
    /// clone's local state branches and commit there, marked Nyl-Local.
    #[arg(long)]
    pub local: bool,

    /// With --local, take this clone's own lease instead of the remote one,
    /// accepting that CI may run at the same time.
    #[arg(long, requires = "local")]
    pub local_lease: bool,

    /// With --local, execute this unit although its recovery policy is not
    /// converge, recorded with the local approval identity (repeatable, or
    /// comma-separated). Other such units wait for approval.
    #[arg(long = "approve", value_name = "UNIT", requires = "local", value_delimiter = ',')]
    pub approve: Vec<String>,

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
    Ok(reconcile(args, wiring)
        .unwrap_or_else(|stop| stopped(ReportKind::ReconcileReport, &args.environment, args.output, stop)))
}

fn reconcile(args: &ReconcileArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment)?;
    // Reconcile writes state, so it alone of the run commands needs an identity.
    let identity = wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?;
    let local = if args.local {
        Some(session.local_run(args.local_lease, &args.approve)?)
    } else {
        None
    };
    let request = session.request(&RunOptions {
        units: &args.units,
        concurrency: args.concurrency,
        source: args.source.as_deref(),
        wait: args.wait_lease,
        local: local.clone(),
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
    let store = session.store(Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    }))?;
    // A `--local` run works on the clone's local branches through the same shell.
    let local_store = local.map(|local| LocalStore::new(&store, local));
    let state = local_store.as_ref().map_or(&store as &dyn StateStore, |local| local);
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
