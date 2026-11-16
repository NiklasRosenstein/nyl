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
/// only because time moved on, a failing attestation passing CI, and a
/// failing attest step being taken as an error or erasing recorded results.
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
    let record = harness.shell.show(OBSERVED, "observed/units/svc/attestations.yaml");
    assert!(
        record.contains("kind: AttestationRecord") && record.contains("smoke:"),
        "{record}"
    );
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

    // A step that cannot produce results is unknown: exit 2, and the
    // recorded attestation of the receipt stays.
    let tip = harness.shell.tip(OBSERVED);
    std::fs::remove_file(harness.results.path().join("smoke.json")).unwrap();
    let unknown = harness.shell.invoke(&["attest", "-e", "dev", "-o", "json"]);
    assert_eq!(unknown.exit, 2, "{}", unknown.stderr);
    assert_eq!(unit(&unknown.report, "svc")["check"], "unknown");
    assert!(unknown.stderr.contains("NYL-ATTEST-UNKNOWN"), "{}", unknown.stderr);
    assert_eq!(harness.shell.tip(OBSERVED), tip, "an unknown result records nothing");

    let named = harness
        .shell
        .invoke(&["attest", "-e", "dev", "--unit", "plain", "-o", "json"]);
    assert_eq!(named.exit, 2, "{}", named.stderr);
    assert_eq!(unit(&named.report, "plain")["check"], "unsupported");
    assert!(named.stderr.contains("NYL-ATTEST-UNSUPPORTED"), "{}", named.stderr);
}

const DESIRED: &str = "refs/heads/nyl/dev/desired";

/// `gate` declares `qa` for people and `smoke` for its driver; the
/// environment declares `signoff`. Returns the harness with `dev` reconciled
/// when `reconciled`.
fn external(reconciled: bool) -> (shell::Harness, TempDir) {
    let units = command(
        "gate",
        "true",
        "  attestations: [{name: qa, from: external}, {name: smoke}]\n",
    );
    let environment = ENVIRONMENT.replace(
        "  allowUnprotectedSource",
        "  attestations: [signoff]\n  allowUnprotectedSource",
    );
    assert!(environment.contains("signoff"), "{environment}");
    let project = project(&[
        NYL_TOML,
        ("nyl/environment.yaml", &environment),
        ("nyl/gate.yaml", &units),
    ]);
    let harness = shell::Harness::new(project.path(), true);
    assert_eq!(harness.invoke(&["state", "init", "-e", "dev"]).exit, 0);
    if reconciled {
        let run = harness.invoke(&["reconcile", "-e", "dev"]);
        assert_eq!(run.exit, 0, "{}", run.stderr);
    }
    (harness, project)
}

/// A unit-scoped external attestation names the unit's current receipt,
/// records the attester and what the caller gave (identity asserted by the
/// flags), writes one observed commit under the operation `attest`, leaves
/// the desired ref alone, and releases the lease. Catches the result landing
/// in the wrong file or ref, the attester or url being dropped, and the
/// write skipping the lease.
#[test]
fn test_an_external_attestation_records_the_unit_receipt_and_its_attester() {
    let (harness, _project) = external(true);
    let desired = harness.tip(DESIRED);
    let attested = harness.invoke(&[
        "attest",
        "-e",
        "dev",
        "--unit",
        "gate",
        "--name",
        "qa",
        "--result",
        "pass",
        "--reason",
        "QA signed off",
        "--url",
        "https://qa.example/1",
        "--attested-by",
        "alice",
        "--attestation-source",
        "qa-job 7",
        "-o",
        "json",
    ]);
    assert_eq!(attested.exit, 0, "{}", attested.stderr);
    assert_eq!(attested.report["details"]["scope"], "unit");
    assert_eq!(harness.tip(DESIRED), desired, "attesting writes the observed ref only");
    assert!(harness.message(OBSERVED).starts_with("nyl: attest dev"));
    assert!(harness.lease_released());
    let record = harness.show(OBSERVED, "observed/units/gate/attestations.yaml");
    for expected in [
        "kind: AttestationRecord",
        "qa:",
        "result: pass",
        "reason: QA signed off",
        "external:",
        "- alice",
        "identity: asserted",
        "source: qa-job 7",
        "url: https://qa.example/1",
    ] {
        assert!(record.contains(expected), "{expected}: {record}");
    }
    let status = harness.invoke(&["status", "-e", "dev"]);
    assert_eq!(status.exit, 0, "{}", status.stderr);
}

