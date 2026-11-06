//! Attested evidence for dependents, end to end through the orchestration
//! shell with Command units and Git state: a `dependsOn` entry or `fromUnit`
//! that requires the producer's attestations keeps its consumer `blocked`
//! until the producer's current receipt has them passing, whether `nyl
//! attest` records them or the producer's own `command` reports them.
//!
//! Contract: [Units](../../design/orchestration-core.md#units) and
//! [Attestations](../../design/orchestration-core.md#attestations).

mod scenario;

use scenario::shell::{self, project, ENVIRONMENT, NYL_TOML};
use serde_json::Value;
use tempfile::TempDir;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";

fn unit<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["environments"][0]["units"][name]
}

/// `svc` attests `smoke` with the content of `<results>/smoke.json` when
/// `nyl attest` asks, and declares `qa`, which only people supply. `ship`
/// waits for `smoke` through `dependsOn`; `reader` waits for it through a
/// reference.
struct Harness {
    shell: shell::Harness,
    results: TempDir,
    _project: TempDir,
}

impl Harness {
    fn new(extra_units: &str) -> Self {
        let results = TempDir::new().unwrap();
        // `sh` reads backslashes as escapes; Windows accepts forward slashes.
        let dir = results.path().display().to_string().replace('\\', "/");
        let units = format!(
            "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: svc
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'printf \"{{\\\"greeting\\\": \\\"hi\\\"}}\" > \"$NYL_OUTPUTS\"']
  attest: [sh, -c, 'cp {dir}/smoke.json \"$NYL_ATTESTATIONS\"']
  attestations: [{{name: smoke}}, {{name: qa, from: external}}]
  outputs: {{greeting: {{type: string}}}}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: ship
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  dependsOn: [{{unit: svc, attestations: [smoke]}}]
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: reader
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  values:
    greeting: {{fromUnit: {{unit: svc, output: greeting, attestations: [smoke]}}}}
{extra_units}"
        );
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", ENVIRONMENT),
            ("nyl/units.yaml", &units),
        ]);
        let harness = Self {
            shell: shell::Harness::new(project.path(), true),
            results,
            _project: project,
        };
        assert_eq!(harness.shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
        harness
    }

    fn result(&self, result: &str) {
        std::fs::write(
            self.results.path().join("smoke.json"),
            format!("{{\"smoke\": \"{result}\"}}"),
        )
        .unwrap();
    }
}

