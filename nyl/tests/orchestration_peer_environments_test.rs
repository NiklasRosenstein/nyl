//! How a broken other environment affects a command: an error when the
//! command must read it, a warning when it reads it only to give advice, and
//! no effect when the command does not need it.
//!
//! Contract: [Other environments](../../design/orchestration-core.md#other-environments).

mod scenario;

use std::path::Path;

use scenario::cross::{environment, Cross};
use serde_json::Value;

/// The broken declarations of an environment's state, by what is wrong: a
/// repository that is not there, a `repositoryRef` the project does not
/// declare, and a repository URL that is not a URL on any platform.
fn broken_states(nowhere: &Path) -> [(&'static str, String); 3] {
    let missing = reqwest::Url::from_file_path(nowhere.join("nyl-state.git")).unwrap();
    [
        (
            "unreadable",
            format!("  state:\n    repository: {{repoURL: '{missing}'}}\n"),
        ),
        (
            "undeclared repositoryRef",
            "  state:\n    repositoryRef: {name: nowhere}\n".to_owned(),
        ),
        (
            "malformed URL",
            "  state:\n    repository: {repoURL: 'file://bad host/nyl-state.git'}\n".to_owned(),
        ),
    ]
}

fn findings<'a>(outcome: &'a nyl::orchestration::run::CommandOutcome, list: &str) -> Vec<&'a Value> {
    outcome.report["environments"][0][list]
        .as_array()
        .map(|findings| findings.iter().collect())
        .unwrap_or_default()
}

fn unavailable<'a>(outcome: &'a nyl::orchestration::run::CommandOutcome, list: &str, peer: &str) -> Vec<&'a Value> {
    findings(outcome, list)
        .into_iter()
        .filter(|finding| {
            finding["code"] == "NYL-STATE-UNAVAILABLE" && finding["message"].as_str().unwrap().contains(peer)
        })
        .collect()
}

/// `observe` scans the other environments only to name consumers whose input
/// changed, so any problem with one is a `NYL-STATE-UNAVAILABLE` warning that
/// names it and points to `nyl validate`; the check still succeeds and writes
/// its own state. Catches a broken peer failing a check that did not need it,
/// and a peer skipped silently, which hides that its consumers were not
/// compared.
#[test]
fn test_observe_warns_about_a_broken_peer_and_succeeds() {
    let nowhere = tempfile::TempDir::new().unwrap();
    for (kind, state) in broken_states(nowhere.path()) {
        let cross = Cross::new();
        cross.declare("prod", &state);
        cross.read("203.0.113.20");
        let observed = cross.shell.invoke(&["observe", "-e", "dev", "-o", "json"]);
        assert_eq!(observed.exit, 0, "{kind}: {}", observed.stderr);
        assert_eq!(
            observed.report["environments"][0]["units"]["lb"]["check"], "observed",
            "{kind}"
        );
        let warnings = unavailable(&observed, "warnings", "prod");
        assert_eq!(warnings.len(), 1, "{kind}: {}", observed.stderr);
        assert!(
            warnings[0]["fix"].as_str().unwrap().contains("nyl validate"),
            "{kind}: {}",
            warnings[0]
        );
        assert!(findings(&observed, "errors").is_empty(), "{kind}: {}", observed.stderr);
    }
}

/// `status` reads the producer environments its consumers reference, so a
/// producer it cannot read is an error with a code and a fix, and one it does
/// not read, however broken, is not mentioned. Catches a status that shows
/// consumers as current without having compared the values they resolved, and
/// one that fails over an environment it never reads.
#[test]
fn test_status_errors_for_a_broken_producer_and_ignores_an_unreferenced_environment() {
    let nowhere = tempfile::TempDir::new().unwrap();
    for (kind, state) in broken_states(nowhere.path()) {
        // dev is the producer prod's dns reads.
        let cross = Cross::new();
        cross.declare("dev", &state);
        let status = cross.status_of_prod();
        assert_eq!(status.exit, 1, "{kind}: {}", status.stderr);
        let errors = unavailable(&status, "errors", "dev");
        assert_eq!(errors.len(), 1, "{kind}: {}", status.stderr);
        assert!(!errors[0]["fix"].as_str().unwrap().is_empty(), "{kind}: {}", errors[0]);

        // stage is read by nothing.
        let cross = Cross::new();
        cross.shell.world.commit_files(
            "main",
            "declare stage",
            &[("nyl/stage.yaml", &environment("stage", &state))],
        );
        let status = cross.status_of_prod();
        assert_eq!(status.exit, 0, "{kind}: {}", status.stderr);
        assert!(
            !status.stderr.contains("NYL-STATE-UNAVAILABLE"),
            "{kind}: {}",
            status.stderr
        );
    }
}

