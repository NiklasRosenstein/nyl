//! `nyl status -e <env>`: one snapshot of an environment's state, lease,
//! and runs, with blockers and suggested next actions.
//!
//! Contract: [Inspection](../../../../design/orchestration-core.md#inspection).
//! Exit categories: 0 all current; 2 blocked, held, or waiting; 3 a unit
//! failed; 4 uncertain or a run lost its lease.

use std::path::PathBuf;

use clap::Args;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::Result;

/// Show an environment's state.
#[derive(Args, Debug)]
pub struct StatusArgs {
    /// The environment to show.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &StatusArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let _ = (args, wiring);
    todo!("cli-integration")
}
