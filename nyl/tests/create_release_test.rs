use std::fs;

use assert_cmd::Command;
use git2::Repository;
use predicates::prelude::*;
use tempfile::TempDir;

/// A project whose AppProject admits only the `default` namespace and whose
/// group selects Releases from a subdirectory.
fn project(source: &str, include: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(
        temp.path().join("gitops.yaml"),
        format!(
            r"apiVersion: k8s.gitops.nyl/v1
kind: AppProjectDefinition
metadata:
  name: workloads
spec:
  management: Rendered
  manifest:
    apiVersion: argoproj.io/v1alpha1
    kind: AppProject
    metadata:
      name: workloads
      namespace: argocd
    spec:
      sourceRepos: []
      destinations:
        - server: https://kubernetes.default.svc
          namespace: default
---
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: platform
spec:
  projectRef: workloads
  applicationNamespace: argocd
  source:
    path: {source}
    include:
      - '{include}'
"
        ),
    )
    .unwrap();
    temp
}

#[test]
fn warns_when_the_group_source_does_not_select_the_new_release() {
    let temp = project("applications/platform", "releases/*.yaml");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(temp.path())
        .args(["create", "release", "api", "--namespace", "default"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "ApplicationGroup \"platform\" does not select applications/platform/api.yaml",
        ));
    assert!(temp.path().join("applications/platform/api.yaml").is_file());
}

#[test]
fn warns_when_the_release_namespace_is_outside_the_referenced_app_project() {
    let temp = project("applications/platform", "*.yaml");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(temp.path())
        .args(["create", "release", "api", "--additional-namespaces", "observability"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("AppProject \"workloads\" does not allow namespace \"api\"")
                .and(predicate::str::contains("does not allow namespace \"observability\"")),
        );

    // A namespace the project admits is not reported.
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(temp.path())
        .args(["create", "release", "web", "--namespace", "default"])
        .assert()
        .success()
        .stderr(predicate::str::contains("does not allow namespace").not());
}
