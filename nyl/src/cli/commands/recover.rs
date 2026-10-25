//! `nyl recover -e <env> --unit <u> (--retry | --accept <run>)`: clear a
//! unit's uncertain condition or non-retryable failure for re-execution, or
//! adopt a result a run checkpointed, recording the operator's reason.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery).
//! It never executes: the unit becomes ready for the next `reconcile`.
//! Exit: 0 done, 1 error or refusal (including `NYL-RUN-CANCELLED` when a
//! signal stops `--wait-lease`), 2 the lease is held. Once it holds the
//! lease, recover finishes its one commit despite a first signal.

use clap::Args;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{RunId, UnitName};
use nyl_core::orchestration::state::Operation;
use nyl_orchestration::report::StateOperationReport;
use nyl_orchestration::rules::recovery::RecoverAction;
use nyl_orchestration::shell::recover::recover;

use super::state::Common;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::{NylError, Result};

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
    let unit = UnitName::new(&args.unit).map_err(|error| NylError::config(error.to_string()))?;
    let action = match &args.accept {
        None => Ok(RecoverAction::Retry),
        Some(run) => RunId::new(run)
            .map(|run| RecoverAction::Accept { run })
            .map_err(|error| {
                Finding::new(
                    Code::RecoverCheckpointMissing,
                    format!("--accept {run:?} is not a run ID: {error}"),
                    "pass a run ID as `nyl recover` lists the checkpoints under evidence",
                )
            }),
    };
    let requested = super::state::requested(&args.common, wiring);
    super::state::run(
        &args.common,
        wiring,
        Operation::Recover.as_str(),
        None,
        |ctx| match &action {
            Ok(action) => recover(ctx.context, &ctx.environment, ctx.location, &unit, action, &requested),
            Err(finding) => {
                StateOperationReport::new(ctx.environment.clone(), Operation::Recover.as_str()).refused(finding.clone())
            }
        },
    )
}
