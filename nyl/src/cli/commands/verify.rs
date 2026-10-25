//! `nyl verify -e <env>`: run driver verification against the current
//! receipts and record the latest observations and driver attestations in
//! one observed commit.
//!
//! Contract: [Commands](../../../../design/orchestration-core.md#commands)
//! and [Inspection](../../../../design/orchestration-core.md#inspection).
//! Exit categories: 0 every verified unit clean and its attestations pass;
//! 2 a unit could not be verified (no current receipt, a kind that cannot
//! verify, or unknown drift) or the lease is held; 3 drift or a failing
//! attestation; 1 error; the most severe wins.

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::run_report::ReportKind;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{stopped, Stop};
use crate::Result;

/// Verify an environment's current receipts.
#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// The environment to verify.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Verify only these units (repeatable, or comma-separated with --units).
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

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
pub fn run(args: &VerifyArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    Ok(verify(args, wiring)
        .unwrap_or_else(|stop| stopped(ReportKind::VerifyReport, &args.environment, args.output, stop)))
}

fn verify(args: &VerifyArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    // Package `recover-verify` wires the shell's verify like reconcile.
    let _ = (args, wiring);
    todo!("recover-verify")
}
