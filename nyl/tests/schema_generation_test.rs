use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn resource_schema_cli_accepts_canonical_kind_and_alias() {
    for kind in ["GitOpsTarget", "target"] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        command
            .args(["generate", "schema", "resource", kind])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "\"const\": \"gitops.nyl.niklasrosenstein.github.com/v1\"",
            ))
            .stdout(predicate::str::contains("\"const\": \"GitOpsTarget\""));
    }
}

#[test]
fn resource_schema_cli_supports_cluster() {
    for kind in ["Cluster", "cluster"] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        command
            .args(["generate", "schema", "resource", kind])
            .assert()
            .success()
            .stdout(predicate::str::contains("\"const\": \"Cluster\""));
    }
}

#[test]
fn aggregate_schema_cli_uses_relative_refs() {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .args(["generate", "schema", "gitops"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"$ref\": \"git-repository.schema.json\""))
        .stdout(predicate::str::contains("\"$ref\": \"cluster.schema.json\""))
        .stdout(predicate::str::contains("\"$ref\": \"application-group.schema.json\""));
}

#[test]
fn all_schema_cli_writes_the_complete_set() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .args(["generate", "schema", "all", "--output-dir"])
        .arg(directory.path())
        .assert()
        .success();

    let expected = [
        "nyl.schema.json",
        "git-repository.schema.json",
        "cluster.schema.json",
        "gitops-target.schema.json",
        "app-project-definition.schema.json",
        "application-group.schema.json",
        "gitops-resource.schema.json",
    ];
    for filename in expected {
        let contents = fs::read_to_string(directory.path().join(filename)).unwrap();
        let _: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert!(contents.ends_with('\n'));
    }
}
