//! Operator commands dispatch by capability, preserve holds, and fence later execution.
mod scenario;

use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_core::orchestration::driver::*;
use nyl_core::orchestration::ids::EnvironmentName;
use nyl_core::orchestration::state::{DesiredUnit, Receipt};
use nyl_drivers::command::CommandDriver;
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use scenario::shell::{project, Harness, NYL_TOML};
use serde_json::Value;

struct OperatorDriver(Arc<Mutex<Vec<String>>>);
impl Driver for OperatorDriver {
    fn api_version(&self) -> &'static str {
        CommandDriver.api_version()
    }
    fn kind(&self) -> &'static str {
        "Command"
    }
    fn behavior_version(&self) -> u32 {
        CommandDriver.behavior_version()
    }
    fn capabilities(&self, spec: &Value) -> Capabilities {
        Capabilities {
            exec: Some(ExecTerminal::NonInteractive),
            ..CommandDriver.capabilities(spec)
        }
    }
    fn recovery(&self, spec: &Value) -> RecoveryPolicies {
        CommandDriver.recovery(spec)
    }
    fn spec_schema(&self) -> schemars::Schema {
        CommandDriver.spec_schema()
    }
    fn validate_spec(&self, spec: &Value) -> Result<(), String> {
        CommandDriver.validate_spec(spec)
    }
    fn ownership_scopes(&self, env: &EnvironmentName, spec: &Value) -> Vec<OwnershipScope> {
        CommandDriver.ownership_scopes(env, spec)
    }
    fn execution_key_inputs(&self, ctx: &ResolutionContext<'_>, spec: &Value) -> DriverResult<KeyInputs> {
        CommandDriver.execution_key_inputs(ctx, spec)
    }
    fn plan(&self, ctx: &ExecutionContext<'_>, unit: &DesiredUnit) -> DriverResult<Supported<PlanReport>> {
        CommandDriver.plan(ctx, unit)
    }
    fn reconcile(&self, _ctx: &ExecutionContext<'_>, _unit: &DesiredUnit) -> DriverResult<Supported<Outcome>> {
        Ok(Supported::Supported(Outcome::Succeeded {
            versions: Default::default(),
            outputs: serde_json::json!({}),
            artifacts: Vec::new(),
            attestations: Vec::new(),
        }))
    }
    fn verify(
        &self,
        ctx: &ExecutionContext<'_>,
        unit: &DesiredUnit,
        receipt: &Receipt,
    ) -> DriverResult<Supported<Verification>> {
        CommandDriver.verify(ctx, unit, receipt)
    }
    fn exec(
        &self,
        ctx: &ExecutionContext<'_>,
        _: &DesiredUnit,
        args: &[OsString],
    ) -> DriverResult<Supported<ExecStatus>> {
        assert_eq!(args, [OsString::from("native-argument")]);
        self.0
            .lock()
            .unwrap()
            .push(std::fs::read_to_string(ctx.project_dir.join("repair-input")).unwrap());
        #[cfg(unix)]
        if let Ok(metadata) = ctx.worktree.join(".github/copilot-instructions.md").symlink_metadata() {
            assert!(metadata.file_type().is_symlink());
            assert_eq!(
                std::fs::read_link(ctx.worktree.join(".github/copilot-instructions.md")).unwrap(),
                std::path::Path::new("../CLAUDE.md")
            );
        }
        std::fs::write(ctx.project_dir.join("operator-created"), "private execution tree").unwrap();
        Ok(Supported::Supported(ExecStatus::Exited { code: 17 }))
    }
}

fn exec(harness: &Harness, checkout: &std::path::Path, worktree: bool) -> CommandOutcome {
    let tools = ProcessRunner::new();
    let wiring = Wiring {
        clock: harness.clock.clone(),
        ids: &harness.ids,
        drivers: &harness.drivers,
        tools: &tools,
        faults: &NoFaults,
        runner: "operator test".to_owned(),
        author: Some(harness.identity.clone()),
        process_env: harness.process_env.clone(),
        cancellation: Cancellation::default(),
        prompt: None,
        observer: None,
    };
    let mut args = vec![
        "nyl".to_owned(),
        "exec".to_owned(),
        "-e".to_owned(),
        "dev".to_owned(),
        "--unit".to_owned(),
        "job".to_owned(),
        "--path".to_owned(),
        checkout.display().to_string(),
        "--reason".to_owned(),
        "repair test".to_owned(),
    ];
    if worktree {
        args.push("--worktree".to_owned());
    }
    args.extend(["--".to_owned(), "native-argument".to_owned()]);
    Cli::try_parse_from(args).unwrap().execute_in_process(&wiring).unwrap()
}

#[test]
fn test_exec_is_generic_requires_hold_and_snapshots_dirty_files_before_repair() {
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {name: dev}\nspec:\n  unitSelector: {matchLabels: {dev: 'true'}}\n  allowUnprotectedSource: true\n"),
        ("nyl/job.yaml", "apiVersion: units.gitops.nyl/v1\nkind: Command\nmetadata: {name: job, labels: {dev: 'true'}}\nspec:\n  command: [sh, -c, 'true']\n  idempotent: true\n"),
        ("nyl/repair-input", "committed"),
    ]);
    let mut harness = Harness::new(project.path(), true);
    let seen = Arc::new(Mutex::new(Vec::new()));
    harness.drivers.register(Arc::new(OperatorDriver(seen.clone())));
    harness.init();
    let initial = harness.invoke(&["reconcile", "-e", "dev"]);
    assert_eq!(initial.exit, 0, "{}", initial.stderr);
    let checkout = harness.world.fresh_clone("main");
    let refused = exec(&harness, &checkout, false);
    assert_eq!(refused.exit, 125, "{}", refused.stderr);
    assert!(refused.stderr.contains("NYL-EXEC-NOT-HELD"));
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(harness.invoke(&["hold", "-e", "dev", "--unit", "job"]).exit, 0);
    assert_eq!(exec(&harness, &checkout, false).exit, 17);
    std::fs::write(checkout.join("nyl/repair-input"), "uncommitted").unwrap();
    #[cfg(unix)]
    {
        std::fs::create_dir(checkout.join(".github")).unwrap();
        std::os::unix::fs::symlink("../CLAUDE.md", checkout.join(".github/copilot-instructions.md")).unwrap();
    }
    assert_eq!(exec(&harness, &checkout, true).exit, 17);
    assert_eq!(*seen.lock().unwrap(), ["committed", "uncommitted"]);
    assert!(!checkout.join("nyl/operator-created").exists());
    let snapshot = harness.snapshot();
    assert!(snapshot.observed[&scenario::shell::unit("job")]
        .operator_modified
        .is_some());
    assert_eq!(harness.invoke(&["resume", "-e", "dev", "--unit", "job"]).exit, 0);
    let waiting = harness.invoke(&["reconcile", "-e", "dev"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(
        waiting.report["environments"][0]["units"]["job"]["result"],
        "awaiting-approval"
    );
    let approved = harness.invoke(&["reconcile", "-e", "dev", "--approve", "job"]);
    assert_eq!(approved.exit, 0, "{}", approved.stderr);
    assert!(harness.snapshot().observed[&scenario::shell::unit("job")]
        .operator_modified
        .is_none());
}
