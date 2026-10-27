//! Drift repair end to end, in-process with Git state: `nyl verify` records
//! drift of Command units whose `verify` step reads a marker file outside
//! the project (a change made outside Nyl), and `status`, `plan`, and
//! `reconcile` act on it as `driftRepair` and the recovery policies say.
//!
//! Contract: [Drift](../../design/orchestration-core.md#drift), as amended
//! by the part 5 decisions: `drifted` is work for the next reconcile;
//! `driftRepair: converge` repairs `converge` units and leaves the others
//! awaiting approval while the run continues; `manual` repairs nothing by
//! itself; `--unit <u> --reapply --reason <text>` forces named units under
//! the same approval rule.

mod scenario;

use std::path::{Path, PathBuf};

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use scenario::runner::AUTHOR;
use scenario::shell::{self, command, project, ENVIRONMENT, NYL_TOML};
use scenario::world::git_output;
use serde_json::Value;
use tempfile::TempDir;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";

/// A project with two Command units whose `verify` reports drift while a
/// marker file `<unit>.drift` exists in `markers`, and whose command
/// removes the marker (a repair) and appends to `<unit>.runs`:
/// `api` is idempotent (recovery policy `converge`), `job` is not
/// (`manual`).
struct Harness {
    shell: shell::Harness,
    markers: TempDir,
    _project: TempDir,
}

impl Harness {
    fn new(drift_repair: Option<&str>) -> Self {
        let markers = TempDir::new().unwrap();
        // `sh` reads backslashes as escapes; Windows accepts forward slashes.
        let dir = markers.path().display().to_string().replace('\\', "/");
        let unit = |name: &str, idempotent: bool| {
            command(
                name,
                &format!("rm -f {dir}/{name}.drift; echo run >> {dir}/{name}.runs"),
                &format!(
                    "  verify: [sh, -c, 'test ! -e {dir}/{name}.drift || exit 2']\n{}",
                    if idempotent { "  idempotent: true\n" } else { "" }
                ),
            )
        };
        let environment = match drift_repair {
            Some(repair) => format!("{ENVIRONMENT}  driftRepair: {repair}\n"),
            None => ENVIRONMENT.to_owned(),
        };
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", &environment),
            ("nyl/api.yaml", &unit("api", true)),
            ("nyl/job.yaml", &unit("job", false)),
        ]);
        let shell = shell::Harness::new(project.path(), true);
        Self {
            shell,
            markers,
            _project: project,
        }
    }

    /// `nyl <args>` in-process in `clone`.
    fn invoke_at(&self, clone: &Path, args: &[&str]) -> Result<CommandOutcome, String> {
        let tools = ProcessRunner::new();
        let wiring = Wiring {
            clock: self.shell.clock.clone(),
            ids: &self.shell.ids,
            drivers: &self.shell.drivers,
            tools: &tools,
            faults: &NoFaults,
            runner: "test-runner".to_owned(),
            author: Some(AUTHOR.parse().unwrap()),
            process_env: self.shell.process_env.clone(),
            cancellation: Cancellation::default(),
            prompt: None,
            observer: None,
        };
        let argv = std::iter::once("nyl".to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .chain(std::iter::once(clone.display().to_string()));
        let cli = Cli::try_parse_from(argv).map_err(|error| error.to_string())?;
        cli.execute_in_process(&wiring).map_err(|error| error.to_string())
    }

    /// `nyl <args>` in-process in a fresh clone of `main`, which it returns.
    fn invoke_in_clone(&self, args: &[&str]) -> (CommandOutcome, PathBuf) {
        let clone = self.shell.world.fresh_clone("main");
        let outcome = self.invoke_at(&clone, args).unwrap();
        (outcome, clone)
    }

    fn invoke(&self, args: &[&str]) -> CommandOutcome {
        self.invoke_in_clone(args).0
    }

    /// `nyl reconcile -e dev -o json <args>`.
    fn reconcile(&self, args: &[&str]) -> CommandOutcome {
        let mut argv = vec!["reconcile", "-e", "dev", "-o", "json"];
        argv.extend_from_slice(args);
        self.invoke(&argv)
    }

    /// Initialize `dev` and reconcile both units once.
    fn reconciled(drift_repair: Option<&str>) -> Self {
        let harness = Self::new(drift_repair);
        assert_eq!(harness.invoke(&["state", "init", "-e", "dev"]).exit, 0);
        let first = harness.reconcile(&[]);
        assert_eq!(first.exit, 0, "{}", first.stderr);
        assert_eq!(harness.runs("api"), 1);
        assert_eq!(harness.runs("job"), 1);
        harness
    }

    /// A change outside Nyl: the unit's `verify` reports drift from now on,
    /// until its command runs again.
    fn drift(&self, unit: &str) {
        std::fs::write(self.markers.path().join(format!("{unit}.drift")), "").unwrap();
    }

    /// `nyl verify -e dev`, which records the drift observations.
    fn verify(&self) -> CommandOutcome {
        self.invoke(&["verify", "-e", "dev", "-o", "json"])
    }

    /// How often the unit's command ran.
    fn runs(&self, unit: &str) -> usize {
        std::fs::read_to_string(self.markers.path().join(format!("{unit}.runs")))
            .map(|runs| runs.lines().count())
            .unwrap_or(0)
    }

    fn tip(&self) -> String {
        git_output(&self.shell.world.bare(), &["rev-parse", OBSERVED])
    }

    fn message(&self) -> String {
        git_output(&self.shell.world.bare(), &["log", "-1", "--format=%B", OBSERVED])
    }
}

