//! `nyl status -e <env>`: one snapshot of an environment's state, lease,
//! and runs, with blockers and suggested next actions.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection).
//! Exit categories: 0 all current; 2 blocked, held, or waiting; 3 a unit
//! failed; 4 uncertain or a run lost its lease.

use std::path::PathBuf;

use clap::Args;

use super::reconcile::RunOutput;
use nyl_core::orchestration::ports::StateStore;
use nyl_orchestration::run_report::ReportKind;
use nyl_orchestration::shell::local::LocalStore;

use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, Session, Stop};
use crate::Result;

/// Show an environment's state.
#[derive(Args, Debug, Clone)]
pub struct StatusArgs {
    /// The environment to show.
    #[arg(short = 'e', long = "environment", required_unless_present_any = ["labels", "group"], conflicts_with_all = ["labels", "group"])]
    pub environment: Vec<String>,

    /// Select environments whose literal labels match key=value pairs.
    #[arg(short = 'l', long, conflicts_with_all = ["environment", "group"])]
    pub labels: Option<String>,

    /// Select members of this EnvironmentGroup.
    #[arg(short = 'g', long, conflicts_with_all = ["environment", "labels"])]
    pub group: Option<String>,

    /// Show this clone's local view of the state, including --local runs.
    #[arg(long)]
    pub local: bool,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &StatusArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    use crate::orchestration::multi_environment::{dispatch, select};
    Ok(dispatch(
        ReportKind::StatusReport,
        args.output,
        select(
            &args.path,
            &args.environment,
            args.labels.as_deref(),
            args.group.as_deref(),
            false,
            None,
            wiring.drivers,
        ),
        |environment, _selected, _planned, _unavailable| {
            let mut args = args.clone();
            args.environment = vec![environment.to_owned()];
            let prepare = || -> std::result::Result<_, Stop> { status(&args, wiring) };
            (
                prepare().unwrap_or_else(|stop| stopped(ReportKind::StatusReport, environment, args.output, stop)),
                std::collections::BTreeSet::default(),
            )
        },
    ))
}

fn status(args: &StatusArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment[0])?;
    // Status only reads: a read-only store, no commit identity, no lease.
    let store = session.store(None)?;
    // `--local`: the clone's local view, through the store a `--local` run uses.
    let local_store = if args.local {
        Some(LocalStore::new(&store, session.local_run(false, &[])?))
    } else {
        None
    };
    let state = local_store.as_ref().map_or(&store as &dyn StateStore, |local| local);
    let context = session.context(state, wiring, None);
    let mut report = nyl_orchestration::shell::inspect::status_with(
        &context,
        &session.resolved.name,
        &session.resolved.location,
        Some(wiring.drivers),
    );
    session.annotate_report(&mut report);
    let exit = report.exit().code();
    Ok(outcome(&report, args.output, exit))
}
