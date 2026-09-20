use std::fs;

use assert_cmd::Command;
use git2::Repository;
use nyl::config::{ProjectConfig, VendorMode};
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
    assert!(!yaml.contains("clusterRef:"));
    assert!(!yaml.contains("pathPrefix:"));
    assert!(!yaml.contains("destinationNamespace:"));
    // The group keeps the implied permissive AppProject; nothing declares a project.
    assert!(!yaml.contains("kind: AppProjectDefinition"));
    assert!(!yaml.contains("projectRef:"));
    assert!(!yaml.contains("projectTemplate:"));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .args(["init", ".", "--yes"])
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

#[test]
fn initializes_a_missing_directory_inside_the_git_worktree() {
    let repository = repository();
    let project = repository.path().join("platform");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .env("KUBECONFIG", repository.path().join("missing-kubeconfig"))
        .args(["init", "platform", "--yes", "--no-context", "--skip-applications"])
        .assert()
        .success();
    assert!(project.join("nyl.toml").is_file());
    assert!(project.join("gitops.yaml").is_file());
}

#[test]
fn minimal_mode_rejects_gitops_options() {
    Command::cargo_bin("nyl")
        .unwrap()
        .args([
            "init",
            "--minimal",
            "--repo-url",
            "https://git.example.invalid/deploy.git",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn records_the_requested_vendor_policy_in_the_generated_project_config() {
    let repository = repository();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .env("KUBECONFIG", repository.path().join("missing-kubeconfig"))
        .args(["init", ".", "--yes", "--no-context", "--vendor", "required"])
        .assert()
        .success();

    let config = ProjectConfig::load(Some(repository.path().join("nyl.toml"))).unwrap();
    assert_eq!(config.vendor().map(|vendor| vendor.mode), Some(VendorMode::Required));

    // The policy of an existing project configuration is never rewritten.
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .args([
            "init",
            ".",
            "--yes",
            "--no-context",
            "--output",
            "second.yaml",
            "--vendor",
            "preferred",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--vendor cannot modify"));
}

#[test]
fn minimal_mode_records_the_requested_vendor_policy() {
    let repository = repository();
    let project = repository.path().join("platform");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .args(["init", "platform", "--minimal", "--vendor", "preferred"])
        .assert()
        .success();

    let config = ProjectConfig::load(Some(project.join("nyl.toml"))).unwrap();
    assert_eq!(config.vendor().map(|vendor| vendor.mode), Some(VendorMode::Preferred));
    assert_eq!(
        config.vendor().map(|vendor| vendor.path.clone()),
        Some(project.join("vendor"))
    );
}

#[test]
fn vendor_policy_requires_a_written_project_config() {
    let repository = repository();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .env("KUBECONFIG", repository.path().join("missing-kubeconfig"))
        .args([
            "init",
            ".",
            "--yes",
            "--no-context",
            "--output",
            "-",
            "--vendor",
            "required",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--vendor cannot be used with --output -"));
    assert!(!repository.path().join("nyl.toml").exists());
}

#[test]
fn project_scope_options_narrow_the_generated_application_group() {
    let repository = repository();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(repository.path())
        .env("KUBECONFIG", repository.path().join("missing-kubeconfig"))
        .args([
            "init",
            ".",
            "--yes",
            "--no-context",
            "--project-name",
            "workloads",
            "--allow-namespace",
            "apps",
            "--allow-cluster-resource",
            "core/Namespace",
        ])
        .assert()
        .success();

    let yaml = fs::read_to_string(repository.path().join("gitops.yaml")).unwrap();
    assert!(yaml.contains("projectTemplate:"));
    assert!(yaml.contains("name: workloads"));
    assert!(yaml.contains("- apps"));
    assert!(yaml.contains("kind: Namespace"));
    assert!(!yaml.contains("kind: AppProjectDefinition"));
}
