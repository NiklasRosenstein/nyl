#![allow(deprecated)]

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_cli_help() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Kubernetes manifest generator"));
}

#[test]
fn test_cli_version() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("--version");
    cmd.assert().success().stdout(predicate::str::contains("nyl"));
}

#[test]
fn test_render_command_basic() {
    let temp = TempDir::new().unwrap();

    // Create minimal project structure
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

    // Run render command
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("render");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

#[test]
fn test_diff_command_requires_default_profile_when_profiles_exist() {
    let temp = TempDir::new().unwrap();

    // Create config with a profile that is NOT named "default"
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
profiles:
  staging:
    values:
      environment: staging
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("diff");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Profile 'default' not found"))
        .stderr(predicate::str::contains("Available profiles: staging"));
}

#[test]
fn test_apply_command_requires_default_profile_when_profiles_exist() {
    let temp = TempDir::new().unwrap();

    // Create config with a profile that is NOT named "default"
    fs::write(
        temp.path().join("nyl-project.yaml"),
        r#"
profiles:
  production:
    values:
      environment: production
"#,
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("apply");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("Profile 'default' not found"))
        .stderr(predicate::str::contains("Available profiles: production"));
}

#[test]
fn test_new_project_command() {
    let temp = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("new").arg("project").arg("test-project");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Project created successfully"));

    // Verify project structure (defaults to TOML format with hidden file)
    let project_dir = temp.path().join("test-project");
    assert!(project_dir.exists());
    assert!(project_dir.join(".nyl-project.toml").exists());
    assert!(project_dir.join("components").exists());
}

#[test]
fn test_new_without_subcommand_shows_error() {
    let temp = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("new").arg("test-project");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn test_new_component_command() {
    let temp = TempDir::new().unwrap();

    // Create a project first
    let config_path = temp.path().join("nyl-project.yaml");
    fs::write(&config_path, "settings: {}").unwrap();

    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("new").arg("component").arg("v1.example.io").arg("MyApp");
    cmd.assert().success().stdout(predicate::str::contains(
        "Component 'v1.example.io/MyApp' created successfully",
    ));

    // Verify component structure
    let component_dir = components_dir.join("v1.example.io").join("MyApp");
    assert!(component_dir.exists());
    assert!(component_dir.join("Chart.yaml").exists());
    assert!(component_dir.join("values.yaml").exists());
    assert!(component_dir.join("values.schema.json").exists());
    assert!(component_dir.join("templates").join("deployment.yaml").exists());
}

#[test]
fn test_validate_command_with_config() {
    let temp = TempDir::new().unwrap();

    // Create a valid project
    let config_path = temp.path().join("nyl-project.yaml");
    fs::write(&config_path, "settings: {}").unwrap();

    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("validate");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Found project config"))
        .stdout(predicate::str::contains("Validation passed"));
}

#[test]
fn test_validate_command_no_config() {
    let temp = TempDir::new().unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("validate");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("No project configuration file found"));
}

#[test]
fn test_validate_command_strict_mode() {
    let temp = TempDir::new().unwrap();

    // Create config with invalid on_lookup_failure
    let config_path = temp.path().join("nyl-project.yaml");
    fs::write(&config_path, "settings:\n  on_lookup_failure: InvalidValue").unwrap();

    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.current_dir(temp.path());
    cmd.arg("validate").arg("--strict");
    cmd.assert()
        .failure()
        .stdout(predicate::str::contains("Validation failed in strict mode"));
}

#[test]
fn test_verbose_flag() {
    let mut cmd = Command::cargo_bin("nyl").unwrap();
    cmd.arg("--verbose").arg("validate");
    // Should succeed and enable verbose logging
    cmd.assert().success();
}