fn unit<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["environments"][0]["units"][name]
}

/// Under the default `driftRepair: converge`: `verify` exits 3 and its fix
/// says what the next reconcile does per unit; `status` shows both units
/// `drifted` (exit 2); `reconcile` executes the idempotent unit (summary
/// `driftRepaired`) and reports the other `awaiting-approval` (exit 2)
/// while the run goes on; the next reconcile writes nothing for the
/// repaired unit; a `--local --approve` run repairs the other with the
/// local approval record.
#[test]
fn test_reconcile_repairs_converge_units_and_awaits_approval_for_others() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    harness.drift("job");

    let verified = harness.verify();
    assert_eq!(verified.exit, 3, "{}", verified.stderr);
    assert_eq!(unit(&verified.report, "api")["verification"], "drifted");
    assert!(
        verified
            .stderr
            .contains("the next `nyl reconcile -e dev` repairs it by executing api again"),
        "{}",
        verified.stderr
    );
    assert!(
        verified
            .stderr
            .contains("run `nyl reconcile -e dev --local --approve job`, or wait for remote approvals"),
        "{}",
        verified.stderr
    );

    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 2, "{}", status.stderr);
    for name in ["api", "job"] {
        assert_eq!(unit(&status.report, name)["state"], "drifted", "{name}");
        assert_eq!(unit(&status.report, name)["drift"]["result"], "drifted", "{name}");
    }

    let plan = harness.invoke(&["plan", "-e", "dev", "-o", "json"]);
    assert_eq!(plan.exit, 0, "{}", plan.stderr);

    let repaired = harness.reconcile(&[]);
    assert_eq!(repaired.exit, 2, "{}", repaired.stderr);
    assert_eq!(unit(&repaired.report, "api")["result"], "executed");
    assert_eq!(unit(&repaired.report, "job")["result"], "awaiting-approval");
    assert_eq!((harness.runs("api"), harness.runs("job")), (2, 1));
    let message = harness.message();
    assert!(message.contains("driftRepaired: [api]"), "{message}");
    let after_repair = harness.tip();

    let again = harness.reconcile(&[]);
    assert_eq!(again.exit, 2, "{}", again.stderr);
    assert_eq!(unit(&again.report, "api")["result"], "current");
    assert_eq!(harness.runs("api"), 2, "a repaired unit is not repaired again");
    assert_eq!(harness.tip(), after_repair, "nothing to write for the repaired unit");
    let reverified = harness.verify();
    assert_eq!(unit(&reverified.report, "api")["verification"], "clean");

    let (local, clone) =
        harness.invoke_in_clone(&["reconcile", "-e", "dev", "--local", "--approve", "job", "-o", "json"]);
    assert_eq!(local.exit, 0, "{}", local.stderr);
    assert_eq!(unit(&local.report, "job")["result"], "executed");
    assert_eq!(harness.runs("job"), 2);
    let local_message = git_output(&clone, &["log", "-1", "--format=%B", "refs/heads/nyl/dev/observed"]);
    assert!(local_message.contains("driftRepaired: [job]"), "{local_message}");
    assert!(local_message.contains(r#""identity":"local""#), "{local_message}");
}

/// Under `driftRepair: manual`, reconcile executes no drifted unit, even an
/// idempotent one, and reports it `drifted`; `status` keeps exiting 2.
#[test]
fn test_manual_drift_repair_leaves_drifted_units() {
    let harness = Harness::reconciled(Some("manual"));
    harness.drift("api");
    assert_eq!(harness.verify().exit, 3);

    let reconciled = harness.reconcile(&[]);
    assert_eq!(reconciled.exit, 2, "{}", reconciled.stderr);
    assert_eq!(unit(&reconciled.report, "api")["result"], "drifted");
    assert_eq!(harness.runs("api"), 1);

    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 2, "{}", status.stderr);
    assert_eq!(unit(&status.report, "api")["state"], "drifted");
}

