//! Generic native tools for held units, prepared and fenced by orchestration.
use std::ffi::OsString;
use std::path::PathBuf;

use clap::Args;
use nyl_core::orchestration::ids::UnitName;
use nyl_core::orchestration::time::Duration;
use nyl_orchestration::shell::operator::{ExecOptions, WorktreeUnit};
use nyl_orchestration::shell::run::{LifecycleOptions, RunMode};
use nyl_state::git::CommitAuthor;

use crate::orchestration::run::{CommandOutcome, Wiring};
use crate::orchestration::session::{RunOptions, Session, Stop};
use crate::{NylError, Result};

/// Run a unit driver's operator tool without deploying source.
#[derive(Args, Debug)]
pub struct ExecArgs {
    #[arg(short = 'e', long = "environment")]
    pub environment: String,
    #[arg(long)]
    pub unit: String,
    /// Use a snapshot of the current checkout's spec and unpinned source files.
    #[arg(long)]
    pub worktree: bool,
    #[arg(long)]
    pub timeout: Option<Duration>,
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long)]
    pub wait_lease: Option<Duration>,
    #[arg(long)]
    pub author: Option<String>,
    #[arg(long, default_value = ".")]
    pub path: PathBuf,
    /// Native tool arguments, passed unchanged after --.
    #[arg(last = true, required = true)]
    pub arguments: Vec<OsString>,
}

/// Keep refusal status distinct from the native tool's own exit status.
pub fn run(args: &ExecArgs, wiring: &Wiring<'_>) -> Result<CommandOutcome> {
    let result = prepare(args, wiring);
    Ok(match result {
        Ok(outcome) => outcome,
        Err(stop) => {
            let message = match stop {
                Stop::Finding(finding) => format!("{}: {}\n{}\n", finding.code, finding.message, finding.fix),
                Stop::Error(error) => format!("NYL-EXEC-INVALID: {error}\n"),
            };
            CommandOutcome {
                exit: 125,
                report: serde_json::json!({"kind":"ExecReport","errors":[message]}),
                stdout: String::new(),
                stderr: message,
            }
        }
    })
}

fn prepare(args: &ExecArgs, wiring: &Wiring<'_>) -> std::result::Result<CommandOutcome, Stop> {
    let session = Session::open(&args.path, &args.environment)?;
    let identity = wiring.commit_identity(&session.resolved.git_dir, args.author.as_deref())?;
    let unit = UnitName::new(&args.unit).map_err(|error| NylError::config(error.to_string()))?;
    let prepared = args
        .worktree
        .then(|| crate::orchestration::direct::prepare(&args.path, &args.unit, Some(&args.environment), None, wiring))
        .transpose()?;
    let request = session.request(&RunOptions {
        units: &[],
        concurrency: 1,
        source: None,
        wait: args.wait_lease,
        local: None,
        cancellation: wiring.cancellation.clone(),
        mode: RunMode::Reconcile,
        lifecycle: LifecycleOptions::default(),
        reapply: false,
        fail_fast: false,
        reason: args.reason.as_deref(),
    })?;
    let store = session.store(Some(CommitAuthor {
        identity: identity.clone(),
        clock: wiring.clock.clone(),
    }))?;
    let options = ExecOptions {
        unit,
        arguments: args.arguments.clone(),
        timeout: args.timeout,
        interactive: wiring.prompt.is_some(),
        worktree: prepared.as_ref().map(|prepared| WorktreeUnit {
            unit: prepared.unit.clone(),
            worktree: prepared.worktree.clone(),
            project_dir: prepared.project_dir.clone(),
            source_roots: prepared.source_roots.clone(),
            audit: prepared.audit.clone(),
        }),
    };
    let report = session.run_with(wiring, &store, Some(&identity), &request, |context, request| {
        nyl_orchestration::shell::operator::exec(context, request, &options)
    })?;
    let mut stderr = String::new();
    for finding in &report.errors {
        crate::orchestration::session::write_finding(&mut stderr, "error", None, finding);
    }
    for finding in &report.warnings {
        crate::orchestration::session::write_finding(&mut stderr, "warning", None, finding);
    }
    Ok(CommandOutcome {
        exit: report.exit,
        report: serde_json::to_value(&report).expect("exec report serializes"),
        stdout: String::new(),
        stderr,
    })
}
