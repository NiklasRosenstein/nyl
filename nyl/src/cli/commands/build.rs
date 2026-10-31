//! Direct artifact builds: no state write, receipt, or lease.
use crate::orchestration::direct;
use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::secrets::ProjectSecrets;
use crate::{NylError, Result};
use clap::Args;
use nyl_core::orchestration::driver::{DriverOperation, ExecutionContext, Supported};
use std::path::PathBuf;

/// Build one unit from the worktree, including uncommitted changes.
#[derive(Args, Debug)]
pub struct BuildArgs {
    pub unit: String,
    /// Render environment values and read its current input receipts without writing state.
    #[arg(short = 'e', long = "environment")]
    pub environment: Option<String>,
    /// Build at a source revision instead of the current worktree.
    #[arg(long)]
    pub source: Option<String>,
    /// Project directory or a path beneath it.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Arguments interpreted by the unit driver, passed unchanged after --.
    #[arg(last = true)]
    pub arguments: Vec<std::ffi::OsString>,
}

#[allow(clippy::too_many_lines)] // Build admission and execution use one prepared unit and context.
pub fn run(args: &BuildArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let prepared = direct::prepare(
        &args.path,
        &args.unit,
        args.environment.as_deref(),
        args.source.as_deref(),
        wiring,
    )?;
    let driver = wiring
        .drivers
        .get(&prepared.unit.unit.api_version, &prepared.unit.unit.kind)
        .ok_or_else(|| NylError::config("the unit's driver is unavailable"))?;
    let spec = prepared.unit.resolved_spec.as_ref().expect("prepared spec");
    if !driver.capabilities(spec).build {
        return Err(NylError::config(format!(
            "{} has no build capability; use plan and reconcile for units with external effects",
            prepared.unit.unit.kind
        )));
    }
    let temp = prepared.workspace.path().join("build");
    std::fs::create_dir_all(&temp)?;
    let provider = ProjectSecrets::new(&prepared.inventory.project_root);
    let (env, secrets) =
        nyl_orchestration::shell::run::driver_credentials(&wiring.process_env, Some(&provider), driver, spec)
            .map_err(NylError::config)?;
    let now = wiring.clock.now();
    let timeout = nyl_orchestration::shell::run::unit_timeout(wiring.drivers, &prepared.unit);
    let deadline = timeout.after(now);
    let tools = nyl_orchestration::shell::tools::DeadlineRunner {
        tools: wiring.tools,
        clock: wiring.clock.as_ref(),
        deadline,
    };
    let context = ExecutionContext {
        unit: nyl_core::orchestration::ids::UnitAddress::new(
            prepared.unit.unit.environment.clone(),
            prepared.unit.unit.name.clone(),
        ),
        operation: DriverOperation::Build,
        worktree: prepared.worktree.clone(),
        project_dir: prepared.project_dir.clone(),
        source_roots: prepared.source_roots.clone(),
        temp_dir: temp,
        env,
        secrets,
        artifacts: prepared
            .snapshot
            .artifacts
            .get(&prepared.unit.unit.name)
            .cloned()
            .unwrap_or_default(),
        approved_digest: None,
        deadline,
        now,
        observed: prepared.snapshot.observed.get(&prepared.unit.unit.name),
        tools: &tools,
        commit_identity: None,
        cache_dir: prepared.workspace.path().join("cache"),
        teardown_wait: None,
        interactive: false,
    };
    let artifacts = match driver
        .build(&context, &prepared.unit, &args.arguments)
        .map_err(|error| NylError::config(error.to_string()))?
    {
        Supported::Supported(artifacts) => artifacts,
        Supported::Unsupported => {
            return Err(NylError::config(
                "the kind declared build support but its driver refused it",
            ))
        }
    };
    let report = if artifacts.len() == 1 {
        serde_json::to_value(&artifacts[0])
    } else {
        serde_json::to_value(&artifacts)
    }
    .map_err(|error| NylError::config(error.to_string()))?;
    Ok(CommandOutcome {
        exit: 0,
        stdout: format!(
            "{}\n",
            serde_json::to_string_pretty(&report).expect("artifacts serialize")
        ),
        report,
        stderr: String::new(),
    })
}