fn blocked_by(report: &Value, name: &str) -> Vec<(String, String, String)> {
    unit(report, name)["blockedBy"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    (
                        entry["pointer"].as_str().unwrap().to_owned(),
                        entry["reason"].as_str().unwrap().to_owned(),
                        format!(
                            "{}/{}",
                            entry["unit"].as_str().unwrap(),
                            entry["attestation"].as_str().unwrap()
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A consumer's whole path from a pending attestation to a satisfied one:
/// pending after the producer's execution (exit 2, one warning with the
/// fix), failing while the recorded attestation fails, ready once `nyl
/// attest` records a pass, and pending again for a new receipt of the
/// producer without executing the consumer again. Catches a requirement
/// silently met by a bare receipt, a failing attestation not blocking, a
/// stale pass counting for a new receipt, and a consumer re-executing only
/// because its producer was re-attested.
#[test]
fn test_consumers_wait_for_attestations_of_the_current_receipt() {
    let harness = Harness::new("");
    let shell = &harness.shell;

    let first = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(first.exit, 2, "{}", first.stderr);
    assert_eq!(unit(&first.report, "svc")["result"], "executed");
    for waiting in ["ship", "reader"] {
        assert_eq!(unit(&first.report, waiting)["result"], "blocked", "{waiting}");
    }
    assert_eq!(
        blocked_by(&first.report, "ship"),
        [(
            "/dependsOn/0".to_owned(),
            "attestation-pending".to_owned(),
            "svc/smoke".to_owned()
        )]
    );
    assert_eq!(
        blocked_by(&first.report, "reader"),
        [(
            "/values/greeting".to_owned(),
            "attestation-pending".to_owned(),
            "svc/smoke".to_owned()
        )]
    );
    assert!(first.stderr.contains("NYL-ATTESTATION-PENDING"), "{}", first.stderr);
    assert!(first.stderr.contains("nyl attest -e dev"), "{}", first.stderr);

    // A plan and a status show the same blocker.
    for command in ["plan", "status"] {
        let shown = shell.invoke(&[command, "-e", "dev", "-o", "json"]);
        assert_eq!(shown.exit, 2, "{command}: {}", shown.stderr);
        assert_eq!(unit(&shown.report, "ship")["state"], "blocked", "{command}");
        assert_eq!(
            blocked_by(&shown.report, "ship")[0].1,
            "attestation-pending",
            "{command}"
        );
        assert!(
            shown.stderr.contains("NYL-ATTESTATION-PENDING"),
            "{command}: {}",
            shown.stderr
        );
    }

    // A failing attestation blocks too, with its own code.
    harness.result("fail");
    assert_eq!(shell.invoke(&["attest", "-e", "dev"]).exit, 3);
    let failing = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(failing.exit, 2, "{}", failing.stderr);
    assert_eq!(blocked_by(&failing.report, "ship")[0].1, "attestation-failing");
    assert!(failing.stderr.contains("NYL-ATTESTATION-FAILING"), "{}", failing.stderr);

    // A newer passing attestation unblocks both in the next reconcile.
    harness.result("pass");
    assert_eq!(shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    let passing = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(passing.exit, 0, "{}", passing.stderr);
    for executed in ["ship", "reader"] {
        assert_eq!(unit(&passing.report, executed)["result"], "executed", "{executed}");
    }
    assert_eq!(unit(&passing.report, "svc")["result"], "current");
    let tip = shell.tip(OBSERVED);
    assert_eq!(shell.invoke(&["reconcile", "-e", "dev"]).exit, 0);
    assert_eq!(shell.tip(OBSERVED), tip, "nothing left to do writes nothing");

    // A new receipt of the producer starts pending again, and its old pass is
    // dropped in the same commit. The consumers' own inputs did not change,
    // so once re-attested they are current without executing again.
    shell.world.commit_files(
        "main",
        "Change svc",
        &[(
            "nyl/units.yaml",
            &std::fs::read_to_string(shell.world.fresh_clone("main").join("nyl/units.yaml"))
                .unwrap()
                .replacen("\\\"hi\\\"", "\\\"hello\\\"", 1),
        )],
    );
    let again = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(again.exit, 2, "{}", again.stderr);
    assert_eq!(unit(&again.report, "svc")["result"], "executed");
    assert_eq!(blocked_by(&again.report, "ship")[0].1, "attestation-pending");
    assert!(
        !shell
            .files(OBSERVED)
            .lines()
            .any(|file| file == "observed/units/svc/attestations.yaml"),
        "the replaced receipt's attestations are gone: {}",
        shell.files(OBSERVED)
    );
    assert_eq!(shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    let settled = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(settled.exit, 0, "{}", settled.stderr);
    assert_eq!(unit(&settled.report, "svc")["result"], "current");
    // `reader` resolves the new greeting, so only it executes again.
    assert_eq!(unit(&settled.report, "reader")["result"], "executed");
    assert_eq!(unit(&settled.report, "ship")["result"], "current");
}

/// Catches an attestation nobody can record waiting forever: a requirement
/// naming something the producer does not declare fails validation and every
/// run with `NYL-ATTESTATION-UNDECLARED` and the names the producer declares,
/// before any unit executes.
#[test]
fn test_requiring_an_undeclared_attestation_is_an_error_before_anything_runs() {
    let harness = Harness::new(
        "---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: typo
  labels: {dev: 'true'}
spec:
  command: [sh, -c, 'true']
  dependsOn: [{unit: svc, attestations: [smok]}]
",
    );
    let planned = harness.shell.invoke(&["reconcile", "-e", "dev"]);
    assert_eq!(planned.exit, 1, "{}", planned.stderr);
    assert!(
        planned.stderr.contains("NYL-ATTESTATION-UNDECLARED"),
        "{}",
        planned.stderr
    );
    assert!(planned.stderr.contains("qa, smoke"), "{}", planned.stderr);
}

/// Catches a consumer that waits forever for an attestation only a person or
/// an external system could supply, which Nyl cannot record yet: a name
/// declared `from: external`, required by name or through `evidence:
/// attested`, fails every run with `NYL-ATTESTATION-EXTERNAL-UNSUPPORTED`
/// before anything executes, and the fix names what can be required instead.
#[test]
fn test_requiring_an_external_attestation_is_an_error_before_anything_runs() {
    for requirement in ["attestations: [qa]", "evidence: attested"] {
        let harness = Harness::new(&format!(
            "---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: gated
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  dependsOn: [{{unit: svc, {requirement}}}]
"
        ));
        let planned = harness.shell.invoke(&["reconcile", "-e", "dev"]);
        assert_eq!(planned.exit, 1, "{requirement}: {}", planned.stderr);
        assert!(
            planned.stderr.contains("NYL-ATTESTATION-EXTERNAL-UNSUPPORTED"),
            "{requirement}: {}",
            planned.stderr
        );
        assert!(
            planned.stderr.contains("cannot be recorded yet") && planned.stderr.contains("attestations: [smoke]"),
            "{requirement}: {}",
            planned.stderr
        );
    }
}

/// A producer that attests itself in `command` unblocks its consumer in a
/// later wave of the same run, records the attestation with `source: {driver:
/// reconcile}` for the receipt it wrote, and, when the result is `fail`, keeps
/// the consumer blocked with `NYL-ATTESTATION-FAILING`. Catches a driver's
/// reconcile attestations being dropped by the shell, and a pass written for
/// another receipt being counted.
#[test]
fn test_a_producer_attesting_during_reconcile_unblocks_its_consumer_in_the_same_run() {
    let results = TempDir::new().unwrap();
    let dir = results.path().display().to_string().replace('\\', "/");
    let units = format!(
        "apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: checked
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'cp {dir}/ok.json \"$NYL_ATTESTATIONS\"']
  attestations: [{{name: ok}}]
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: after
  labels: {{dev: 'true'}}
spec:
  command: [sh, -c, 'true']
  dependsOn: [{{unit: checked, evidence: attested}}]
"
    );
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", ENVIRONMENT),
        ("nyl/units.yaml", &units),
    ]);
    let shell = shell::Harness::new(project.path(), true);
    assert_eq!(shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);

    std::fs::write(results.path().join("ok.json"), "{\"ok\": \"fail\"}").unwrap();
    let failing = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(failing.exit, 2, "{}", failing.stderr);
    assert_eq!(unit(&failing.report, "checked")["result"], "executed");
    assert_eq!(unit(&failing.report, "after")["result"], "blocked");
    assert_eq!(blocked_by(&failing.report, "after")[0].1, "attestation-failing");
    let record = shell.show(OBSERVED, "observed/units/checked/attestations.yaml");
    assert!(
        record.contains("driver: reconcile") && record.contains("result: fail"),
        "{record}"
    );

    // The unit executes again (a new input), reports a pass, and its consumer
    // runs in wave 2 of that same run.
    std::fs::write(results.path().join("ok.json"), "{\"ok\": \"pass\"}").unwrap();
    shell.world.commit_files(
        "main",
        "Change checked",
        &[("nyl/units.yaml", &units.replacen("cp ", "true; cp ", 1))],
    );
    let passing = shell.invoke(&["reconcile", "-e", "dev", "-o", "json"]);
    assert_eq!(passing.exit, 0, "{}", passing.stderr);
    assert_eq!(unit(&passing.report, "checked")["result"], "executed");
    assert_eq!(unit(&passing.report, "after")["result"], "executed");
    let record = shell.show(OBSERVED, "observed/units/checked/attestations.yaml");
    assert!(
        record.contains("result: pass") && !record.contains("result: fail"),
        "{record}"
    );
}

/// The `attested-evidence` walkthrough, which `scenario_harness_test` runs
/// once the harness reaches M5, runs here already: its steps are the contract
/// of the rules above, in the scenario format.
#[test]
fn test_the_attested_evidence_walkthrough_passes() {
    let (loaded, problems) = scenario::load_all();
    assert!(problems.is_empty(), "invalid scenarios:\n{}", problems.join("\n"));
    let walkthrough = loaded
        .iter()
        .find(|loaded| loaded.name == "walkthroughs/attested-evidence")
        .expect("the walkthrough exists");
    if let Err(failure) = scenario::runner::run(walkthrough) {
        panic!("{failure}");
    }
}