/// An environment-scoped attestation names the desired commit of the state
/// and records the workstation's Git user as a `local` attester when
/// nothing else says who; a failing result is recorded with exit 0 (D14),
/// and the driver form still exits 3 for the same kind of result. Catches
/// the environment scope keyed to anything but the desired commit, an
/// identity left empty, and a failing external result reported as an error.
#[test]
fn test_an_environment_attestation_names_the_desired_commit_and_the_local_identity() {
    let (harness, _project) = external(true);
    for (result, exit) in [("pass", 0), ("fail", 0)] {
        let attested = harness.invoke(&["attest", "-e", "dev", "--name", "signoff", "--result", result]);
        assert_eq!(attested.exit, exit, "{result}: {}", attested.stderr);
        let record = harness.show(OBSERVED, "observed/attestations.yaml");
        assert!(
            record.contains(&format!("executionKey: {}", harness.tip(DESIRED))),
            "{record}"
        );
        assert!(record.contains(&format!("result: {result}")), "{record}");
        assert!(
            record.contains("identity: local") && record.contains("source: local"),
            "{record}"
        );
        assert!(!record.contains("unit:"), "{record}");
    }
    assert!(harness.lease_released());
}

/// Refusals exit 1, write nothing, and carry a stable code with the fix: a
/// name the scope does not declare as external (a typo, a name its driver
/// owns, a unit declaring none, the environment's), a unit that is unknown
/// or has no current receipt, and an environment with no recorded state.
/// Catches evidence nobody requires being recorded, and a refusal that
/// still writes.
#[test]
fn test_external_attestations_refuse_undeclared_names_and_missing_state() {
    let (harness, _project) = external(false);
    let refuse = |args: &[&str], code: &str, text: &str| {
        let tip = harness.tip(OBSERVED);
        let mut full = vec!["attest", "-e", "dev", "--result", "pass", "-o", "json"];
        full.extend_from_slice(args);
        let refused = harness.invoke(&full);
        assert_eq!(refused.exit, 1, "{args:?}: {}", refused.stderr);
        assert!(
            refused.stderr.contains(code) && refused.stderr.contains(text),
            "{args:?}: {}",
            refused.stderr
        );
        assert_eq!(harness.tip(OBSERVED), tip, "{args:?}: nothing is written");
    };
    // Before the first reconcile nothing is recorded: no unit, no state.
    refuse(&["--unit", "gate", "--name", "qa"], "NYL-UNIT-UNKNOWN", "gate");
    refuse(&["--name", "signoff"], "NYL-ATTEST-NO-RECEIPT", "no recorded state");
    assert_eq!(harness.invoke(&["reconcile", "-e", "dev"]).exit, 0);
    refuse(
        &["--unit", "gate", "--name", "q"],
        "NYL-ATTESTATION-UNDECLARED",
        "use one of qa",
    );
    refuse(
        &["--unit", "gate", "--name", "smoke"],
        "NYL-ATTESTATION-UNDECLARED",
        "produced by the driver",
    );
    refuse(&["--name", "qa"], "NYL-ATTESTATION-UNDECLARED", "use one of signoff");
    refuse(&["--unit", "nope", "--name", "qa"], "NYL-UNIT-UNKNOWN", "nope");
}

/// The external form runs under the environment's lease: a lease held by
/// another run is exit 2 (`NYL-LEASE-HELD`) with nothing written, as for
/// every state-writing command. Catches a write that ignores the lease and
/// races a running reconcile.
#[test]
fn test_an_external_attestation_waits_for_a_held_lease() {
    use nyl_core::orchestration::ports::{Clock as _, IdGenerator as _};
    use nyl_core::orchestration::state::{Lease, LeaseKind, Operation, StateV1};
    let (harness, _project) = external(true);
    let holder = Lease {
        api_version: StateV1,
        kind: LeaseKind,
        run: harness.ids.run_id(),
        runner: "https://ci.example.com/runs/1".to_owned(),
        operation: Operation::Reconcile,
        units: Vec::new(),
        deadline: harness.clock.now() + chrono::Duration::minutes(5),
        taken_at: harness.clock.now(),
        broken: None,
    };
    let version = harness
        .store
        .take_lease(&harness.location, &shell::dev(), &holder, None)
        .unwrap();
    let tip = harness.tip(OBSERVED);
    let refused = harness.invoke(&[
        "attest", "-e", "dev", "--unit", "gate", "--name", "qa", "--result", "pass",
    ]);
    assert_eq!(refused.exit, 2, "{}", refused.stderr);
    assert!(refused.stderr.contains("NYL-LEASE-HELD"), "{}", refused.stderr);
    assert_eq!(harness.tip(OBSERVED), tip);
    harness
        .store
        .release_lease(&harness.location, &shell::dev(), &version)
        .unwrap();
    let recorded = harness.invoke(&[
        "attest", "-e", "dev", "--unit", "gate", "--name", "qa", "--result", "pass",
    ]);
    assert_eq!(recorded.exit, 0, "{}", recorded.stderr);
}
