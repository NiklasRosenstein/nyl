//! `nyl teardown -e <env> (--unit <u>… | --all) [--hold] [--confirm-removed]`:
//! tear units down, replacing a unit still in the ownership set, holding
//! them afterwards with `--hold` (and `--all`, for units still in the
//! ownership set), and ending a publication's teardown wait on the
//! operator's word with `--confirm-removed`. A held unit is not torn down:
//! the request is recorded on it and carried out after `nyl resume`.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Explicit teardown", [Instances](../../../../design/orchestration-core.md#instances),
//! and [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//! Exit categories as for `reconcile`: 0 every named unit's deletion
//! completed, or its request was recorded on a held unit; 2 a teardown waits (`--allow-dependents`, a dependent, a
//! teardown wait, readiness, an approval) or the lease is held; 3 a teardown
//! failed; 4 uncertain, the lease was lost, or cancelled; 1 error.

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;

use nyl_orchestration::run_report::ReportKind;
use nyl_orchestration::shell::run::{LifecycleOptions, RunMode, TeardownTarget};
use nyl_state::git::CommitAuthor;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, unit_names, RunOptions, Session, Stop};
use crate::Result;

/// Tear units down.
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
#[command(group(clap::ArgGroup::new("target").required(true).args(["units", "all"])))]
pub struct TeardownArgs {
    /// Manual teardown approvals and their audit identity.
    #[command(flatten)]
    pub approval: crate::orchestration::approval::ApprovalArgs,
    /// The environment.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Tear down these units (repeatable, or comma-separated with --units).
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    /// Tear down every unit, dependents first, and hold the units still in
    /// the ownership set afterwards; kinds that cannot tear down and
    /// deletionPolicy Retain units are released.
    #[arg(long)]
    pub all: bool,

    /// Hold the units after their teardown, in the same operation.
    #[arg(long)]
    pub hold: bool,

    /// Start no further teardown after the first failure, uncertain outcome,
    /// or approval wait; what finished is still recorded.
    #[arg(long)]
    pub fail_fast: bool,

    /// Confirm that a publication's workloads are gone, ending its teardown
    /// wait: directly, or by signalling the run waiting on it.
    #[arg(long)]
    pub confirm_removed: bool,

    /// Tear down units that retained tombstones still cite.
    #[arg(long)]
    pub allow_dependents: bool,

    /// Proceed despite unmet readiness requirements or an unfinished
    /// teardown wait, recording what may remain.
    #[arg(long)]
    pub allow_incomplete: bool,

    /// Take the units' source files from this revision when their recorded
    /// source commit cannot be fetched; recorded as an override.
    #[arg(long)]
    pub source: Option<String>,

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
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &TeardownArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    Ok(teardown(args, wiring)
        .unwrap_or_else(|stop| stopped(ReportKind::TeardownReport, &args.environment, args.output, stop)))
}

/// What `--unit` or `--all` names.
fn target(args: &TeardownArgs) -> std::result::Result<TeardownTarget, Stop> {
    if args.all {
        Ok(TeardownTarget::All)
    } else {
        Ok(TeardownTarget::Units(unit_names("--unit", &args.units)?))
    }
}

fn teardown(args: &TeardownArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment)?;
    let identity = wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?;
    let mut request = session.request(&RunOptions {
        // The units are the mode's target; a teardown run selects nothing else.
        units: &[],
        concurrency: nyl_orchestration::shell::run::DEFAULT_CONCURRENCY,
        source: args.source.as_deref(),
        wait: args.wait_lease,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode: RunMode::Teardown { target: target(args)? },
        lifecycle: LifecycleOptions {
            allow_dependents: args.allow_dependents,
            allow_incomplete: args.allow_incomplete,
            confirm_removed: args.confirm_removed,
            hold: args.hold,
            ..LifecycleOptions::default()
        },
        reapply: false,
        fail_fast: args.fail_fast,
        reason: args.reason.as_deref(),
    })?;
    let selected = if args.all {
        None
    } else {
        Some(unit_names("--unit", &args.units)?)
    };
    request.approvals = args.approval.resolve(&session, wiring, &identity, selected.as_ref())?;
    let store = session.store(Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    }))?;
    let report = session.run(
        wiring,
        &store,
        Some(&identity),
        &request,
        nyl_orchestration::shell::teardown::teardown,
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

    /// Catches a teardown that names nothing (and so would tear down
    /// nothing, or everything) being accepted: `--unit` or `--all` is
    /// required, and `--source` applies to both (its fix names `--unit`).
    #[test]
    fn test_teardown_needs_unit_or_all() {
        assert!(!parses(&["teardown", "-e", "dev"]));
        assert!(parses(&["teardown", "-e", "dev", "--unit", "api"]));
        assert!(parses(&["teardown", "-e", "dev", "--all", "--fail-fast"]));
        assert!(!parses(&["teardown", "-e", "dev", "--unit", "api", "--all"]));
        assert!(parses(&["teardown", "-e", "dev", "--unit", "api", "--source", "main"]));
    }
}
