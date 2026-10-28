//! `nyl verify -e <env>`: run driver verification against the current
//! receipts and record the latest observations and driver attestations in
//! one observed commit.
//!
//! Contract: [Commands](../../../../design/orchestration-core.md#commands)
//! and [Inspection](../../../../design/orchestration-core.md#inspection).
//! Exit categories: 0 every verified unit clean and its attestations pass;
//! 2 a unit could not be verified (no current receipt, a kind that cannot
//! verify, or unknown drift) or the lease is held; 3 drift or a failing
//! attestation; 4 cancelled by SIGINT or SIGTERM (`NYL-RUN-CANCELLED`,
//! nothing recorded); 1 error; the most severe wins.
//!
//! A drifted unit's `NYL-VERIFY-DRIFT` fix says what the next reconcile does
//! with it under the environment's stored `driftRepair` and the unit's
//! recovery policy ([Drift](../../../../design/orchestration-core.md#drift);
//! `nyl_orchestration::rules::verification::warnings`).

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::run_report::ReportKind;
use nyl_orchestration::shell::run::DEFAULT_CONCURRENCY;
use nyl_state::git::CommitAuthor;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use crate::Result;

/// Verify an environment's current receipts.
#[derive(Args, Debug, Clone)]
pub struct VerifyArgs {
    /// The environment to verify.
    #[arg(short = 'e', long = "environment", required_unless_present_any = ["labels", "group"], conflicts_with_all = ["labels", "group"])]
    pub environment: Vec<String>,

    /// Select environments whose literal labels match key=value pairs.
    #[arg(short = 'l', long, conflicts_with_all = ["environment", "group"])]
    pub labels: Option<String>,

    /// Select members of this EnvironmentGroup.
    #[arg(short = 'g', long, conflicts_with_all = ["environment", "labels"])]
    pub group: Option<String>,

    /// Verify only these units (repeatable, or comma-separated with --units).
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    #[arg(skip)]
    pub unit_filter: bool,

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
    use crate::orchestration::multi_environment::{dispatch, select};
    Ok(dispatch(
        ReportKind::VerifyReport,
        args.output,
        select(
            &args.path,
            &args.environment,
            args.labels.as_deref(),
            args.group.as_deref(),
            false,
            None,
            wiring.drivers,
        ),
        |environment, selected, _planned, _unavailable| {
            let mut args = args.clone();
            args.environment = vec![environment.to_owned()];
            args.unit_filter = !args.units.is_empty();
            let mut prepare = || -> std::result::Result<_, Stop> {
                args.units = nyl_orchestration::rules::multi_environment::units(
                    "--unit",
                    &args.units,
                    environment,
                    selected,
                    selected.len() > 1,
                )?;
                verify(&args, wiring)
            };
            (
                prepare().unwrap_or_else(|stop| stopped(ReportKind::VerifyReport, environment, args.output, stop)),
                std::collections::BTreeSet::default(),
            )
        },
    ))
}

fn verify(args: &VerifyArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment[0])?;
    // Verify writes observations, so it needs a commit identity.
    let identity = wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?;
    let mut request = session.request(&RunOptions {
        units: &args.units,
        concurrency: DEFAULT_CONCURRENCY,
        source: None,
        wait: args.wait_lease,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode: nyl_orchestration::shell::run::RunMode::default(),
        lifecycle: nyl_orchestration::shell::run::LifecycleOptions::default(),
        reapply: false,
        fail_fast: false,
        reason: None,
    })?;
    if args.unit_filter && args.units.is_empty() {
        request.units = Some(std::collections::BTreeSet::default());
    }
    let store = session.store(Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    }))?;
    let report = session.run(
        wiring,
        &store,
        Some(&identity),
        &request,
        nyl_orchestration::shell::verify::verify,
    )?;
    let exit = report.exit().code();
    Ok(outcome(&report, args.output, exit))
}
