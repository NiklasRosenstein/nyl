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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
kind: DeploymentTarget
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
fn tree_commands_expose_explicit_secret_input_admission() {
    for command in ["render-tree", "diff-tree", "publish-tree"] {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
        cmd.args([command, "--help"]);
        cmd.assert()
            .success()
            .stdout(predicate::str::contains("--allow-secret-inputs"));
    }
}

#[test]
fn test_cli_version() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("--version");
    cmd.assert().success().stdout(predicate::str::contains("nyl"));
}

#[test]
fn test_render_reuses_the_shared_bundle_cache() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();
    fs::write(
        temp.path().join("app.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: cached\n",
    )
    .unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["render", "app.yaml", "--offline"])
        .assert()
        .success();

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .env("RUST_LOG", "nyl::render::session=debug")
        .args(["render", "app.yaml", "--offline"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Reusing cached rendered Release"));
}

#[test]
fn project_resource_commands_are_exposed_at_the_top_level() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("create"))
        .stdout(predicate::str::contains("get"))
        .stdout(predicate::str::contains("update"))
        .stdout(predicate::str::contains("delete"));
}

#[test]
fn removed_command_families_are_rejected() {
    for command in ["new", "target", "cluster", "source", "generate"] {
        Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .arg(command)
            .arg("--help")
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand"));
    }
}

#[test]
fn test_create_cluster_records_explicit_context_without_contacting_kubernetes() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    let kubeconfig = temp.path().join("kubeconfig.yaml");
    fs::write(
        &kubeconfig,
        r#"apiVersion: v1
kind: Config
contexts:
  - name: admin@primary
    context:
      cluster: primary
clusters: []
users: []
"#,
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path()).env("KUBECONFIG", &kubeconfig).args([
        "create",
        "cluster",
        "primary",
        "--context",
        "admin@primary",
    ]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("config/clusters/primary.yaml"));

    let cluster = fs::read_to_string(temp.path().join("config/clusters/primary.yaml")).unwrap();
    assert!(cluster.contains("context: admin@primary"));
    assert!(cluster.contains("apiVersions: []"));
}

#[test]
fn test_create_cluster_does_not_inspect_kubeconfig() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    let kubeconfig = temp.path().join("kubeconfig.yaml");
    fs::write(
        &kubeconfig,
        "apiVersion: v1\nkind: Config\ncontexts: []\nclusters: []\nusers: []\n",
    )
    .unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path())
        .env("KUBECONFIG", &kubeconfig)
        .args(["create", "cluster", "primary"]);
    cmd.assert().success().stderr(predicate::str::is_empty());
}

#[test]
fn test_create_repository_requires_and_writes_repository_urls() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();

    let mut missing = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    missing
        .current_dir(temp.path())
        .args(["create", "repository", "deploy"]);
    missing
        .assert()
        .failure()
        .stderr(predicate::str::contains("--repo-url"));

    let mut create = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    create.current_dir(temp.path()).args([
        "create",
        "repository",
        "deploy",
        "--repo-url",
        "https://git.example.com/platform/deploy.git",
        "--publish-url",
        "git@git.example.com:platform/deploy.git",
    ]);
    create.assert().success();

    let repository = fs::read_to_string(temp.path().join("config/repositories/deploy.yaml")).unwrap();
    assert!(repository.contains("repoURL: \"https://git.example.com/platform/deploy.git\""));
    assert!(repository.contains("publishURL: \"git@git.example.com:platform/deploy.git\""));
}

#[test]
fn test_create_argocd_instance_uses_documented_name() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .current_dir(temp.path())
        .args(["create", "argocd-instance", "central"]);
    command.assert().success();
    let resource = fs::read_to_string(temp.path().join("config/argocd-instances/central.yaml")).unwrap();
    assert!(resource.contains("kind: ArgoCDInstance"));
}

#[test]
fn create_get_and_delete_edit_the_primary_gitops_file() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    let original = "# primary configuration\napiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: deploy\nspec:\n  repoURL: https://git.example.invalid/deploy.git\n";
    fs::write(temp.path().join("gitops.yaml"), original).unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["create", "cluster", "production", "--context", "admin@production"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created Cluster in gitops.yaml"));

    let created = fs::read_to_string(temp.path().join("gitops.yaml")).unwrap();
    assert!(created.starts_with(original));
    assert!(created.contains("---\n# yaml-language-server:"));
    assert!(created.contains("kind: Cluster"));

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["get", "cluster", "production"])
        .assert()
        .success()
        .stdout(predicate::str::contains("NAME        DESTINATION"))
        .stdout(predicate::str::contains("gitops.yaml#document-2"));

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["delete", "cluster", "production", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would delete Cluster \"production\""));
    assert_eq!(fs::read_to_string(temp.path().join("gitops.yaml")).unwrap(), created);

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["delete", "cluster", "production"])
        .assert()
        .success();
    assert_eq!(fs::read_to_string(temp.path().join("gitops.yaml")).unwrap(), original);
}

