use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn resource_schema_cli_accepts_canonical_kind_and_alias() {
    for kind in ["DeploymentTarget", "target"] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        command
            .args(["schema", "resource", kind])
            .assert()
            .success()
            .stdout(predicate::str::contains("\"const\": \"k8s.gitops.nyl/v1\""))
            .stdout(predicate::str::contains("\"const\": \"DeploymentTarget\""));
    }
}

#[test]
fn resource_schema_cli_supports_cluster() {
    for kind in ["Cluster", "cluster"] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        command
            .args(["schema", "resource", kind])
            .assert()
            .success()
            .stdout(predicate::str::contains("\"const\": \"Cluster\""));
    }
}

#[test]
fn resource_schema_cli_supports_argocd_instance() {
    for kind in ["ArgoCDInstance", "argocd-instance"] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        command
            .args(["schema", "resource", kind])
            .assert()
            .success()
            .stdout(predicate::str::contains("\"const\": \"ArgoCDInstance\""));
    }
}

#[test]
fn resource_schema_cli_supports_release() {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .args(["schema", "resource", "Release"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"const\": \"k8s.gitops.nyl/v1\""))
        .stdout(predicate::str::contains("\"const\": \"Release\""))
        .stdout(predicate::str::contains("\"additionalNamespaces\""))
        .stdout(predicate::str::contains("\"include\""));
}

#[test]
fn aggregate_schema_cli_uses_relative_refs() {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .args(["schema", "gitops"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"$ref\": \"git-repository.schema.json\""))
        .stdout(predicate::str::contains("\"$ref\": \"cluster.schema.json\""))
        .stdout(predicate::str::contains("\"$ref\": \"application-group.schema.json\""))
        .stdout(predicate::str::contains("\"$ref\": \"release.schema.json\""));
}

#[test]
fn all_schema_cli_writes_the_complete_set() {
    let directory = tempfile::tempdir().unwrap();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .args(["schema", "all", "--output-dir"])
        .arg(directory.path())
        .assert()
        .success();

    let expected = [
        "nyl.schema.json",
        "git-repository.schema.json",
        "cluster.schema.json",
        "deployment-target.schema.json",
        "app-project-definition.schema.json",
        "application-group.schema.json",
        "release.schema.json",
        "gitops-resource.schema.json",
    ];
    for filename in expected {
        let contents = fs::read_to_string(directory.path().join(filename)).unwrap();
        let _: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert!(contents.ends_with('\n'));
    }
}

#[test]
fn rendering_schema_cli_supports_api_qualification_and_dynamic_kinds() {
    for (kind, api) in [
        ("HelmChart", "k8s.nyl/v1"),
        ("RemoteManifest", "k8s.nyl/v1"),
        ("Component", "components.k8s.nyl/v1"),
    ] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .args(["schema", "resource", kind, "--api-version", api])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let schema: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(schema["properties"]["apiVersion"]["const"], api);
        assert!(schema["description"].as_str().unwrap().contains("Kubernetes"));
        if kind == "Component" {
            assert!(schema["properties"]["kind"].get("const").is_none());
        }
    }
    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .args(["schema", "resource", "Cluster", "--api-version", "gitops.nyl/v1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("k8s.gitops.nyl/v1"));
}