/// An environment that a command does not need never stops it: with `prod`
/// (a consumer of `dev`) or an unrelated `stage` broken, the commands of
/// `dev` and the reconcile of `prod` run as they do without it. Catches
/// resolving every environment up front and failing the request on the first
/// invalid one.
#[test]
fn test_a_broken_environment_a_command_does_not_need_leaves_it_unaffected() {
    let nowhere = tempfile::TempDir::new().unwrap();
    for (kind, state) in broken_states(nowhere.path()) {
        let cross = Cross::new();
        cross.declare("prod", &state);
        cross.shell.world.commit_files(
            "main",
            "declare stage",
            &[("nyl/stage.yaml", &environment("stage", &state))],
        );
        for command in [
            ["plan", "-e", "dev"],
            ["reconcile", "-e", "dev"],
            ["status", "-e", "dev"],
        ] {
            let mut args = command.to_vec();
            args.extend(["-o", "json"]);
            let outcome = cross.shell.invoke(&args);
            assert_eq!(outcome.exit, 0, "{kind}: {command:?}: {}", outcome.stderr);
            assert!(findings(&outcome, "errors").is_empty(), "{kind}: {command:?}");
        }
    }
    for (kind, state) in broken_states(nowhere.path()) {
        let cross = Cross::new();
        cross.shell.world.commit_files(
            "main",
            "declare stage",
            &[("nyl/stage.yaml", &environment("stage", &state))],
        );
        for command in [["plan", "-e", "prod"], ["reconcile", "-e", "prod"]] {
            let mut args = command.to_vec();
            args.extend(["-o", "json"]);
            let outcome = cross.shell.invoke(&args);
            assert_eq!(outcome.exit, 0, "{kind}: {command:?}: {}", outcome.stderr);
        }
    }
}

/// A reconcile or plan resolves a unit's references from the producer
/// environments it names, so a producer it cannot read stops the consumer with
/// the producer's own error (`NYL-STATE-UNAVAILABLE` and its fix), not with a
/// misleading complaint about an undeclared environment; `get units` reads the
/// same producers as `status` and refuses alike. Catches the real cause being
/// replaced by a consequence, which sends the operator to fix the reference
/// instead of the state.
#[test]
fn test_reconcile_plan_and_get_error_for_a_broken_producer_with_its_own_cause() {
    let nowhere = tempfile::TempDir::new().unwrap();
    for (kind, state) in broken_states(nowhere.path()) {
        let cross = Cross::new();
        cross.declare("dev", &state);
        for command in [["plan", "-e", "prod"], ["reconcile", "-e", "prod"]] {
            let mut args = command.to_vec();
            args.extend(["-o", "json"]);
            let outcome = cross.shell.invoke(&args);
            assert_eq!(outcome.exit, 1, "{kind}: {command:?}: {}", outcome.stderr);
            let errors = findings(&outcome, "errors");
            assert_eq!(
                unavailable(&outcome, "errors", "dev").len(),
                1,
                "{kind}: {command:?}: {errors:?}"
            );
            assert_eq!(errors.len(), 1, "{kind}: {command:?}: {errors:?}");
        }
        let table = cross.shell.invoke(&["get", "units", "-e", "prod"]);
        assert_ne!(table.exit, 0, "{kind}: {}", table.stdout);
        assert!(
            table.stderr.contains("NYL-STATE-UNAVAILABLE"),
            "{kind}: {}",
            table.stderr
        );
    }
}

/// `nyl validate` reports every invalid declaration as an error, the state of
/// an environment included, so CI catches what the commands that do not need
/// the environment tolerate.
#[test]
fn test_validate_reports_a_malformed_state_url_and_an_undeclared_repository() {
    let nowhere = tempfile::TempDir::new().unwrap();
    for (kind, state) in broken_states(nowhere.path()).into_iter().skip(1) {
        let cross = Cross::new();
        cross.declare("stage", &state);
        let clone = cross.shell.world.fresh_clone("main");
        let validate = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .args(["validate", "nyl"])
            .current_dir(&clone)
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&validate.get_output().stderr).into_owned();
        assert!(
            stderr.contains("NYL-STATE-UNAVAILABLE") && stderr.contains("stage"),
            "{kind}: {stderr}"
        );
    }
}

/// A unit that leaves its environment may be read by a consumer in any other
/// environment, so then every other environment is required: one whose state
/// cannot be read is an error and nothing is torn down on a guess. Catches a
/// teardown that proceeds over a consumer it could not see.
#[test]
fn test_a_leaving_unit_requires_every_other_environment() {
    let nowhere = tempfile::TempDir::new().unwrap();
    let (_, state) = broken_states(nowhere.path()).into_iter().next().unwrap();
    let cross = Cross::new();
    // dev's `lb` leaves the source; prod's `dns` no longer reads it.
    let dns_only = scenario::cross::CROSS_UNITS.split("---\n").nth(1).unwrap().replace(
        "fromUnit: {environment: dev, unit: lb, observation: ip}",
        "value: static",
    );
    cross.shell.world.commit_files(
        "main",
        "drop lb and its consumer's reference",
        &[("nyl/units.yaml", &dns_only)],
    );
    cross.declare("prod", &state);
    let plan = cross.shell.invoke(&["plan", "-e", "dev", "-o", "json"]);
    assert_eq!(plan.exit, 1, "{}", plan.stderr);
    assert_eq!(unavailable(&plan, "errors", "prod").len(), 1, "{}", plan.stderr);
}
