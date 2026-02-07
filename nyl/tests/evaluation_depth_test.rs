#![allow(deprecated)]

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
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath: []
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
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("render").arg("--max-depth").arg("5");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

/// Test that --track-parent flag is accepted
#[test]
fn test_track_parent_flag_accepted() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath: []
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
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("render").arg("--track-parent");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

/// Test that --max-depth and --track-parent work together
#[test]
fn test_max_depth_and_track_parent_together() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath: []
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
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--max-depth")
        .arg("3")
        .arg("--track-parent");
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
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath: []
"#,
    )
    .unwrap();

    // The command will succeed with no manifests, but we're testing that flags are accepted
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("apply")
        .arg("--max-depth")
        .arg("5")
        .arg("--track-parent")
        .arg("--dry-run");
    
    // Should succeed but with no manifests message
    cmd.assert().success();
}

/// Test that diff command accepts new flags
#[test]
fn test_diff_with_new_flags() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
settings:
  searchPath: []
"#,
    )
    .unwrap();

    // The command will succeed with no manifests, but we're testing that flags are accepted
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("diff")
        .arg("--max-depth")
        .arg("5")
        .arg("--track-parent");
    
    // Should succeed but with no manifests message
    cmd.assert().success();
}
