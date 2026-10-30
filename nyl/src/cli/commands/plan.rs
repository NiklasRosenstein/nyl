//! `nyl plan -e <env>`: what a reconcile would do, without the lease and
//! without writing anything.
//!
//! Contract: [Planning](../../../../design/orchestration-core.md#planning).
//! Exit categories: 0 complete; 2 a unit is blocked or waits; 3 a driver
//! plan failed; 1 error, or `--fail-on-leaving` with a unit leaving.

use std::path::PathBuf;

use clap::Args;

use super::reconcile::RunOutput;
use nyl_orchestration::run_report::{ExitCategory, ReportKind};

use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{outcome, stopped, RunOptions, Session, Stop};
use crate::Result;

/// Plan an environment's run.
#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // Independent command-line switches.
pub struct PlanArgs {
    /// The environment to plan.
    #[arg(short = 'e', long = "environment", required_unless_present_any = ["labels", "group"], conflicts_with_all = ["labels", "group"])]
    pub environment: Vec<String>,

    /// Select environments whose literal labels match key=value pairs.
    #[arg(short = 'l', long, conflicts_with_all = ["environment", "group"])]
    pub labels: Option<String>,

    /// Select members of this EnvironmentGroup.
    #[arg(short = 'g', long, conflicts_with_all = ["environment", "labels"])]
    pub group: Option<String>,

    /// Report only these units; the leaving section is never filtered.
    #[arg(long = "unit", alias = "units", value_delimiter = ',')]
    pub units: Vec<String>,

    #[arg(skip)]
    pub unit_filter: bool,

    /// Render units at this revision or commit instead of the Environment's source.
    #[arg(long)]
    pub source: Option<String>,

    /// Exit 1 when a unit leaves the ownership set.
    #[arg(long)]
    pub fail_on_leaving: bool,

    /// Preview a teardown that has not been requested: the destroy plans of
    /// what `nyl teardown` would remove, for the --unit units or with --all.
    #[arg(long)]
    pub teardown: bool,

    /// With --teardown, preview tearing down every unit.
    #[arg(long, requires = "teardown", conflicts_with = "units")]
    pub all: bool,

    /// Output format.
    #[arg(short = 'o', long = "output", value_enum, default_value_t)]
    pub output: RunOutput,

    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// The run mode of `plan --teardown`: a teardown preview of the `--unit`
/// units or, with `--all`, of every unit.
fn teardown_mode(args: &PlanArgs) -> std::result::Result<nyl_orchestration::shell::run::RunMode, Stop> {
    use nyl_orchestration::shell::run::{RunMode, TeardownTarget};
    if !args.teardown {
        return Ok(RunMode::Reconcile);
    }
    if args.all {
        return Ok(RunMode::Teardown {
            target: TeardownTarget::All,
        });
    }
    if args.units.is_empty() {
        return Err(Stop::Finding(nyl_core::orchestration::codes::Finding::new(
            nyl_core::orchestration::codes::Code::UnitUnknown,
            "plan --teardown names no unit to preview tearing down",
            format!(
                "name the units with `nyl plan -e {} --teardown --unit <u>`, or preview tearing down every unit with --all",
                args.environment[0]
            ),
        )));
    }
    Ok(RunMode::Teardown {
        target: TeardownTarget::Units(crate::orchestration::session::unit_names("--unit", &args.units)?),
    })
}

/// Run the command in-process with `wiring`.
pub fn run(args: &PlanArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    use crate::orchestration::multi_environment::{dispatch, select};
    Ok(dispatch(
        ReportKind::PlanReport,
        args.output,
        select(
            &args.path,
            &args.environment,
            args.labels.as_deref(),
            args.group.as_deref(),
            !args.teardown,
            args.source.as_deref(),
            false,
            wiring.drivers,
        ),
        |environment, selected, planned, unavailable| {
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
                crate::orchestration::multi_environment::single_source(args.source.as_deref(), selected)?;
                plan(&args, wiring, planned, unavailable)
            };
            prepare().unwrap_or_else(|stop| {
                (
                    stopped(ReportKind::PlanReport, environment, args.output, stop),
                    std::collections::BTreeSet::default(),
                )
            })
        },
    ))
}

fn plan(
    args: &PlanArgs,
    wiring: &Wiring<'_>,
    planned: &std::collections::BTreeMap<
        nyl_core::orchestration::ids::EnvironmentName,
        std::collections::BTreeSet<nyl_core::orchestration::ids::UnitName>,
    >,
    unavailable: &std::collections::BTreeSet<nyl_core::orchestration::ids::EnvironmentName>,
) -> std::result::Result<
    (
        CommandOutcome,
        std::collections::BTreeSet<nyl_core::orchestration::ids::UnitName>,
    ),
    Stop,
> {
    let session = Session::open(&args.path, &args.environment[0])?;
    let mut request = session.request(&RunOptions {
        units: &args.units,
        concurrency: nyl_orchestration::shell::run::DEFAULT_CONCURRENCY,
        source: args.source.as_deref(),
        wait: None,
        local: None,
        cancellation: nyl_orchestration::shell::run::Cancellation::default(),
        mode: teardown_mode(args)?,
        lifecycle: nyl_orchestration::shell::run::LifecycleOptions::default(),
        reapply: false,
        fail_fast: false,
        reason: None,
    })?;
    request.unavailable.clone_from(unavailable);
    request.planned = planned.clone();
    // A plan writes nothing, so it reads through a read-only store and needs
    // no commit identity.
    if args.unit_filter && args.units.is_empty() {
        request.units = Some(std::collections::BTreeSet::default());
    }
    let store = session.store(None)?;
    let report = session.run(wiring, &store, None, &request, nyl_orchestration::shell::run::plan)?;
    let mut exit = report.exit();
    let leaving = report
        .environments
        .iter()
        .any(|environment| !environment.leaving.is_empty());
    if args.fail_on_leaving && leaving {
        exit = exit.most_severe(ExitCategory::Error);
    }
    let changes = report
        .planned_changes()
        .remove(&session.resolved.name)
        .unwrap_or_default();
    Ok((outcome(&report, args.output, exit.code()), changes))
}
