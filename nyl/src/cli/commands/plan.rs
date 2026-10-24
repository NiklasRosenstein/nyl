//! `nyl plan -e <env>`: what a reconcile would do, without the lease and
//! without writing anything.
//!
//! Contract: [Planning](../../../../design/orchestration-core.md#planning).
//! Exit categories: 0 complete; 2 a unit is blocked or waits; 3 a driver
//! plan failed; 1 error, or `--fail-on-leaving` with a unit leaving.

use std::path::PathBuf;

use clap::Args;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::Result;

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
    let _ = (args, wiring);
    todo!("cli-integration")
}
