//! `nyl recover -e <env> [--unit <u> (--retry | --accept <run>)]`: import
//! the run of a broken lease without executing, and clear a unit's
//! uncertain condition or non-retryable failure for re-execution, or adopt
//! a result a run checkpointed, recording the operator's reason.
//!
//! Contract: [Recovery](../../../../design/orchestration-core.md#recovery).
//! It never executes: imported units and the recovered unit run in the next
//! `reconcile`. It runs on the shared run path
//! ([`recover_environment`]), which writes one transition commit and no run
//! record. Exit: 0 done (also once an import is recorded with units left
//! uncertain), 1 error or refusal (including `NYL-RUN-CANCELLED` when a
//! signal stops `--wait-lease`), 2 the lease is held. Once it holds the
//! lease, recover finishes its one commit despite a first signal.

use clap::Args;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{RunId, UnitName};
use nyl_core::orchestration::state::Operation;
use nyl_orchestration::report::StateOperationReport;
use nyl_orchestration::rules::recovery::RecoverAction;
use nyl_orchestration::shell::recover::{recover_environment, RecoverUnit};
use nyl_orchestration::shell::run::{LifecycleOptions, RunMode};

use super::state::{render, Common};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::placeholder_environment;
use crate::{NylError, Result};

/// Import a broken lease's run, clear a unit's condition, or accept a checkpointed result.
#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("action").args(["retry", "accept"])))]
pub struct RecoverArgs {
    #[command(flatten)]
    pub common: Common,

    /// The unit to recover; needs --retry or --accept. Without it, recover
    /// imports the run of a broken lease.
    #[arg(long, requires = "action")]
    pub unit: Option<String>,

    /// Clear the condition so the next reconcile executes the unit again.
    #[arg(long, requires = "unit")]
    pub retry: bool,

    /// Adopt the receipt, outputs, and artifacts this run checkpointed for
    /// the unit; needs --unit and --reason.
    #[arg(long, value_name = "RUN", requires_all = ["unit", "reason"])]
    pub accept: Option<String>,
}

/// Run the command in-process with `wiring`.
pub fn run(args: &RecoverArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let common = &args.common;
    let refused = |finding: Finding| {
        let environment = placeholder_environment(&common.environment);
        render(
            &StateOperationReport::new(environment, Operation::Recover.as_str()).refused(finding),
            common.output,
        )
    };
    let unit = match &args.unit {
        None => None,
        Some(unit) => {
            let unit = UnitName::new(unit).map_err(|error| NylError::config(error.to_string()))?;
            let action = match &args.accept {
                None => RecoverAction::Retry,
                Some(run) => match RunId::new(run) {
                    Ok(run) => RecoverAction::Accept { run },
                    Err(error) => {
                        return Ok(refused(Finding::new(
                            Code::RecoverCheckpointMissing,
                            format!("--accept {run:?} is not a run ID: {error}"),
                            "pass a run ID as `nyl recover` lists the checkpoints under evidence",
                        )))
                    }
                },
            };
            Some(RecoverUnit { unit, action })
        }
    };
    super::state::run_lifecycle(
        common,
        wiring,
        Operation::Recover.as_str(),
        RunMode::Recover { unit },
        LifecycleOptions::default(),
        None,
        None,
        recover_environment,
    )
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        recover: RecoverArgs,
    }

    /// Catches the import form (`nyl recover -e dev`, no unit) rejected by
    /// the parser, or a unit action accepted without its unit or action.
    #[test]
    fn test_recover_parses_the_import_form_and_ties_actions_to_a_unit() {
        let parsed = Cli::try_parse_from(["recover", "-e", "dev"]).unwrap();
        assert!(parsed.recover.unit.is_none() && !parsed.recover.retry);
        let parsed = Cli::try_parse_from(["recover", "-e", "dev", "--unit", "database", "--retry"]).unwrap();
        assert_eq!(parsed.recover.unit.as_deref(), Some("database"));
        for invalid in [
            &["recover", "-e", "dev", "--retry"][..],
            &["recover", "-e", "dev", "--unit", "database"],
            &["recover", "-e", "dev", "--unit", "database", "--accept", "run"],
            &[
                "recover", "-e", "dev", "--unit", "database", "--retry", "--accept", "run", "--reason", "x",
            ],
        ] {
            assert!(Cli::try_parse_from(invalid).is_err(), "{invalid:?} parses");
        }
    }
}
