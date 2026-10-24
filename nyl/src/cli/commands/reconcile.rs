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

use crate::orchestration::run::{CommandOutcome, Wiring};
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
    let _ = (args, wiring);
    todo!("cli-integration")
}
