//! `nyl hold -e <env> --unit <u>` and `nyl resume -e <env> --unit <u>`:
//! freeze a unit so `reconcile` changes nothing about it, and lift the hold.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit". Each writes one desired commit. `hold` works for
//! a unit never reconciled in the environment (a held marker) and never
//! renders source; while a reconcile or teardown run holds the lease,
//! `hold` delivers the hold to it through its signal ref
//! (`NYL-HOLD-DELIVERED`, exit 0). `resume` only lifts the hold. Exit: 0
//! done, delivered, or unchanged; 1 error or refusal; 2 the lease is
//! broken, or held by an operation that reads no signals (or, for
//! `resume`, by any run).

use clap::Args;

use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::state::Operation;
use nyl_orchestration::shell::lifecycle;

use super::state::{requested, run, Common};
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::{NylError, Result};

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
    let unit = unit_name(&args.unit)?;
    let requested = requested(&args.common, wiring);
    run(&args.common, wiring, Operation::Hold.as_str(), None, false, |ctx| {
        lifecycle::hold(ctx.context, &ctx.environment, ctx.location, &unit, &requested)
    })
}

/// Run `nyl resume` in-process with `wiring`.
pub fn run_resume(args: &ResumeArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let unit = unit_name(&args.unit)?;
    let requested = requested(&args.common, wiring);
    run(&args.common, wiring, Operation::Resume.as_str(), None, false, |ctx| {
        lifecycle::resume(ctx.context, &ctx.environment, ctx.location, &unit, &requested)
    })
}

fn unit_name(unit: &str) -> Result<UnitName> {
    UnitName::new(unit).map_err(|error| NylError::config(format!("--unit {unit:?} is not a unit name: {error}")))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::cli::Cli;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nyl").chain(args.iter().copied())).is_ok()
    }

    /// Catches `hold` and `resume` accepted without the unit they act on.
    #[test]
    fn test_hold_and_resume_need_a_unit() {
        assert!(parses(&["hold", "-e", "dev", "--unit", "api", "--reason", "incident"]));
        assert!(!parses(&["hold", "-e", "dev"]));
        assert!(!parses(&["resume", "-e", "dev"]));
    }
}
