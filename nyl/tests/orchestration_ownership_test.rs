//! Stored native resources stay owned while held or pending teardown.
mod scenario;

use scenario::shell::{project, Harness, NYL_TOML};

fn environment(name: &str) -> String {
    format!("apiVersion: gitops.nyl/v1\nkind: Environment\nmetadata: {{name: {name}}}\nspec:\n  unitSelector: {{matchLabels: {{scope: {name}}}}}\n  allowUnprotectedSource: true\n")
}

fn infra(name: &str, owner: &str, enabled: bool) -> String {
    format!("apiVersion: units.gitops.nyl/v1\nkind: OpenTofu\nmetadata:\n  name: {name}\n  labels: {{scope: {owner}}}\nspec:\n  enabled: {enabled}\n  source: {{path: ../infra}}\n  backend: {{path: '%SCENARIO_TMP%/shared.tfstate'}}\n  variables: {{owner: {owner}}}\n")
}

fn stored_claim_blocks_reassignment(held: bool) {
    let initial = infra("old", "owner", true);
    let project = project(&[
        NYL_TOML,
        ("nyl/owner.yaml", &environment("owner")),
        ("nyl/candidate.yaml", &environment("candidate")),
        ("nyl/old.yaml", &initial),
        ("infra/main.tf", "# fake infrastructure source\n"),
    ]);
    let harness = Harness::new(project.path(), true);
    let init = harness.invoke(&["state", "init", "-e", "owner"]);
    assert_eq!(init.exit, 0, "{}", init.stderr);
    // The candidate's not-yet-initialized state is known empty.
    let initial = harness.invoke(&["reconcile", "-e", "owner"]);
    assert_eq!(initial.exit, 0, "{}", initial.stderr);
    let repeated = harness.invoke(&["reconcile", "-e", "owner"]);
    assert_eq!(repeated.exit, 0, "{}", repeated.stderr);
    let backend = harness.world.scenario_tmp().join("shared.tfstate");
    let original = std::fs::read(&backend).unwrap();
    if held {
        let hold = harness.invoke(&["hold", "-e", "owner", "--unit", "old", "--reason", "incident"]);
        assert_eq!(hold.exit, 0, "{}", hold.stderr);
    }
    harness.world.commit_files(
        "main",
        "deselect old owner",
        &[("nyl/old.yaml", &infra("old", "owner", false))],
    );
    let leaving = harness.invoke(&["reconcile", "-e", "owner"]);
    assert_eq!(leaving.exit, 2, "{}", leaving.stderr);
    assert_eq!(
        leaving.report["environments"][0]["units"]["old"]["result"],
        if held { "held" } else { "pending-teardown" }
    );
    harness.world.commit_files(
        "main",
        "claim the old backend",
        &[("nyl/new.yaml", &infra("new", "candidate", true))],
    );
    let init = harness.invoke(&["state", "init", "-e", "candidate"]);
    assert_eq!(init.exit, 0, "{}", init.stderr);
    let refused = harness.invoke(&["reconcile", "-e", "candidate"]);
    assert_eq!(refused.exit, 1, "{}", refused.stderr);
    assert!(
        refused.stderr.contains("NYL-UNIT-OWNERSHIP-OVERLAP"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("owner/old") && refused.stderr.contains("candidate/new"),
        "{}",
        refused.stderr
    );
    assert_eq!(std::fs::read(&backend).unwrap(), original);
    if held {
        let resume = harness.invoke(&["resume", "-e", "owner", "--unit", "old"]);
        assert_eq!(resume.exit, 0, "{}", resume.stderr);
    }
    let removed = harness.invoke(&[
        "teardown",
        "-e",
        "owner",
        "--unit",
        "old",
        "--reason",
        "release backend",
    ]);
    assert_eq!(removed.exit, 0, "{}", removed.stderr);
    let transferred = harness.invoke(&["reconcile", "-e", "candidate"]);
    assert_eq!(transferred.exit, 0, "{}", transferred.stderr);
    assert_eq!(
        transferred.report["environments"][0]["units"]["new"]["result"],
        "executed"
    );
    assert_ne!(std::fs::read(&backend).unwrap(), original);
}

#[test]
fn test_held_incarnation_keeps_its_backend_after_source_deselection() {
    stored_claim_blocks_reassignment(true);
}

#[test]
fn test_deleting_incarnation_keeps_its_backend_until_teardown_finishes() {
    stored_claim_blocks_reassignment(false);
}
