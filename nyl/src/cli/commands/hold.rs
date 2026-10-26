//! `nyl hold -e <env> --unit <u>` and `nyl resume -e <env> --unit <u>`:
//! freeze a unit so `reconcile` changes nothing about it, and lift the hold.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit". Each writes one desired commit. Exit: 0 done or
//! unchanged, 1 error or refusal, 2 the lease is held or broken.

use clap::Args;

use super::state::Common;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::Result;

/// Freeze a unit.
#[derive(Args, Debug)]
pub struct HoldArgs {
    #[command(flatten)]
    pub common: Common,

    /// The unit to hold.
    #[arg(long)]
    pub unit: String,
}

/// Lift a hold.
#[derive(Args, Debug)]
pub struct ResumeArgs {
    #[command(flatten)]
    pub common: Common,

    /// The unit to resume.
    #[arg(long)]
    pub unit: String,
}

/// Run `nyl hold` in-process with `wiring`.
pub fn run_hold(args: &HoldArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let _ = (args, wiring);
    todo!("operator: through commands::state::run and shell::lifecycle::hold")
}

/// Run `nyl resume` in-process with `wiring`.
pub fn run_resume(args: &ResumeArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let _ = (args, wiring);
    todo!("operator: through commands::state::run and shell::lifecycle::resume")
}
