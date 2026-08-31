use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Test that --max-depth flag is accepted
#[test]
fn test_max_depth_flag_accepted() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create a simple resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
"#,
    )
    .unwrap();

    // Run render command with --max-depth
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--offline")
        .arg("--kube-version")
        .arg("1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1")
        .arg("--max-depth")
        .arg("5")
        .arg("test-resource.yaml");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

/// Test that --max-depth actually limits recursive evaluation
#[test]
fn test_max_depth_limits_evaluation() {
    let temp = TempDir::new().unwrap();

    // Create project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create a local helm chart
    let chart_dir = temp.path().join("charts").join("test-chart");
    fs::create_dir_all(chart_dir.join("templates")).unwrap();

    fs::write(
        chart_dir.join("Chart.yaml"),
        r#"
apiVersion: v2
name: test-chart
version: 1.0.0
"#,
    )
    .unwrap();

    // Chart template that produces a nested HelmChart
    fs::write(
        chart_dir.join("templates").join("nested.yaml"),
        r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nested-chart
  namespace: {{ .Release.Namespace }}
spec:
  chart:
    name: ./charts/test-chart
"#,
    )
    .unwrap();

    // Top-level HelmChart resource
    fs::write(
        temp.path().join("test.yaml"),
        r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: top-chart
  namespace: default
spec:
  chart:
    name: ./charts/test-chart
"#,
    )
    .unwrap();

    // Run with max-depth=1 - should succeed and return the nested HelmChart without expanding it
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--offline")
        .arg("--kube-version")
        .arg("1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1")
        .arg("--max-depth")
        .arg("1")
        .arg("test.yaml");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains("kind: HelmChart"))
        .stdout(predicate::str::contains("nested-chart"));
}

/// Test that --track-parent flag is accepted
#[test]
fn test_track_parent_flag_accepted() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create a simple resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
"#,
    )
    .unwrap();

    // Run render command with --track-parent
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--offline")
        .arg("--kube-version")
        .arg("1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1")
        .arg("--track-parent")
        .arg("test-resource.yaml");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

/// Test that --track-parent actually adds parent annotations
#[test]
fn test_track_parent_adds_annotations() {
    let temp = TempDir::new().unwrap();

    // Create project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create a local helm chart
    let chart_dir = temp.path().join("charts").join("nginx");
    fs::create_dir_all(chart_dir.join("templates")).unwrap();

    fs::write(
        chart_dir.join("Chart.yaml"),
        r#"
apiVersion: v2
name: nginx
version: 1.0.0
"#,
    )
    .unwrap();

    // Simple pod template
    fs::write(
        chart_dir.join("templates").join("pod.yaml"),
        r#"
apiVersion: v1
kind: Pod
metadata:
  name: test-pod
  namespace: {{ .Release.Namespace }}
spec:
  containers:
  - name: nginx
    image: nginx:latest
"#,
    )
    .unwrap();

    // HelmChart resource
    fs::write(
        temp.path().join("test.yaml"),
        r#"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: my-nginx
  namespace: default
spec:
  chart:
    name: ./charts/nginx
"#,
    )
    .unwrap();

    // Run with --track-parent
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--offline")
        .arg("--kube-version")
        .arg("1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1")
        .arg("--track-parent")
        .arg("test.yaml");

    cmd.assert()
        .success()
        .stdout(predicate::str::contains(
            "nyl.niklasrosenstein.github.com/parent-api-version",
        ))
        .stdout(predicate::str::contains("nyl.niklasrosenstein.github.com/parent-kind"))
        .stdout(predicate::str::contains("nyl.niklasrosenstein.github.com/parent-name"))
        .stdout(predicate::str::contains(
            "nyl.niklasrosenstein.github.com/parent-namespace",
        ))
        .stdout(predicate::str::contains("my-nginx")); // parent name
}

/// Test that --max-depth and --track-parent work together
#[test]
fn test_max_depth_and_track_parent_together() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create a simple resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
data:
  key: value
"#,
    )
    .unwrap();

    // Run render command with both flags
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--offline")
        .arg("--kube-version")
        .arg("1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1")
        .arg("--max-depth")
        .arg("3")
        .arg("--track-parent")
        .arg("test-resource.yaml");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

/// Test that apply command accepts new flags
#[test]
fn test_apply_with_new_flags() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create empty resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
# Empty file
"#,
    )
    .unwrap();

    // Command should parse flags, then enforce explicit target selection for live operations.
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.env("KUBECONFIG", temp.path().join("does-not-exist-kubeconfig"));
    cmd.arg("apply")
        .arg("--max-depth")
        .arg("5")
        .arg("--track-parent")
        .arg("--no-release")
        .arg("test-resource.yaml");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("nyl apply requires --target"));
}

#[test]
fn test_apply_no_release_conflicts_with_append_release() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("apply")
        .arg("--no-release")
        .arg("--append-release")
        .arg("test-resource.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("--no-release"))
        .stderr(predicate::str::contains("--append-release"));
}

#[test]
fn test_apply_no_release_conflicts_with_name() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("apply")
        .arg("--no-release")
        .arg("--name")
        .arg("demo")
        .arg("test-resource.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("--no-release"))
        .stderr(predicate::str::contains("--name"));
}

#[test]
fn test_apply_no_release_conflicts_with_namespace() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("apply")
        .arg("--no-release")
        .arg("--namespace")
        .arg("default")
        .arg("test-resource.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("--no-release"))
        .stderr(predicate::str::contains("--namespace"));
}

/// Test that diff command accepts new flags
#[test]
fn test_diff_with_new_flags() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl.toml"),
        r#"
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
"#,
    )
    .unwrap();

    // Create empty resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
# Empty file
"#,
    )
    .unwrap();

    // Command should parse flags, then enforce explicit target selection for live operations.
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.env("KUBECONFIG", temp.path().join("does-not-exist-kubeconfig"));
    cmd.arg("diff")
        .arg("--max-depth")
        .arg("5")
        .arg("--track-parent")
        .arg("test-resource.yaml");

    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("nyl diff requires --target"));
}
