//! `nyl recover -e <env> --unit <u> (--retry | --accept <run>)`: clear a
//! unit's uncertain condition or non-retryable failure for re-execution, or
//! adopt a result a run checkpointed, recording the operator's reason.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery).
//! It never executes: the unit becomes ready for the next `reconcile`.
//! Exit: 0 done, 1 error or refusal, 2 the lease is held.

use clap::Args;

use super::state::Common;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::Result;

/// Clear a unit's condition, or accept a checkpointed result.
#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("action").required(true).args(["retry", "accept"])))]
pub struct RecoverArgs {
    #[command(flatten)]
    pub common: Common,

    /// The unit to recover.
    #[arg(long)]
    pub unit: String,

    /// Clear the condition so the next reconcile executes the unit again.
    #[arg(long)]
    pub retry: bool,

    /// Adopt the receipt, outputs, and artifacts this run checkpointed for
    /// the unit; needs --reason.
    #[arg(long, value_name = "RUN", requires = "reason")]
    pub accept: Option<String>,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &RecoverArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    // Package `recover-verify` runs shell::recover::recover through
    // super::state::run, as the state commands do.
    let _ = (args, wiring);
    todo!("recover-verify")
}