/// `--unit … --reapply --reason <text>` executes a current, clean unit
/// again and records who asked and why under `reapplied` and in the
/// trailers; a named unit that is not safe to re-run awaits approval in a
/// remote run, and the run goes on (exit 2).
#[test]
fn test_reapply_forces_named_units_under_the_approval_rule() {
    let harness = Harness::reconciled(Some("manual"));
    let reapplied = harness.reconcile(&["--unit", "api,job", "--reapply", "--reason", "rotate credentials"]);
    assert_eq!(reapplied.exit, 2, "{}", reapplied.stderr);
    assert_eq!(unit(&reapplied.report, "api")["result"], "executed");
    assert_eq!(unit(&reapplied.report, "job")["result"], "awaiting-approval");
    assert_eq!((harness.runs("api"), harness.runs("job")), (2, 1));
    let message = harness.message();
    let reapplied_line = message
        .lines()
        .find(|line| line.starts_with("reapplied: "))
        .unwrap_or_else(|| panic!("{message}"));
    let reapplied: Value = serde_json::from_str(&reapplied_line["reapplied: ".len()..]).unwrap();
    assert_eq!(reapplied["by"], AUTHOR);
    assert_eq!(reapplied["reason"], "rotate credentials");
    assert_eq!(reapplied["units"], serde_json::json!(["api"]));
    assert!(message.contains("Nyl-Requested-By: "), "{message}");
    assert!(message.contains("Nyl-Reason: rotate credentials"), "{message}");
}

/// `--reapply` names its units with `--unit` and needs a `--reason`; the
/// parser refuses it otherwise, before anything runs.
#[test]
fn test_reapply_requires_unit_and_reason() {
    let harness = Harness::new(None);
    let clone = harness.shell.world.fresh_clone("main");
    for args in [
        &["reconcile", "-e", "dev", "--reapply", "--reason", "x"][..],
        &["reconcile", "-e", "dev", "--unit", "api", "--reapply"][..],
    ] {
        let error = harness.invoke_at(&clone, args).unwrap_err();
        assert!(error.contains("required arguments were not provided"), "{error}");
    }
}

/// `nyl plan` lists a drifted unit selection would repair as `drifted`
/// among the units it would execute.
#[test]
fn test_plan_lists_a_drifted_unit_as_drifted() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    assert_eq!(harness.verify().exit, 3);
    let plan = harness.invoke(&["plan", "-e", "dev", "-o", "json"]);
    assert_eq!(plan.exit, 0, "{}", plan.stderr);
    assert_eq!(unit(&plan.report, "api")["state"], "drifted");
}

/// `driftRepaired` lists only the units the run executed again with a new
/// receipt: a repair whose execution fails stays out of it, and its drift
/// observation stays. Catches a commit history recording a drift as
/// repaired that is not.
#[test]
fn test_a_failed_repair_is_not_recorded_as_repaired() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    assert_eq!(harness.verify().exit, 3);
    // The repair's append to its run log fails.
    let runs = harness.markers.path().join("api.runs");
    std::fs::remove_file(&runs).unwrap();
    std::fs::create_dir(&runs).unwrap();

    let failed = harness.reconcile(&[]);
    assert_eq!(unit(&failed.report, "api")["result"], "failed", "{:#}", failed.report);
    assert!(
        failed.report["environments"][0]["driftRepaired"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "{:#}",
        failed.report
    );
    let message = harness.message();
    assert!(!message.contains("driftRepaired"), "{message}");
    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(
        unit(&status.report, "api")["drift"]["result"],
        "drifted",
        "{:#}",
        status.report
    );
}

/// `--fail-fast` starts no unit after the first approval wait, also one
/// decided at selection: a drifted unit that is not safe to re-run awaits
/// approval, and the drifted `converge` unit it would otherwise repair is
/// not started (NYL-RUN-STOPPED). Without `--fail-fast` the run goes on.
#[test]
fn test_fail_fast_stops_at_an_approval_wait_decided_at_selection() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    harness.drift("job");
    assert_eq!(harness.verify().exit, 3);

    let stopped = harness.reconcile(&["--fail-fast"]);
    assert_eq!(
        unit(&stopped.report, "job")["result"],
        "awaiting-approval",
        "{:#}",
        stopped.report
    );
    assert_eq!(
        unit(&stopped.report, "api")["result"],
        "blocked",
        "{:#}",
        stopped.report
    );
    assert_eq!(
        stopped.report["environments"][0]["warnings"][0]["code"], "NYL-RUN-STOPPED",
        "{:#}",
        stopped.report
    );
    assert_eq!(harness.runs("api"), 1, "api is not started");

    let continued = harness.reconcile(&[]);
    assert_eq!(
        unit(&continued.report, "api")["result"],
        "executed",
        "{:#}",
        continued.report
    );
    assert_eq!(harness.runs("api"), 2);
}