#[test]
fn delete_removes_a_dedicated_resource_file() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["create", "cluster", "production"])
        .assert()
        .success();
    let resource = temp.path().join("config/clusters/production.yaml");
    assert!(resource.is_file());

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["delete", "cluster", "production"])
        .assert()
        .success();
    assert!(!resource.exists());
}

#[test]
fn delete_refuses_to_break_remaining_resource_references() {
    let temp = TempDir::new().unwrap();
    git2::Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    let resources = "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: deploy\nspec:\n  repoURL: https://git.example.invalid/deploy.git\n---\napiVersion: gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: primary\nspec:\n  destination:\n    name: primary\n  kubernetes:\n    apiVersions: []\n---\napiVersion: gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: primary\nspec:\n  publication:\n    repositoryRef:\n      name: deploy\n    revision: deploy/primary\n";
    let path = temp.path().join("gitops.yaml");
    fs::write(&path, resources).unwrap();

    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(["delete", "cluster", "primary"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Cannot delete Cluster \"primary\" because the remaining project is invalid",
        ));
    assert_eq!(fs::read_to_string(path).unwrap(), resources);
}

#[test]
fn vendor_modes_are_mutually_exclusive() {
    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .args(["vendor", "--check", "--prune"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .args(["vendor", "--prune", "--target", "production"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn test_schema_config_command() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.arg("schema").arg("config");
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
fn test_render_command_expands_release_includes() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();
    fs::create_dir(temp.path().join("manifests")).unwrap();
    fs::write(
        temp.path().join("release.yaml"),
        "apiVersion: gitops.nyl/v1\nkind: Release\nmetadata:\n  name: example\n  namespace: example\nspec:\n  include: [manifests/*.yaml]\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("manifests/config.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: included\n  namespace: example\n",
    )
    .unwrap();

    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    command
        .current_dir(temp.path())
        .args(["render", "--offline", "release.yaml"]);
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("name: included"))
        .stdout(predicate::str::contains("kind: Release").not());
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
    list.current_dir(temp.path()).args(["get", "clusters"]);
    list.assert()
        .success()
        .stdout(predicate::str::contains("kasoku"))
        .stdout(predicate::str::contains("config/clusters/kasoku.yaml#document-1"));

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
fn test_diff_command_reports_missing_configured_target() {
    let temp = TempDir::new().unwrap();

    git2::Repository::init(temp.path()).unwrap();
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
    cmd.assert().failure().stderr(predicate::str::contains(
        "requires a DeploymentTarget, but none are configured",
    ));
}

#[test]
fn test_apply_command_reports_missing_configured_target() {
    let temp = TempDir::new().unwrap();

    git2::Repository::init(temp.path()).unwrap();
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
    cmd.assert().failure().stderr(predicate::str::contains(
        "requires a DeploymentTarget, but none are configured",
    ));
}

#[test]
fn test_init_minimal_project_command() {
    let temp = TempDir::new().unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("init").arg("test-project").arg("--minimal");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Initialized Nyl project"));

    // Verify project structure (defaults to TOML format with hidden file)
    let project_dir = temp.path().join("test-project");
    assert!(project_dir.exists());
    assert!(project_dir.join("nyl.toml").exists());
    assert!(project_dir.join("components").exists());
}

#[test]
fn test_create_component_command() {
    let temp = TempDir::new().unwrap();

    // Create a project first
    let config_path = temp.path().join("nyl.toml");
    fs::write(&config_path, "[project]\n").unwrap();

    let components_dir = temp.path().join("components");
    fs::create_dir(&components_dir).unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("create").arg("component").arg("v1.example.io").arg("MyApp");
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
fn test_render_missing_file_reports_path() {
    let temp = TempDir::new().unwrap();

    fs::write(temp.path().join("nyl.toml"), "[project]\n").unwrap();
    fs::write(temp.path().join("secrets.yaml"), "provider: null\n").unwrap();

    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("nyl"));
    cmd.current_dir(temp.path());
    cmd.arg("render").arg("does-not-exist.yaml");
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("File not found: does-not-exist.yaml"));
}

#[test]
fn test_render_rejects_removed_profile_flag() {
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
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument '--profile'"));
}
