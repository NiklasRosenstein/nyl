//! Multi-environment commands use independent Git transactions, in producer order.
mod scenario;

use clap::Parser;
use nyl::cli::Cli;
use nyl::orchestration::run::{CommandOutcome, Wiring};
use nyl_orchestration::fault::NoFaults;
use nyl_orchestration::shell::run::Cancellation;
use nyl_units::process::ProcessRunner;
use scenario::observing::{pending, value, Observing, Probe, Read};
use scenario::runner::AUTHOR;
use scenario::shell::{self, project, NYL_TOML};
use serde_json::{json, Value};
use std::sync::Arc;
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
  detectDrift: [sh, -c, 'true']
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
  detectDrift: [sh, -c, 'true']
  values:
    network: {fromUnit: {environment: infra, unit: network, output: id}}
"#;

struct Harness {
    shell: shell::Harness,
    _project: TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::with_units(PRODUCER, CONSUMER)
    }

    fn with_units(producer: &str, consumer: &str) -> Self {
        Self::build(producer, consumer, None)
    }

    /// Units whose Command driver reads what `probe` scripts for observed values.
    fn observing(producer: &str, consumer: &str, probe: &Arc<Probe>) -> Self {
        Self::build(producer, consumer, Some(probe))
    }

    fn build(producer: &str, consumer: &str, probe: Option<&Arc<Probe>>) -> Self {
        let environment = |name: &str| {
            format!("apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata:\n  name: {name}\n  labels: {{stage: test}}\nspec:\n  unitSelector:\n    matchLabels: {{scope: {name}}}\n  allowUnprotectedSource: true\n")
        };
        let project = project(&[
            NYL_TOML,
            ("nyl/infra.yaml", &environment("infra")),
            ("nyl/app.yaml", &environment("app")),
            ("nyl/network.yaml", producer),
            ("nyl/consumer.yaml", consumer),
            ("nyl/group.yaml", "apiVersion: gitops.nyl/v1\nkind: EnvironmentGroup\nmetadata: {name: testing}\nspec:\n  environmentSelector:\n    matchLabels: {stage: test}\n"),
        ]);
        let mut shell = shell::Harness::new(project.path(), true);
        if let Some(probe) = probe {
            shell.drivers.register(Arc::new(Observing {
                inner: nyl_units::command::CommandDriver,
                probe: probe.clone(),
            }));
        }
        let harness = Self {
            shell,
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
            progress: None,
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
    let verified = harness.invoke(&["detect-drift", "-g", "testing", "-o", "json"]);
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
    assert_eq!(
        app["blockedBy"],
        json!([{"pointer": "/values/network", "environment": "infra", "unit": "network", "reason": "planned-change"}]),
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

#[test]
fn test_busy_producer_blocks_old_receipts_but_independent_consumer_units_execute() {
    use nyl_core::orchestration::ids::{EnvironmentName, RunId};
    use nyl_core::orchestration::ports::Clock;
    use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, StateV1};

    let harness = Harness::new();
    assert_eq!(harness.invoke(&["reconcile", "-g", "testing"]).exit, 0);
    let independent = CONSUMER.replace("name: app", "name: independent").replace(
        "  values:\n    network: {fromUnit: {environment: infra, unit: network, output: id}}\n",
        "",
    );
    harness.shell.world.commit_files(
        "main",
        "change producer and consumer, add independent work",
        &[
            ("nyl/network.yaml", &PRODUCER.replace("vpc-one", "vpc-two")),
            ("nyl/consumer.yaml", &CONSUMER.replace("'true'", "'echo updated'")),
            ("nyl/independent.yaml", &independent),
        ],
    );
    let session = nyl::orchestration::session::Session::open(harness.shell.store_clone.as_ref().unwrap(), "infra")
        .ok()
        .expect("open infra session");
    let environment = EnvironmentName::new("infra").unwrap();
    let now = harness.shell.clock.now();
    let lease = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: RunId::new("00000000-0000-7000-9000-0000000000fe").unwrap(),
        runner: "another runner".to_owned(),
        operation: Operation::Reconcile,
        units: vec![],
        taken_at: now,
        deadline: now + chrono::Duration::minutes(30),
        broken: None,
    };
    harness
        .shell
        .store
        .take_lease(&session.resolved.location, &environment, &lease, None)
        .unwrap();
    let run = harness.invoke(&["reconcile", "-g", "testing", "-o", "json"]);
    assert_eq!(run.exit, 2, "{}", run.stderr);
    assert!(run.stderr.contains("NYL-LEASE-HELD"), "{}", run.stderr);
    assert_eq!(section(&run, "app")["units"]["app"]["result"], "blocked");
    assert_eq!(section(&run, "app")["units"]["independent"]["result"], "executed");
}

/// A consumer of another environment's unit that requires its attestation:
/// the report and the table name the producer with its environment, the
/// consumer runs after `attest` records a pass in the producer's
/// environment, and requirements no one can meet (a name the producer does
/// not declare, one only an external source supplies) fail the run with
/// their own codes. Catches a cross-environment wait that shows only a
/// pointer and a reason, and one that waits forever.
#[test]
fn test_a_consumer_waits_for_an_attestation_of_another_environments_unit() {
    let producer = PRODUCER.replace(
        "  outputs:\n    id: {type: string}\n",
        "  outputs:\n    id: {type: string}\n  attest: [sh, -c, 'printf \"{\\\"healthy\\\": \\\"pass\\\"}\" > \"$NYL_ATTESTATIONS\"']\n  attestations: [{name: healthy}, {name: qa, from: external}]\n",
    );
    assert!(producer.contains("attestations:"), "{producer}");
    let consumer = |requirement: &str| {
        CONSUMER.replace(
            "  values:\n",
            &format!("  dependsOn: [{{environment: infra, unit: network, {requirement}}}]\n  values:\n"),
        )
    };
    let harness = Harness::with_units(&producer, &consumer("attestations: [healthy]"));
    let waiting = harness.invoke(&["reconcile", "-e", "infra", "-e", "app", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(
        section(&waiting, "app")["units"]["app"]["blockedBy"],
        serde_json::json!([{
            "pointer": "/dependsOn/0",
            "environment": "infra",
            "unit": "network",
            "reason": "attestation-pending",
            "attestation": "healthy",
        }])
    );
    let shown = harness.invoke(&["status", "-e", "app"]);
    assert!(
        shown.stdout.contains("attestation healthy of infra/network"),
        "{}",
        shown.stdout
    );
    assert!(shown.stderr.contains("NYL-ATTESTATION-PENDING"), "{}", shown.stderr);
    assert_eq!(harness.invoke(&["attest", "-e", "infra"]).exit, 0);
    let run = harness.invoke(&["reconcile", "-e", "app", "-o", "json"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(section(&run, "app")["units"]["app"]["result"], "executed");

    for (requirement, code) in [
        ("attestations: [helthy]", "NYL-ATTESTATION-UNDECLARED"),
        ("attestations: [qa]", "NYL-ATTESTATION-EXTERNAL-UNSUPPORTED"),
        ("evidence: attested", "NYL-ATTESTATION-EXTERNAL-UNSUPPORTED"),
    ] {
        let harness = Harness::with_units(&producer, &consumer(requirement));
        let rejected = harness.invoke(&["reconcile", "-e", "infra", "-e", "app"]);
        assert_eq!(rejected.exit, 1, "{requirement}: {}", rejected.stderr);
        assert!(rejected.stderr.contains(code), "{requirement}: {}", rejected.stderr);
    }
}

/// A consumer in one environment waits for an observed value of a unit in
/// another; once `nyl observe` records the value, both commands name the
/// consumer, and it proceeds.
///
/// Catches the cross-environment wait being stored in a form that loses the
/// producer's environment (the blocker read back from text, where
/// `<environment>/<unit>` is no unit name): the stored document names the
/// producer with its environment as typed data, `nyl observe -e infra` and
/// `nyl status -e app` recognize the waiting consumer as able to resolve, and
/// the consumer executes with the value after it arrives.
#[test]
fn test_a_consumer_waits_for_an_observed_value_of_another_environments_unit_then_proceeds() {
    let producer = PRODUCER.replace(
        "  outputs:\n    id: {type: string}\n",
        "  outputs:\n    id: {type: string}\n  observations:\n    ip: {type: string, from: {service: web}}\n",
    );
    assert!(producer.contains("observations:"), "{producer}");
    let consumer = CONSUMER.replace(
        "    network: {fromUnit: {environment: infra, unit: network, output: id}}\n",
        "    address: {fromUnit: {environment: infra, unit: network, observation: ip}}\n",
    );
    assert!(consumer.contains("observation: ip"), "{consumer}");
    let probe = Arc::new(Probe::default());
    let harness = Harness::observing(&producer, &consumer, &probe);

    probe.script("network", [Read::Values(vec![pending("ip", "no address yet")])]);
    let waiting = harness.invoke(&["reconcile", "-e", "infra", "-e", "app", "-o", "json"]);
    assert_eq!(waiting.exit, 2, "{}", waiting.stderr);
    assert_eq!(section(&waiting, "infra")["units"]["network"]["result"], "executed");
    assert_eq!(
        section(&waiting, "app")["units"]["app"]["blockedBy"],
        json!([{
            "pointer": "/values/address",
            "environment": "infra",
            "unit": "network",
            "reason": "observation-pending",
            "observation": "ip",
        }])
    );
    let shown = harness.invoke(&["status", "-e", "app"]);
    assert!(
        shown.stdout.contains("observed value ip of infra/network"),
        "{}",
        shown.stdout
    );
    let stored = harness
        .shell
        .show("refs/heads/nyl/app/desired", "desired/units/app.yaml");
    assert!(
        stored.contains("type: observation")
            && stored.contains("environment: infra")
            && stored.contains("name: ip")
            && stored.contains("why: no address yet"),
        "{stored}"
    );

    probe.script("network", [Read::Values(vec![value("ip", json!("203.0.113.10"))])]);
    let observed = harness.invoke(&["observe", "-e", "infra"]);
    assert_eq!(observed.exit, 0, "{}", observed.stderr);
    assert!(
        observed.stderr.contains("NYL-OBSERVATION-CHANGED")
            && observed
                .stderr
                .contains("observed value ip of infra/network has a value now, so app/app can resolve"),
        "{}",
        observed.stderr
    );
    let ready = harness.invoke(&["status", "-e", "app", "-o", "json"]);
    let unit = &section(&ready, "app")["units"]["app"];
    assert_eq!(unit["state"], "ready", "{}", ready.stdout);
    assert!(
        unit["message"]
            .as_str()
            .is_some_and(|message| message.contains("observed value ip of infra/network arrived")),
        "{unit}"
    );
    let run = harness.invoke(&["reconcile", "-e", "app", "-o", "json"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    assert_eq!(section(&run, "app")["units"]["app"]["result"], "executed");
    assert!(section(&run, "app")["units"]["app"].get("blockedBy").is_none());
}

/// `reconcile --detect-drift` over several environments checks each one inside
/// its own run, under its own lease, and records its records in its own commit.
/// Catches a check shared across environments or one that outlives its lease.
#[test]
fn test_detect_drift_checks_each_selected_environment_under_its_own_lease() {
    let harness = Harness::new();
    let converged = harness.invoke(&["reconcile", "-e", "app", "-e", "infra", "-o", "json"]);
    assert_eq!(converged.exit, 0, "{}", converged.stderr);

    let run = harness.invoke(&["reconcile", "-l", "stage=test", "--detect-drift", "-o", "json"]);
    assert_eq!(run.exit, 0, "{}", run.stderr);
    for (environment, unit) in [("infra", "network"), ("app", "app")] {
        let section = section(&run, environment);
        assert_eq!(section["units"][unit]["driftCheck"]["check"], "clean", "{environment}");
        assert_eq!(section["units"][unit]["result"], "current", "{environment}");
        assert!(
            section["commits"]["observed"].is_string(),
            "{environment} wrote its records"
        );
    }
    assert_ne!(section(&run, "infra")["run"], section(&run, "app")["run"]);
}
