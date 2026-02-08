use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Helper to create a test project structure with multiple resource types
fn create_test_project(temp: &TempDir) {
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath:
    - components

profiles:
  default:
    values:
      environment: test
"#,
    )
    .unwrap();

    fs::write(
        temp.path().join("secrets.yaml"),
        r#"
provider: null
"#,
    )
    .unwrap();

    // Create a manifest with multiple resource types
    fs::write(
        temp.path().join("multi-resource.yaml"),
        r#"
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: test-crd
spec:
  group: example.com
  names:
    kind: TestResource
    plural: testresources
  scope: Namespaced
  versions:
    - name: v1
      served: true
      storage: true
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
---
apiVersion: v1
kind: Secret
metadata:
  name: test-secret
type: Opaque
data:
  password: dGVzdA==
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-deployment
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test
  template:
    metadata:
      labels:
        app: test
    spec:
      containers:
      - name: test
        image: nginx:latest
"#,
    )
    .unwrap();
}

#[test]
fn test_render_only_kind_crds_only() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Render with --only-kind=CustomResourceDefinition
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("multi-resource.yaml")
        .arg("--only-kind=CustomResourceDefinition");

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Should contain CRD
    assert!(stdout.contains("kind: CustomResourceDefinition"));
    assert!(stdout.contains("name: test-crd"));

    // Should NOT contain other resources
    assert!(!stdout.contains("kind: ConfigMap"));
    assert!(!stdout.contains("kind: Secret"));
    assert!(!stdout.contains("kind: Deployment"));
}

#[test]
fn test_render_exclude_kind_no_crds() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Render with --exclude-kind=CustomResourceDefinition
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("multi-resource.yaml")
        .arg("--exclude-kind=CustomResourceDefinition");

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Should NOT contain CRD
    assert!(!stdout.contains("kind: CustomResourceDefinition"));

    // Should contain other resources
    assert!(stdout.contains("kind: ConfigMap"));
    assert!(stdout.contains("kind: Secret"));
    assert!(stdout.contains("kind: Deployment"));
}

#[test]
fn test_render_only_kind_multiple() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Render with multiple kinds: --only-kind=ConfigMap,Secret
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("multi-resource.yaml")
        .arg("--only-kind=ConfigMap,Secret");

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Should contain ConfigMap and Secret
    assert!(stdout.contains("kind: ConfigMap"));
    assert!(stdout.contains("kind: Secret"));

    // Should NOT contain CRD or Deployment
    assert!(!stdout.contains("kind: CustomResourceDefinition"));
    assert!(!stdout.contains("kind: Deployment"));
}

#[test]
fn test_render_exclude_kind_multiple() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Render with multiple exclude kinds: --exclude-kind=Secret,ConfigMap
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("multi-resource.yaml")
        .arg("--exclude-kind=Secret,ConfigMap");

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Should contain CRD and Deployment
    assert!(stdout.contains("kind: CustomResourceDefinition"));
    assert!(stdout.contains("kind: Deployment"));

    // Should NOT contain ConfigMap or Secret
    assert!(!stdout.contains("kind: ConfigMap"));
    assert!(!stdout.contains("kind: Secret"));
}

#[test]
fn test_render_with_api_version_filter() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Create a manifest with same kind but different apiVersions
    fs::write(
        temp.path().join("versioned-resources.yaml"),
        r#"
apiVersion: v1
kind: Secret
metadata:
  name: core-secret
type: Opaque
---
apiVersion: custom.io/v1
kind: Secret
metadata:
  name: custom-secret
"#,
    )
    .unwrap();

    // Filter by full apiVersion/kind
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("versioned-resources.yaml")
        .arg("--only-kind=v1/Secret");

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Should contain only v1 Secret
    assert!(stdout.contains("name: core-secret"));
    assert!(!stdout.contains("name: custom-secret"));
}

#[test]
fn test_render_mutually_exclusive_flags() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Try to use both --only-kind and --exclude-kind (should fail)
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("multi-resource.yaml")
        .arg("--only-kind=ConfigMap")
        .arg("--exclude-kind=Secret");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn test_render_case_sensitive() {
    let temp = TempDir::new().unwrap();
    create_test_project(&temp);

    // Filter with wrong case should return no results
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("multi-resource.yaml")
        .arg("--only-kind=configmap"); // lowercase

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Should be empty (case-sensitive match)
    assert!(!stdout.contains("kind: ConfigMap"));
    assert!(stdout.trim().is_empty() || stdout == "---\n");
}

