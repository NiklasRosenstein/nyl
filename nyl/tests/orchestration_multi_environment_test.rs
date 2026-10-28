//! Multi-environment commands use independent Git transactions, in producer order.
mod scenario;

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_drivers::process::ProcessRunner;
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use scenario::runner::AUTHOR;
use scenario::shell::{self, project, NYL_TOML};
use serde_json::Value;
use tempfile::TempDir;

const PRODUCER: &str = r#"apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: network
  labels: {scope: infra}
spec:
  command:
    - sh
    - -c
    - |
      printf '%s' '{"id":"vpc-one"}' > "$NYL_OUTPUTS"
  idempotent: true
  teardown: [sh, -c, 'true']
  verify: [sh, -c, 'true']
  outputs:
    id: {type: string}
"#;
const CONSUMER: &str = r#"apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: app
  labels: {scope: app}
spec:
  command: [sh, -c, 'true']
  idempotent: true
  verify: [sh, -c, 'true']
  values:
    network: {fromUnit: {environment: infra, unit: network, output: id}}
"#;

struct Harness {
    shell: shell::Harness,
    _project: TempDir,
}

impl Harness {
    fn new() -> Self {
        let environment = |name: &str| {
            format!("apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: {name}\n  labels: {{stage: test}}\nspec:\n  unitSelector:\n    matchLabels: {{scope: {name}}}\n  allowUnprotectedSource: true\n")
        };
        let project = project(&[
            NYL_TOML,
            ("nyl/infra.yaml", &environment("infra")),
            ("nyl/app.yaml", &environment("app")),
            ("nyl/network.yaml", PRODUCER),
            ("nyl/consumer.yaml", CONSUMER),
            ("nyl/group.yaml", "apiVersion: gitops.nyl/v1\nkind: EnvironmentGroup\nmetadata: {name: testing}\nspec:\n  environmentSelector:\n    matchLabels: {stage: test}\n"),
        ]);
        let harness = Self {
            shell: shell::Harness::new(project.path(), true),
            _project: project,
        };
        for name in ["infra", "app"] {
            let initialized = harness.invoke(&["state", "init", "-e", name]);
            assert_eq!(initialized.exit, 0, "{}", initialized.stderr);
        }
        harness
    }

    fn invoke(&self, args: &[&str]) -> CommandOutcome {
        let clone = self.shell.world.fresh_clone("main");
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
        Cli::try_parse_from(argv).unwrap().execute_in_process(&wiring).unwrap()
    }
}

fn section<'a>(outcome: &'a CommandOutcome, name: &str) -> &'a Value {
    outcome.report["environments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|environment| environment["name"] == name)
        .unwrap()
}

#[test]
fn test_cross_environment_first_run_blocks_then_chain_converges_and_verifies() {
    let harness = Harness::new();
    let waiting = harness.invoke(&["reconcile", "-e", "app", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(section(&waiting, "app")["units"]["app"]["result"], "blocked");
    let run = harness.invoke(&["reconcile", "-e", "app", "-e", "infra", "-o", "json"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(run.report["environments"][0]["name"], "infra");
    assert_eq!(section(&run, "app")["units"]["app"]["result"], "executed");
    let again = harness.invoke(&["reconcile", "-l", "stage=test", "-o", "json"]);
    assert_eq!(again.exit, 0, "{}", again.stderr);
    assert_eq!(section(&again, "app")["units"]["app"]["result"], "current");
    assert!(section(&again, "app").get("commits").is_none());
    let verified = harness.invoke(&["verify", "-g", "testing", "-o", "json"]);
    assert_eq!(verified.exit, 0, "{}", verified.stderr);
    assert_eq!(verified.report["environments"].as_array().unwrap().len(), 2);
}

#[test]
fn test_multi_environment_plan_blocks_consumers_on_planned_producer_changes_and_protects_teardown() {
    let harness = Harness::new();
    let initial = harness.invoke(&["reconcile", "-g", "testing"]);
    assert_eq!(initial.exit, 0, "{}", initial.stderr);
    harness.shell.world.commit_files(
        "main",
        "change network",
        &[("nyl/network.yaml", &PRODUCER.replace("vpc-one", "vpc-two"))],
    );
    let plan = harness.invoke(&["plan", "-e", "app", "-e", "infra", "-o", "json"]);
    assert_eq!(plan.exit, 2, "{}", plan.stderr);
    let app = &section(&plan, "app")["units"]["app"];
    assert_eq!(app["state"], "blocked");
    assert!(
        app["blockedBy"]
            .to_string()
            .contains("infra/network has a planned change"),
        "{app}"
    );
    let teardown = harness.invoke(&["teardown", "-e", "infra", "--unit", "network", "-o", "json"]);
    assert_eq!(teardown.exit, 2, "{}", teardown.stderr);
    assert!(teardown.stderr.contains("app/app"), "{}", teardown.stderr);
}

#[test]
fn test_multi_environment_unit_options_require_selected_environment_qualification() {
    let harness = Harness::new();
    let refused = harness.invoke(&["reconcile", "-g", "testing", "--unit", "network", "-o", "json"]);
    assert_eq!(refused.exit, 1, "{}", refused.stderr);
    let selected = harness.invoke(&["reconcile", "-g", "testing", "--unit", "infra/network", "-o", "json"]);
    assert!(selected.exit <= 2, "{}", selected.stderr);
    assert_eq!(section(&selected, "infra")["units"]["network"]["result"], "executed");
    assert_ne!(section(&selected, "app")["units"]["app"]["result"], "executed");
}

#[test]
fn test_an_unavailable_selected_source_reports_its_error_and_independent_environments_still_execute() {
    let harness = Harness::new();
    let infra = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {name: infra}\nspec:\n  unitSelector:\n    matchLabels: {scope: infra}\n  source: {revision: refs/heads/missing}\n  allowUnprotectedSource: true\n";
    let solo = "apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {name: solo}\nspec:\n  unitSelector:\n    matchLabels: {scope: solo}\n  allowUnprotectedSource: true\n";
    let unit = CONSUMER
        .replace("name: app", "name: solo")
        .replace("scope: app", "scope: solo")
        .replace(
            "  values:\n    network: {fromUnit: {environment: infra, unit: network, output: id}}\n",
            "",
        );
    harness.shell.world.commit_files(
        "main",
        "add independent environment and unavailable source",
        &[
            ("nyl/infra.yaml", infra),
            ("nyl/solo.yaml", solo),
            ("nyl/solo-unit.yaml", &unit),
        ],
    );
    let initialized = harness.invoke(&["state", "init", "-e", "solo"]);
    assert_eq!(initialized.exit, 0, "{}", initialized.stderr);
    let run = harness.invoke(&["reconcile", "-e", "infra", "-e", "app", "-e", "solo", "-o", "json"]);
    assert_eq!(run.exit, 1, "{}", run.stderr);
    assert!(!section(&run, "infra")["errors"].as_array().unwrap().is_empty());
    assert_eq!(section(&run, "app")["units"]["app"]["result"], "blocked");
    assert_eq!(section(&run, "solo")["units"]["solo"]["result"], "executed");
    assert_eq!(run.report["environments"].as_array().unwrap().len(), 3);
}
