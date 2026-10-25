//! `nyl reconcile -e <env>`: execute an environment's ready units in waves
//! and record the results in one transition commit per state ref.
//!
//! Contract: [Execution](../../../../design/orchestration-core.md#execution).
//! Exit categories: 0 all selected units current; 2 blocked, held, awaiting
//! approval, or the lease is held; 3 an execution failed; 4 uncertain or the
//! lease was lost; 1 error; the most severe wins (4 > 3 > 1 > 2 > 0).

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;

use nyl_orchestration::run_report::ReportKind;
use nyl_orchestration::shell::run::RunContext;
use nyl_state::git::CommitAuthor;

use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use crate::{NylError, Result};

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
    let request = session.request(&RunOptions {
        units: &args.units,
        concurrency: args.concurrency,
        source: args.source.as_deref(),
        wait: args.wait_lease,
        local: None,
    })?;
    if args.local {
        // Package `local` wires LocalRun into the request and wraps the store in LocalStore.
        let _ = args.local_lease;
        todo!("local")
    }
    let store = session.store(Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    }))?;
    let sources = session.sources()?;
    let renderer = session.renderer();
    let workspace = tempfile::TempDir::new().map_err(NylError::from)?;
    let base = session.context(&store, wiring, Some(identity.to_string()));
    let context = RunContext {
        base: &base,
        sources: &sources,
        renderer: &renderer,
        drivers: wiring.drivers,
        tools: wiring.tools,
        workspace: workspace.path(),
        process_env: &wiring.process_env,
        commit_identity: Some(&identity),
    };
    let mut report = nyl_orchestration::shell::run::reconcile(&context, &request);
    session.add_warnings(&mut report);
    let exit = report.exit().code();
    Ok(outcome(&report, args.output, exit))
}
