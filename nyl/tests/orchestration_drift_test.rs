//! Drift repair end to end, in-process with Git state: `nyl detect-drift` records
//! drift of Command units whose `detectDrift` step reads a marker file outside
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
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::runner::AUTHOR;
use scenario::shell::{self, command, project, ENVIRONMENT, NYL_TOML};
use scenario::world::git_output;
use serde_json::Value;
use tempfile::TempDir;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";

/// A project with two Command units whose `detectDrift` step reports drift while a
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
                    "  detectDrift: [sh, -c, 'test ! -e {dir}/{name}.drift || exit 2']\n{}",
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

    /// A project whose units are `units` (file name, document) next to the
    /// default Environment, for units whose drift check the markers do not drive.
    fn with_units(units: &[(&str, String)]) -> Self {
        let mut files = vec![NYL_TOML, ("nyl/environment.yaml", ENVIRONMENT)];
        files.extend(units.iter().map(|(name, document)| (*name, document.as_str())));
        let project = project(&files);
        let shell = shell::Harness::new(project.path(), true);
        Self {
            shell,
            markers: TempDir::new().unwrap(),
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
            progress: None,
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

    /// A change outside Nyl: the unit's `detectDrift` step reports drift from now on,
    /// until its command runs again.
    fn drift(&self, unit: &str) {
        std::fs::write(self.markers.path().join(format!("{unit}.drift")), "").unwrap();
    }

    /// `nyl detect-drift -e dev`, which records the drift records.
    fn detect_drift(&self) -> CommandOutcome {
        self.invoke(&["detect-drift", "-e", "dev", "-o", "json"])
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

    /// How many commits the observed ref holds.
    fn observed_commits(&self) -> usize {
        git_output(&self.shell.world.bare(), &["rev-list", "--count", OBSERVED])
            .trim()
            .parse()
            .unwrap()
    }

    /// A change repaired by hand: the unit's `detectDrift` step reports no drift.
    fn repair_by_hand(&self, unit: &str) {
        std::fs::remove_file(self.markers.path().join(format!("{unit}.drift"))).unwrap();
    }

    fn message(&self) -> String {
        git_output(&self.shell.world.bare(), &["log", "-1", "--format=%B", OBSERVED])
    }
}

fn unit<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["environments"][0]["units"][name]
}

/// Under the default `driftRepair: converge`: `detect-drift` exits 3 and its fix
/// says what the next reconcile does per unit; `status` shows both units
/// `drifted` (exit 2); `reconcile` executes the idempotent unit (summary
/// `driftRepaired`) and reports the other `awaiting-approval` (exit 2)
/// while the run goes on; the next reconcile writes nothing for the
/// repaired unit; a `--approve` run repairs the other with the
/// local approval record.
#[test]
fn test_reconcile_repairs_converge_units_and_awaits_approval_for_others() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    harness.drift("job");

    let detected = harness.detect_drift();
    assert_eq!(detected.exit, 3, "{}", detected.stderr);
    assert_eq!(unit(&detected.report, "api")["check"], "drifted");
    assert!(
        detected
            .stderr
            .contains("the next `nyl reconcile -e dev` repairs it by executing api again"),
        "{}",
        detected.stderr
    );
    assert!(
        detected.stderr.contains("run `nyl reconcile -e dev --approve job`"),
        "{}",
        detected.stderr
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
    let redetected = harness.detect_drift();
    assert_eq!(unit(&redetected.report, "api")["check"], "clean");

    let (local, _clone) =
        harness.invoke_in_clone(&["reconcile", "-e", "dev", "--local", "--approve", "job", "-o", "json"]);
    assert_eq!(local.exit, 0, "{}", local.stderr);
    assert_eq!(unit(&local.report, "job")["result"], "executed");
    assert_eq!(harness.runs("job"), 2);
    let local_message = git_output(
        &harness.shell.world.bare(),
        &["log", "-1", "--format=%B", "refs/heads/nyl/dev/observed"],
    );
    assert!(local_message.contains("driftRepaired: [job]"), "{local_message}");
}

/// Under `driftRepair: manual`, reconcile executes no drifted unit, even an
/// idempotent one, and reports it `drifted`; `status` keeps exiting 2.
#[test]
fn test_manual_drift_repair_leaves_drifted_units() {
    let harness = Harness::reconciled(Some("manual"));
    harness.drift("api");
    assert_eq!(harness.detect_drift().exit, 3);

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
    assert_eq!(harness.detect_drift().exit, 3);
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
    assert_eq!(harness.detect_drift().exit, 3);
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
    assert_eq!(harness.detect_drift().exit, 3);

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

fn warning_codes(report: &Value, name: &str) -> Vec<String> {
    unit(report, name)["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .map(|warning| warning["code"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `reconcile --detect-drift` checks the current units under its own lease and
/// repairs what drifted in the same run, writing the drift records and the new
/// receipts in one transition commit: the idempotent unit runs again and
/// leaves no record, the other awaits approval and keeps its record, which
/// `status` reads without another `detect-drift`. Catches a lease gap between
/// the check and the repair, a second commit, or a repair decided by another
/// rule than the next reconcile's.
#[test]
fn test_detect_drift_checks_then_repairs_in_one_commit() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    harness.drift("job");
    let before = harness.observed_commits();

    let run = harness.reconcile(&["--detect-drift"]);
    assert_eq!(run.exit, 2, "{}", run.stderr);
    for name in ["api", "job"] {
        assert_eq!(unit(&run.report, name)["driftCheck"]["check"], "drifted", "{name}");
    }
    assert_eq!(unit(&run.report, "api")["result"], "executed");
    assert_eq!(unit(&run.report, "job")["result"], "awaiting-approval");
    assert_eq!((harness.runs("api"), harness.runs("job")), (2, 1));
    assert_eq!(
        run.report["environments"][0]["driftRepaired"],
        serde_json::json!(["api"])
    );
    assert_eq!(harness.observed_commits(), before + 1, "one transition commit");
    let message = harness.message();
    assert!(message.contains("driftRepaired: [api]"), "{message}");
    assert!(message.contains(r#""driftCheck":{"check":"drifted""#), "{message}");
    // Repaired drift is reported by `driftRepaired`; the unit that waits is warned about.
    assert_eq!(warning_codes(&run.report, "api"), Vec::<String>::new());
    assert_eq!(warning_codes(&run.report, "job"), ["NYL-DRIFT-DETECTED"]);

    // The record the run wrote for `job` is state: status shows it with no further check.
    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 2, "{}", status.stderr);
    assert_eq!(unit(&status.report, "api")["state"], "current");
    assert_eq!(unit(&status.report, "job")["state"], "drifted");
    assert_eq!(unit(&status.report, "job")["drift"]["result"], "drifted");
    assert_eq!(unit(&status.report, "api")["drift"], Value::Null);
}

/// Without the flag a reconcile never detects drift, whatever changed outside
/// Nyl; with it, clean results are recorded once and a repeat run that finds
/// nothing new writes nothing.
#[test]
fn test_detect_drift_is_opt_in_and_records_clean_results_once() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    let before = harness.observed_commits();
    let plain = harness.reconcile(&[]);
    assert_eq!(plain.exit, 0, "{}", plain.stderr);
    assert_eq!(unit(&plain.report, "api")["driftCheck"], Value::Null);
    assert_eq!(harness.runs("api"), 1, "no detection, so nothing to repair");
    assert_eq!(harness.observed_commits(), before);
    harness.repair_by_hand("api");

    let first = harness.reconcile(&["--detect-drift"]);
    assert_eq!(first.exit, 0, "{}", first.stderr);
    for name in ["api", "job"] {
        assert_eq!(unit(&first.report, name)["driftCheck"]["check"], "clean", "{name}");
        assert_eq!(unit(&first.report, name)["result"], "current", "{name}");
    }
    assert_eq!(harness.observed_commits(), before + 1);
    let recorded = harness.tip();

    let second = harness.reconcile(&["--detect-drift"]);
    assert_eq!(second.exit, 0, "{}", second.stderr);
    assert_eq!(harness.tip(), recorded, "a check that found nothing new writes nothing");
}

/// The run's check replaces a stored `drifted` record, so a unit repaired by
/// hand since `nyl detect-drift` is not executed again on stale evidence.
#[test]
fn test_detect_drift_does_not_repair_what_was_repaired_by_hand() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    assert_eq!(harness.detect_drift().exit, 3);
    harness.repair_by_hand("api");

    let run = harness.reconcile(&["--detect-drift"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(unit(&run.report, "api")["driftCheck"]["check"], "clean");
    assert_eq!(unit(&run.report, "api")["result"], "current");
    assert_eq!(harness.runs("api"), 1);
    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(status.exit, 0, "{}", status.stderr);
    assert_eq!(unit(&status.report, "api")["drift"]["result"], "clean");
}

/// Under `driftRepair: manual` the check finds the drift and records it, and
/// the run repairs nothing: the unit is `drifted` and the warning names the
/// forced run.
#[test]
fn test_detect_drift_under_manual_repair_records_and_reports_the_drift() {
    let harness = Harness::reconciled(Some("manual"));
    harness.drift("api");
    let run = harness.reconcile(&["--detect-drift"]);
    assert_eq!(run.exit, 2, "{}", run.stderr);
    assert_eq!(unit(&run.report, "api")["driftCheck"]["check"], "drifted");
    assert_eq!(unit(&run.report, "api")["result"], "drifted");
    assert_eq!(harness.runs("api"), 1);
    assert_eq!(warning_codes(&run.report, "api"), ["NYL-DRIFT-DETECTED"]);
    assert!(run.stderr.contains("--reapply --reason"), "{}", run.stderr);
    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(unit(&status.report, "api")["drift"]["result"], "drifted");
}

/// `--unit` limits the check to the named units, as it limits execution:
/// another unit's drift is neither checked nor repaired.
#[test]
fn test_detect_drift_with_unit_checks_only_the_named_units() {
    let harness = Harness::reconciled(None);
    harness.drift("api");
    harness.drift("job");
    let run = harness.reconcile(&["--detect-drift", "--unit", "api"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(unit(&run.report, "api")["result"], "executed");
    let status = harness.invoke(&["status", "-e", "dev", "-o", "json"]);
    assert_eq!(unit(&status.report, "job")["drift"], Value::Null, "job was not checked");
}

/// `--detect-drift` is refused with `--leaving` and `--reapply`, which leave
/// it nothing to check, before the run takes the lease or writes anything.
#[test]
fn test_detect_drift_is_refused_with_options_that_leave_nothing_to_check() {
    let harness = Harness::reconciled(None);
    let before = harness.tip();
    for args in [
        &["--detect-drift", "--leaving"][..],
        &["--detect-drift", "--unit", "api", "--reapply", "--reason", "x"][..],
    ] {
        let refused = harness.reconcile(args);
        assert_eq!(refused.exit, 1, "{}", refused.stderr);
        assert_eq!(
            refused.report["environments"][0]["errors"][0]["code"], "NYL-DRIFT-OPTION-INVALID",
            "{:#}",
            refused.report
        );
        assert!(
            refused.report["environments"][0].get("run").is_none(),
            "no run took a lease"
        );
    }
    assert_eq!(harness.tip(), before);
}

/// A kind that cannot detect drift is not checked and not reported, so a
/// scheduled `reconcile --detect-drift` exits 0 in an environment with such a
/// unit; `--unit` naming it reports `unsupported`, as `detect-drift` does. A
/// drift check that cannot conclude (`unknown`) makes the run incomplete (2),
/// as `detect-drift` does. Both warn with the codes of `detect-drift`.
#[test]
fn test_detect_drift_leaves_out_kinds_that_cannot_detect_drift_and_counts_unknown() {
    let plain = command("plain", "true", "");
    let flaky = command("flaky", "true", "  detectDrift: [sh, -c, 'exit 1']\n");
    let harness = Harness::with_units(&[("nyl/plain.yaml", plain.clone())]);
    assert_eq!(harness.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    let first = harness.reconcile(&[]);
    assert_eq!(first.exit, 0, "{}", first.stderr);
    let run = harness.reconcile(&["--detect-drift"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(unit(&run.report, "plain")["driftCheck"], Value::Null);
    assert_eq!(warning_codes(&run.report, "plain"), Vec::<String>::new());
    let named = harness.reconcile(&["--detect-drift", "--unit", "plain"]);
    assert_eq!(named.exit, 0, "{}", named.stderr);
    assert_eq!(unit(&named.report, "plain")["driftCheck"]["check"], "unsupported");
    assert_eq!(warning_codes(&named.report, "plain"), ["NYL-DRIFT-UNSUPPORTED"]);

    let harness = Harness::with_units(&[("nyl/plain.yaml", plain), ("nyl/flaky.yaml", flaky)]);
    assert_eq!(harness.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    assert_eq!(harness.reconcile(&[]).exit, 0);
    let run = harness.reconcile(&["--detect-drift"]);
    assert_eq!(run.exit, 2, "{}", run.stderr);
    assert_eq!(unit(&run.report, "flaky")["driftCheck"]["check"], "unknown");
    assert_eq!(unit(&run.report, "flaky")["result"], "current");
    assert_eq!(warning_codes(&run.report, "flaky"), ["NYL-DRIFT-UNKNOWN"]);
    assert_eq!(unit(&run.report, "plain")["driftCheck"], Value::Null);
}
