//! `nyl attest` end to end, in-process with Git state: a Command unit's
//! `attest` step reports a result read from a file outside the project, and
//! `attest` records it as an attestation of the unit's current receipt.
//!
//! Contract: [Attestations](../../design/orchestration-core.md#attestations)
//! and [Commands](../../design/orchestration-core.md#commands): `attest`
//! asks the drivers that attest for a unit's spec, records the results with
//! `source: {driver: attest}` in one observed commit, writes nothing when
//! nothing changed, and exits 3 for a failing attestation.

mod scenario;

use scenario::shell::{self, command, project, ENVIRONMENT, NYL_TOML};
use serde_json::Value;
use tempfile::TempDir;

const OBSERVED: &str = "refs/heads/nyl/dev/observed";

/// `svc` attests `smoke` with the content of `<results>/smoke.json`;
/// `plain` has no attest step.
struct Harness {
    shell: shell::Harness,
    results: TempDir,
    _project: TempDir,
}

impl Harness {
    fn reconciled() -> Self {
        let results = TempDir::new().unwrap();
        // `sh` reads backslashes as escapes; Windows accepts forward slashes.
        let dir = results.path().display().to_string().replace('\\', "/");
        let svc = command(
            "svc",
            "true",
            &format!(
                "  attest: [sh, -c, 'cp {dir}/smoke.json \"$NYL_ATTESTATIONS\"']\n  attestations: [{{name: smoke}}]\n"
            ),
        );
        let project = project(&[
            NYL_TOML,
            ("nyl/environment.yaml", ENVIRONMENT),
            ("nyl/svc.yaml", &svc),
            ("nyl/plain.yaml", &command("plain", "true", "")),
        ]);
        let harness = Self {
            shell: shell::Harness::new(project.path(), true),
            results,
            _project: project,
        };
        assert_eq!(harness.shell.invoke(&["state", "init", "-e", "dev"]).exit, 0);
        let reconciled = harness.shell.invoke(&["reconcile", "-e", "dev"]);
        assert_eq!(reconciled.exit, 0, "{}", reconciled.stderr);
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

fn unit<'a>(report: &'a Value, name: &str) -> &'a Value {
    &report["environments"][0]["units"][name]
}

/// Catches attestations not reaching state, a unit that cannot attest
/// making every `attest` incomplete, a repeated `attest` writing a commit
/// only because time moved on, and a failing attestation passing CI.
#[test]
fn test_attest_records_driver_attestations_of_current_receipts() {
    let harness = Harness::reconciled();
    harness.result("pass");
    let attested = harness.shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(attested.exit, 0, "{}", attested.stderr);
    assert_eq!(unit(&attested.report, "svc")["check"], "attested");
    assert_eq!(unit(&attested.report, "svc")["attestations"]["smoke"], "pass");
    assert!(
        unit(&attested.report, "plain").is_null(),
        "a unit that cannot attest is left out"
    );
    let record = harness
        .shell
        .show(OBSERVED, "observed/units/svc/attestations/smoke.yaml");
    assert!(record.contains("driver: attest"), "{record}");
    assert!(harness.shell.message(OBSERVED).starts_with("nyl: attest dev"));

    let tip = harness.shell.tip(OBSERVED);
    assert_eq!(harness.shell.invoke(&["attest", "-e", "dev"]).exit, 0);
    assert_eq!(harness.shell.tip(OBSERVED), tip, "nothing changed but the time");

    harness.result("fail");
    let failing = harness.shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(failing.exit, 3, "{}", failing.stderr);
    assert_eq!(unit(&failing.report, "svc")["attestations"]["smoke"], "fail");
    assert!(failing.stderr.contains("NYL-ATTESTATION-FAILED"), "{}", failing.stderr);

    let named = harness
        .shell
        .invoke(&["attest", "-e", "dev", "--unit", "plain", "-o", "json"]);
    assert_eq!(named.exit, 2, "{}", named.stderr);
    assert_eq!(unit(&named.report, "plain")["check"], "unsupported");
    assert!(named.stderr.contains("NYL-ATTEST-UNSUPPORTED"), "{}", named.stderr);
}
