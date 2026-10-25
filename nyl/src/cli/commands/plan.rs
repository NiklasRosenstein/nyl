//! `nyl plan -e <env>`: what a reconcile would do, without the lease and
//! without writing anything.
//!
//! Contract: [Planning](../../../../design/orchestration-core.md#planning).
//! Exit categories: 0 complete; 2 a unit is blocked or waits; 3 a driver
//! plan failed; 1 error, or `--fail-on-leaving` with a unit leaving.

use std::path::PathBuf;

use clap::Args;

use super::reconcile::RunOutput;
use nyl_orchestration::run_report::{ExitCategory, ReportKind};
use nyl_orchestration::shell::run::RunContext;

use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use crate::{NylError, Result};

/// Plan an environment's run.
#[derive(Args, Debug)]
pub struct PlanArgs {
    /// The environment to plan.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Report only these units; the leaving section is never filtered.
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    /// Render units at this revision or commit instead of the Environment's source.
    #[arg(long)]
    pub source: Option<String>,

    /// Exit 1 when a unit leaves the ownership set.
    #[arg(long)]
    pub fail_on_leaving: bool,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &PlanArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    Ok(plan(args, wiring).unwrap_or_else(|stop| stopped(ReportKind::PlanReport, &args.environment, args.output, stop)))
}

fn plan(args: &PlanArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment)?;
    let request = session.request(&RunOptions {
        units: &args.units,
        concurrency: nyl_orchestration::shell::run::DEFAULT_CONCURRENCY,
        source: args.source.as_deref(),
        wait: None,
    })?;
    // A plan writes nothing, so it reads through a read-only store and needs
    // no commit identity.
    let store = session.store(None)?;
    let sources = session.sources()?;
    let renderer = session.renderer();
    let workspace = tempfile::TempDir::new().map_err(NylError::from)?;
    let base = session.context(&store, wiring, None);
    let context = RunContext {
        base: &base,
        sources: &sources,
        renderer: &renderer,
        drivers: wiring.drivers,
        tools: wiring.tools,
        faults: wiring.faults,
        workspace: workspace.path(),
        process_env: &wiring.process_env,
        commit_identity: None,
    };
    let mut report = nyl_orchestration::shell::run::plan(&context, &request);
    session.add_warnings(&mut report);
    let mut exit = report.exit();
    let leaving = report
        .environments
        .iter()
        .any(|environment| !environment.leaving.is_empty());
    if args.fail_on_leaving && leaving {
        exit = exit.most_severe(ExitCategory::Error);
    }
    Ok(outcome(&report, args.output, exit.code()))
}
