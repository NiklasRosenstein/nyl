//! `nyl detect-drift -e <env>` and `nyl attest -e <env>`: check the
//! current receipts with their drivers and record the drift records, or the
//! driver attestations, in one observed commit.
//!
//! Contract: [Drift](../../../../design/orchestration-core.md#drift),
//! [Attestations](../../../../design/orchestration-core.md#attestations),
//! [Commands](../../../../design/orchestration-core.md#commands), and
//! [Inspection](../../../../design/orchestration-core.md#inspection).
//! Exit categories: 0 every checked unit clean, or its attestations pass;
//! 2 a unit could not be checked (no current receipt, a kind that cannot
//! run the check, or an unknown result) or the lease is held; 3 drift or a
//! failing attestation; 4 cancelled by SIGINT or SIGTERM
//! (`NYL-RUN-CANCELLED`, nothing recorded); 1 error; the most severe wins.
//!
//! A drifted unit's `NYL-DRIFT-DETECTED` fix says what the next reconcile
//! does with it under the environment's stored `driftRepair` and the unit's
//! recovery policy ([Drift](../../../../design/orchestration-core.md#drift);
//! `nyl_orchestration::rules::checks::warnings`).

use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::rules::checks::Check;
use nyl_orchestration::shell::run::DEFAULT_CONCURRENCY;
use nyl_state::git::CommitAuthor;

use super::reconcile::RunOutput;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use nyl_render::Result;

/// Check an environment's current receipts.
#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    /// The environment to check.
    #[arg(short = 'e', long = "environment", required_unless_present_any = ["labels", "group"], conflicts_with_all = ["labels", "group"])]
    pub environment: Vec<String>,

    /// Select environments whose literal labels match key=value pairs.
    #[arg(short = 'l', long, conflicts_with_all = ["environment", "group"])]
    pub labels: Option<String>,

    /// Select members of this EnvironmentGroup.
    #[arg(short = 'g', long, conflicts_with_all = ["environment", "labels"])]
    pub group: Option<String>,

    /// Check only these units (repeatable, or comma-separated with --units).
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

/// Run `check` in-process with `wiring`.
pub fn run(args: &CheckArgs, check: Check, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    use crate::orchestration::multi_environment::{dispatch, select};
    Ok(dispatch(
        check.report_kind(),
        args.output,
        select(
            &args.path,
            &args.environment,
            args.labels.as_deref(),
            args.group.as_deref(),
            false,
            None,
            false,
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
                run_one(&args, check, wiring)
            };
            (
                prepare().unwrap_or_else(|stop| stopped(check.report_kind(), environment, args.output, stop)),
                std::collections::BTreeSet::default(),
            )
        },
    ))
}

fn run_one(args: &CheckArgs, check: Check, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment[0])?;
    // A check writes drift records or attestations, so it needs a commit identity.
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
    let operation = match check {
        Check::DetectDrift => nyl_orchestration::shell::check::detect_drift,
        Check::Attest => nyl_orchestration::shell::check::attest,
    };
    let report = session.run(wiring, &store, Some(&identity), &request, operation)?;
    let exit = report.exit().code();
    Ok(outcome(&report, args.output, exit))
}
