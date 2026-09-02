use std::fs;

use assert_cmd::Command;
use git2::Repository;
use predicates::prelude::*;
use tempfile::TempDir;

fn repository() -> TempDir {
    let temporary = TempDir::new().unwrap();
    let repository = Repository::init(temporary.path()).unwrap();
    repository
        .remote("origin", "https://git.example.invalid/platform.git")
        .unwrap();
    repository
        .remote_set_pushurl("origin", Some("ssh://git@git.example.invalid/platform.git"))
        .unwrap();
    temporary
}

#[test]
fn initializes_simple_gitops_project_from_detected_repository() {
    let repository = repository();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .env("KUBECONFIG", repository.path().join("missing-kubeconfig"))
        .args([
            "init",
            "gitops",
            ".",
            "--yes",
            "--cluster-name",
            "production",
            "--context",
            "production-admin",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Initialized rendered GitOps configuration at gitops.yaml",
        ));

    assert!(repository.path().join("nyl.toml").is_file());
    assert!(repository.path().join("applications").is_dir());
    let yaml = fs::read_to_string(repository.path().join("gitops.yaml")).unwrap();
    assert!(yaml.contains("kind: DeploymentTarget"));
    assert!(yaml.contains("kind: ApplicationGroup"));
    assert!(yaml.contains("repoURL: https://git.example.invalid/platform.git"));
    assert!(yaml.contains("publishURL: ssh://git@git.example.invalid/platform.git"));
    assert!(yaml.contains("sourceRepositoryRefs:"));
    assert!(!yaml.contains("clusterRef:"));
    assert!(!yaml.contains("pathPrefix:"));
    assert!(!yaml.contains("destinationNamespace:"));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .args(["init", "gitops", ".", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Refusing to overwrite existing configuration"));
}

#[test]
fn stdout_mode_has_no_filesystem_side_effects() {
    let repository = repository();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .env("KUBECONFIG", repository.path().join("missing-kubeconfig"))
        .args([
            "init",
            "gitops",
            ".",
            "--yes",
            "--output",
            "-",
            "--no-context",
            "--skip-applications",
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("apiVersion: gitops.nyl/v1"))
        .stdout(predicate::str::contains("kind: DeploymentTarget"))
        .stdout(predicate::str::contains("kind: ApplicationGroup").not());

    assert!(!repository.path().join("nyl.toml").exists());
    assert!(!repository.path().join("gitops.yaml").exists());
    assert!(!repository.path().join("applications").exists());
}