// Cluster tests (require Minikube/kubectl)
// These tests are ignored by default and only run in CI with --ignored flag

#[test]
#[ignore]
fn test_diff_append_release_preview() {
    let temp = TempDir::new().unwrap();

    // Setup project (same as test_append_release_no_pruning)
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath:
    - components
"#,
    )
    .unwrap();

    fs::write(
        temp.path().join("nyl-profiles.yaml"),
        r#"
profiles:
  default:
    context: minikube
    values:
      environment: test
"#,
    )
    .unwrap();

    fs::write(
        temp.path().join("secrets.yaml"),
        r#"
provider: null
"#,
    )
    .unwrap();

    fs::write(
        temp.path().join("bootstrap.yaml"),
        r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: test-diff-append
  namespace: default
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: testresources-diff.example.com
spec:
  group: example.com
  names:
    kind: TestResourceDiff
    plural: testresources-diff
  scope: Namespaced
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-deployment-diff
  namespace: default
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test
  template:
    metadata:
      labels:
        app: test
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
"#,
    )
    .unwrap();

    // Phase 1: Apply CRDs only
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("apply")
        .arg("bootstrap.yaml")
        .arg("--only-kind=CustomResourceDefinition");
    cmd.assert().success();

    // Phase 2: Preview diff with --append-release (should show both CRD and Deployment)
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("diff")
        .arg("bootstrap.yaml")
        .arg("--exclude-kind=CustomResourceDefinition")
        .arg("--append-release");

    let output = cmd.assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // The diff should show the Deployment as added (from current apply)
    // But should NOT show CRD as deleted (because append-release preserves it)
    assert!(stdout.contains("test-deployment-diff") || stdout.contains("Deployment"));

    // Cleanup
    let mut kubectl = Command::new("kubectl");
    kubectl.arg("delete").arg("crd").arg("testresources-diff.example.com");
    kubectl.assert().success();
}

#[test]
#[ignore]
fn test_append_release_no_pruning() {
    let temp = TempDir::new().unwrap();

    // Setup project
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath:
    - components
"#,
    )
    .unwrap();

    fs::write(
        temp.path().join("nyl-profiles.yaml"),
        r#"
profiles:
  default:
    context: minikube
    values:
      environment: test
"#,
    )
    .unwrap();

    fs::write(
        temp.path().join("secrets.yaml"),
        r#"
provider: null
"#,
    )
    .unwrap();

    // Create bootstrap manifest with CRD and Deployment
    fs::write(
        temp.path().join("bootstrap.yaml"),
        r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: test-append-release
  namespace: default
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: testresources.example.com
spec:
  group: example.com
  names:
    kind: TestResource
    plural: testresources
  scope: Namespaced
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: test-deployment
  namespace: default
spec:
  replicas: 1
  selector:
    matchLabels:
      app: test
  template:
    metadata:
      labels:
        app: test
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
"#,
    )
    .unwrap();

    // Phase 1: Apply CRDs only
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("apply")
        .arg("bootstrap.yaml")
        .arg("--only-kind=CustomResourceDefinition");

    cmd.assert().success();

    // Verify CRD exists
    let mut kubectl = Command::new("kubectl");
    kubectl.arg("get").arg("crd").arg("testresources.example.com");
    kubectl.assert().success();

    // Phase 2: Apply everything else with --append-release
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("apply")
        .arg("bootstrap.yaml")
        .arg("--exclude-kind=CustomResourceDefinition")
        .arg("--append-release");

    cmd.assert().success();

    // Verify CRD still exists (not pruned)
    let mut kubectl = Command::new("kubectl");
    kubectl.arg("get").arg("crd").arg("testresources.example.com");
    kubectl.assert().success();

    // Verify Deployment exists
    let mut kubectl = Command::new("kubectl");
    kubectl
        .arg("get")
        .arg("deployment")
        .arg("test-deployment")
        .arg("-n")
        .arg("default");
    kubectl.assert().success();

    // Cleanup
    let mut kubectl = Command::new("kubectl");
    kubectl.arg("delete").arg("crd").arg("testresources.example.com");
    kubectl.assert().success();

    let mut kubectl = Command::new("kubectl");
    kubectl
        .arg("delete")
        .arg("deployment")
        .arg("test-deployment")
        .arg("-n")
        .arg("default");
    kubectl.assert().success();
}
