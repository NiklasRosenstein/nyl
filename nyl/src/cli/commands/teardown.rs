//! `nyl teardown -e <env> (--unit <u>… | --all) [--hold] [--confirm-removed]`:
//! tear units down, replacing a unit still in the ownership set, holding
//! them afterwards with `--hold` (and `--all`, for units still in the
//! ownership set), and ending a publication's teardown wait on the
//! operator's word with `--confirm-removed`. A held unit is not torn down:
//! the request is recorded on it and carried out after `nyl resume`.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Explicit teardown", [Instances](../../../../design/orchestration-core.md#instances),
//! and [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit).
//! Exit categories as for `reconcile`: 0 every named unit's deletion
//! completed, or its request was recorded on a held unit; 2 a teardown waits (`--allow-dependents`, a dependent, a
//! teardown wait, readiness, an approval) or the lease is held; 3 a teardown
//! failed; 4 uncertain, the lease was lost, or cancelled; 1 error.

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::Result;

/// Tear units down.
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
#[command(group(clap::ArgGroup::new("target").required(true).args(["units", "all"])))]
pub struct TeardownArgs {
    /// The environment.
    #[arg(short = 'e', long = "environment")]
    pub environment: String,

    /// Tear down these units (repeatable, or comma-separated with --units).
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    /// Tear down every unit, dependents first, and hold the units still in
    /// the ownership set afterwards; kinds that cannot tear down and
    /// deletionPolicy Retain units are released.
    #[arg(long)]
    pub all: bool,

    /// Hold the units after their teardown, in the same operation.
    #[arg(long)]
    pub hold: bool,

    /// Start no further teardown after the first failure, uncertain outcome,
    /// or approval wait; what finished is still recorded.
    #[arg(long)]
    pub fail_fast: bool,

    /// Confirm that a publication's workloads are gone, ending its teardown
    /// wait: directly, or by signalling the run waiting on it.
    #[arg(long)]
    pub confirm_removed: bool,

    /// Tear down units that retained tombstones still cite.
    #[arg(long)]
    pub allow_dependents: bool,

    /// Proceed despite unmet readiness requirements or an unfinished
    /// teardown wait, recording what may remain.
    #[arg(long)]
    pub allow_incomplete: bool,

    /// With --all, take the units' source files from this revision when the
    /// recorded source commit cannot be fetched.
    #[arg(long, requires = "all")]
    pub source: Option<String>,

    /// Why, recorded as the transition commit's Nyl-Reason.
    #[arg(long)]
    pub reason: Option<String>,

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
pub fn run(args: &TeardownArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let _ = (args, wiring);
    todo!("operator: build the request with RunMode::Teardown and call shell::teardown::teardown")
}
