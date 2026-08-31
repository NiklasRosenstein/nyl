use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn create_gitops_target_fixture(temp: &TempDir) {
    git2::Repository::init(temp.path()).unwrap();
    fs::create_dir_all(temp.path().join("config/clusters")).unwrap();
    fs::create_dir_all(temp.path().join("config/targets")).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();
    fs::write(
        temp.path().join("config/clusters/kasoku.yaml"),
        r#"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: Cluster
metadata:
  name: kasoku
spec:
  destination:
    name: kasoku
  kubernetes:
    kubeVersion: 1.31.0
    apiVersions: [v1, apps/v1]
  values:
    region: fsn1
    environment: cluster-default
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/targets/production.yaml"),
        r#"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: production
spec:
  clusterRef:
    name: kasoku
  values:
    environment: production
  publication:
    repository:
      repoURL: https://example.com/deploy.git
    revision: deploy/production
    pathPrefix: production
"#,
    )
    .unwrap();
}

#[test]
fn test_cli_help() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Kubernetes manifest generator"));
}

#[test]
fn test_cli_version() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("--version");
    cmd.assert().success().stdout(predicate::str::contains("nyl"));
}

#[test]
fn test_generate_schema_config_command() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("generate").arg("schema").arg("config");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("\"project\""))
        .stdout(predicate::str::contains("\"components_search_paths\""))
        .stdout(predicate::str::contains("\"helm_chart_search_paths\""));
}

#[test]
fn test_render_command_basic() {
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

    // Run render command with file path
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--offline")
        .arg("--kube-version")
        .arg("1.28.0")
        .arg("--kube-api-versions")
        .arg("v1,apps/v1")
        .arg("test-resource.yaml");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("apiVersion: v1"))
        .stdout(predicate::str::contains("kind: ConfigMap"));
}

#[test]
fn test_cluster_list_is_static_and_target_render_merges_values() {
    let temp = TempDir::new().unwrap();
    create_gitops_target_fixture(&temp);
    fs::write(
        temp.path().join("application.yaml"),
        r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: rendered
data:
  environment: "{{ values.environment }}"
  region: "{{ values.region }}"
  cluster: "{{ cluster.metadata.name }}"
  target: "{{ target.metadata.name }}"
"#,
    )
    .unwrap();

    let mut list = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    list.current_dir(temp.path()).args(["cluster", "list"]);
    list.assert().success().stdout(predicate::eq("kasoku\n"));

    let mut render = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    render
        .current_dir(temp.path())
        .args(["render", "--offline", "--target", "production", "application.yaml"]);
    render
        .assert()
        .success()
        .stdout(predicate::str::contains("environment: production"))
        .stdout(predicate::str::contains("region: fsn1"))
        .stdout(predicate::str::contains("cluster: kasoku"))
        .stdout(predicate::str::contains("target: production"));

    let mut conflicting = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    conflicting.current_dir(temp.path()).args([
        "render",
        "--offline",
        "--target",
        "production",
        "--kube-version",
        "1.32.0",
        "application.yaml",
    ]);
    conflicting
        .assert()
        .failure()
        .stderr(predicate::str::contains("Cluster resource is authoritative"));
}

#[test]
fn test_diff_command_requires_target() {
    let temp = TempDir::new().unwrap();

    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();

    // Create a simple resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("diff").arg("test-resource.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("nyl diff requires --target"));
}

#[test]
fn test_apply_command_requires_target() {
    let temp = TempDir::new().unwrap();

    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();

    // Create a simple resource file
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("apply").arg("test-resource.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("nyl apply requires --target"));
}

#[test]
fn test_new_project_command() {
    let temp = TempDir::new().unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("new").arg("project").arg("test-project");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Project created successfully"));

    // Verify project structure (defaults to TOML format with hidden file)
    let project_dir = temp.path().join("test-project");
    assert!(project_dir.exists());
    assert!(project_dir.join("nyl.toml").exists());
    assert!(project_dir.join("components").exists());
}

#[test]
fn test_new_without_subcommand_shows_error() {
    let temp = TempDir::new().unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
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
    let config_path = temp.path().join("nyl.toml");
    fs::write(&config_path, "[project]\n").unwrap();

    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
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
    let config_path = temp.path().join("nyl.toml");
    fs::write(&config_path, "[project]\n").unwrap();

    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
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

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("validate");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("No project configuration file found"));
}

#[test]
fn test_validate_command_strict_mode() {
    let temp = TempDir::new().unwrap();

    // Create config with a missing component path (warning in strict mode)
    let config_path = temp.path().join("nyl.toml");
    fs::write(
        &config_path,
        "[project]\ncomponents_search_paths = [\"does-not-exist\"]\n",
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("validate").arg("--strict");
    cmd.assert()
        .failure()
        .stdout(predicate::str::contains("Validation failed in strict mode"));
}

#[test]
fn test_verbose_flag() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("--verbose").arg("validate");
    // Should succeed and enable verbose logging
    cmd.assert().success();
}

#[test]
fn test_render_missing_file_in_argocd_prints_diagnostics() {
    let temp = TempDir::new().unwrap();

    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render").arg("does-not-exist.yaml");
    cmd.env("ARGOCD_APP_NAME", "demo-app");
    cmd.env("ARGOCD_APP_NAMESPACE", "argocd");
    cmd.env("ARGOCD_APP_SOURCE_PATH", "apps");
    cmd.env("ARGOCD_ENV_NYL_CMP_TEMPLATE_INPUT", "does-not-exist.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("File not found: does-not-exist.yaml"))
        .stderr(predicate::str::contains(
            "[nyl-debug] ---- begin argocd file-not-found diagnostics ----",
        ))
        .stderr(predicate::str::contains("[nyl-debug] env.ARGOCD_APP_NAME=demo-app"))
        .stderr(predicate::str::contains(
            "[nyl-debug] render_input.raw=does-not-exist.yaml",
        ))
        .stderr(predicate::str::contains(
            "[nyl-debug] ---- end argocd file-not-found diagnostics ----",
        ));
}

#[test]
fn test_render_missing_file_outside_argocd_does_not_print_diagnostics() {
    let temp = TempDir::new().unwrap();

    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render").arg("does-not-exist.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("File not found: does-not-exist.yaml"))
        .stderr(predicate::str::contains("[nyl-debug] ---- begin argocd file-not-found diagnostics ----").not());
}

#[test]
fn test_render_rejects_removed_profile_flag_without_argocd_diagnostics() {
    let temp = TempDir::new().unwrap();

    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();
    fs::write(
        temp.path().join("test-resource.yaml"),
        r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: test-config
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render")
        .arg("--profile")
        .arg("missing")
        .arg("test-resource.yaml");
    cmd.env("ARGOCD_APP_NAME", "demo-app");
    cmd.env("ARGOCD_APP_NAMESPACE", "argocd");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument '--profile'"))
        .stderr(predicate::str::contains("[nyl-debug] ---- begin argocd file-not-found diagnostics ----").not());
}
