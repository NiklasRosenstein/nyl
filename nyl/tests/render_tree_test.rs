use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use git2::Repository;
use predicates::prelude::*;
use tempfile::TempDir;

fn read_tree(root: &std::path::Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let path = entry.path().strip_prefix(root).unwrap().to_path_buf();
            (path, fs::read(entry.path()).unwrap())
        })
        .collect()
}

fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    Repository::init(temp.path()).unwrap();
    fs::write(temp.path().join("nyl.toml"), "").unwrap();
    for directory in [
        "config/repositories",
        "config/clusters",
        "config/targets",
        "config/projects",
        "config/application-groups",
        "applications/workloads",
    ] {
        fs::create_dir_all(temp.path().join(directory)).unwrap();
    }
    fs::write(
        temp.path().join("config/repositories/deploy.yaml"),
        r#"apiVersion: gitops.nyl/v1
kind: GitRepository
metadata:
  name: deploy
spec:
  repoURL: https://example.invalid/deploy.git
  publishURL: ssh://git@example.invalid/deploy.git
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/clusters/kasoku.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Cluster
metadata:
  name: kasoku
spec:
  destination:
    server: https://kubernetes.default.svc
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions:
      - v1
      - apps/v1
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/targets/production.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
  labels:
    environment: production
spec:
  clusterRef:
    name: kasoku
  applicationGroupSelector:
    matchLabels:
      environment: production
  values:
    environment: production
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: production
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/projects/workloads.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: AppProjectDefinition
metadata:
  name: workloads
spec:
  management: Rendered
  sourceRepositoryRefs:
    - name: deploy
  manifest:
    apiVersion: argoproj.io/v1alpha1
    kind: AppProject
    metadata:
      name: workloads
      namespace: argocd
    spec:
      sourceRepos:
        - https://charts.example.invalid
      destinations:
        - server: https://kubernetes.default.svc
          namespace: '*'
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/application-groups/workloads.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: workloads
  labels:
    environment: production
spec:
  projectRef: workloads
  applicationNamespace: 'argocd-{{ target.metadata.labels.environment }}'
{% if target.metadata.labels.environment == 'production' %}
  annotations:
    environment: production
{% endif %}
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("applications/workloads/api.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: api
  namespace: api
data:
  environment: '{{ values.environment }}'
"#,
    )
    .unwrap();
    temp
}

fn colocate_gitops_resources(root: &std::path::Path) -> PathBuf {
    let resources = [
        "config/repositories/deploy.yaml",
        "config/clusters/kasoku.yaml",
        "config/targets/production.yaml",
        "config/projects/workloads.yaml",
        "config/application-groups/workloads.yaml",
    ];
    let mut documents = Vec::new();
    for relative in resources {
        let path = root.join(relative);
        documents.push(fs::read_to_string(&path).unwrap());
        fs::remove_file(path).unwrap();
    }
    let path = root.join("gitops.yaml");
    fs::write(&path, documents.join("\n---\n")).unwrap();
    path
}

fn commit_all(repository: &Repository, message: &str) {
    let mut index = repository.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
    let parents = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .into_iter()
        .collect::<Vec<_>>();
    let parent_refs = parents.iter().collect::<Vec<_>>();
    repository
        .commit(Some("HEAD"), &signature, &signature, message, &tree, &parent_refs)
        .unwrap();
}

fn seeded_bare_repository() -> (TempDir, TempDir) {
    let bare_dir = TempDir::new().unwrap();
    Repository::init_bare(bare_dir.path()).unwrap();
    let seed_dir = TempDir::new().unwrap();
    let seed = Repository::init(seed_dir.path()).unwrap();
    fs::write(seed_dir.path().join("README.md"), "deployment repository\n").unwrap();
    commit_all(&seed, "Initial");
    let head = seed.head().unwrap().peel_to_commit().unwrap();
    seed.branch("main", &head, true).unwrap();
    seed.set_head("refs/heads/main").unwrap();
    seed.remote("origin", bare_dir.path().to_str().unwrap()).unwrap();
    seed.find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap();
    Repository::open_bare(bare_dir.path())
        .unwrap()
        .set_head("refs/heads/main")
        .unwrap();
    (bare_dir, seed_dir)
}

fn publication_fixture() -> (TempDir, TempDir, TempDir, git2::Oid) {
    let fixture = fixture();
    let (destination, seed) = seeded_bare_repository();
    fs::write(
        fixture.path().join("config/repositories/deploy.yaml"),
        format!(
            "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: deploy\nspec:\n  repoURL: {}\n  publishURL: {}\n",
            destination.path().display(),
            destination.path().display()
        ),
    )
    .unwrap();
    let source = Repository::open(fixture.path()).unwrap();
    let mut source_config = source.config().unwrap();
    source_config.set_str("user.name", "Nyl Tests").unwrap();
    source_config
        .set_str("user.email", "nyl-tests@example.invalid")
        .unwrap();
    source.remote("origin", "https://example.invalid/source.git").unwrap();
    commit_all(&source, "Source");
    let source_commit = source.head().unwrap().peel_to_commit().unwrap().id();
    (fixture, destination, seed, source_commit)
}

fn configure_validation(root: &std::path::Path, data_type: &str) {
    fs::write(root.join("nyl.toml"),
        "[validation]\nenabled=true\n[validation.kubeconform]\nbuiltin_schemas=\"vendor-used\"\nschema_locations=['schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json']\n").unwrap();
    for (group, kind, version) in [
        ("v1", "configmap", "v1"),
        ("v1", "namespace", "v1"),
        ("argoproj.io", "application", "v1alpha1"),
        ("argoproj.io", "appproject", "v1alpha1"),
    ] {
        let directory = root.join("schemas").join(group);
        fs::create_dir_all(&directory).unwrap();
        let schema = if kind == "configmap" {
            serde_json::json!({"type":"object","properties":{"data":{"type":"object","additionalProperties":{"type":data_type}}}})
        } else {
            serde_json::json!({"type":"object"})
        };
        fs::write(
            directory.join(format!("{kind}_{version}.json")),
            serde_json::to_vec(&schema).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn vendor_commands_resolve_relative_project_and_vendor_paths() {
    let fixture = fixture();
    for vendor_path in ["vendor", "third-party"] {
        let vendor_config = format!("[vendor]\nmode='required'\npath='{vendor_path}'\n");
        fs::write(fixture.path().join("nyl.toml"), &vendor_config).unwrap();
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .arg("vendor")
            .assert()
            .success();
        assert!(fixture.path().join(vendor_path).join("lock.yaml").is_file());
        configure_validation(fixture.path(), "string");
        let config = fs::read_to_string(fixture.path().join("nyl.toml")).unwrap();
        fs::write(fixture.path().join("nyl.toml"), format!("{config}\n{vendor_config}")).unwrap();
        for args in [["vendor", "."], ["vendor", "--check"]] {
            Command::cargo_bin("nyl")
                .unwrap()
                .current_dir(fixture.path())
                .timeout(std::time::Duration::from_secs(30))
                .args(args)
                .assert()
                .success();
        }
        let output = TempDir::new().unwrap();
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["render-tree", "--output-dir"])
            .arg(output.path())
            .assert()
            .success()
            .stderr(predicate::str::contains("0 invalid, 0 errors"));
        assert!(fixture.path().join(vendor_path).join("schemas/builtins.json").is_file());
    }
}

#[test]
fn vendor_check_reports_prunable_files_without_modifying_snapshot() {
    use sha2::{Digest, Sha256};

    let fixture = fixture();
    fs::write(fixture.path().join("nyl.toml"), "[vendor]\nmode='required'\n").unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .arg("vendor")
        .assert()
        .success();

    let vendor = fixture.path().join("vendor");
    fs::write(vendor.join("artifacts/unused.yaml"), "unused artifact").unwrap();
    let schemas = vendor.join("schemas/blobs");
    fs::create_dir_all(&schemas).unwrap();
    let digest = hex::encode(Sha256::digest(b"{}"));
    fs::write(schemas.join(format!("{digest}.json")), b"{}").unwrap();
    let before = read_tree(&vendor);
    for selection in [vec![], vec!["--target", "production"]] {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["vendor", "--check"])
            .args(selection)
            .assert()
            .success()
            .stdout(predicate::str::contains("Vendor snapshot is complete and valid"))
            .stdout(predicate::str::contains(
                "Hint: 2 unreferenced vendor artifact(s) can be pruned; run 'nyl vendor --prune'",
            ));
        assert_eq!(read_tree(&vendor), before);
    }

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["vendor", "--prune"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pruned 2 unreferenced vendor artifact(s)"));
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["vendor", "--check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Hint:").not());
}

#[test]
fn validation_failure_writes_inspectable_tree_and_rechecks_cached_artifacts() {
    let fixture = fixture();
    let output = TempDir::new().unwrap();
    configure_validation(fixture.path(), "string");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["render-tree", "--target", "production", "--output-dir"])
        .arg(output.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("0 invalid, 0 errors"));
    let before = read_tree(output.path());
    fs::write(
        fixture.path().join("schemas/v1/configmap_v1.json"),
        r#"{"type":"object","properties":{"data":{"type":"object","additionalProperties":{"type":"integer"}}}}"#,
    )
    .unwrap();
    let manifest = fixture.path().join("applications/workloads/api.yaml");
    fs::write(
        &manifest,
        fs::read_to_string(&manifest)
            .unwrap()
            .replace("\ndata:\n", "\ndata:\n  inspection: available\n"),
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["render-tree", "--check", "--output-dir"])
        .arg(output.path())
        .assert()
        .failure();
    assert_eq!(read_tree(output.path()), before);

    let fresh_output = TempDir::new().unwrap();
    for destination in [output.path(), fresh_output.path(), fresh_output.path()] {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["render-tree", "--target", "production", "--output-dir"])
            .arg(destination)
            .assert()
            .failure()
            .stderr(predicate::str::contains("1 invalid"));
        let resources = fs::read_to_string(destination.join("production/workloads/api/resources.yaml")).unwrap();
        assert!(resources.contains("inspection: available"));
        assert!(destination.join("production/_nyl/index.json").is_file());
    }
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["diff-tree", "--target", "production", "--catalog"])
        .assert()
        .failure()
        .stdout("")
        .stderr(predicate::str::contains("1 invalid"));
}

#[test]
fn validation_prevents_invalid_publication_even_when_output_is_already_published() {
    let (fixture, destination, _seed, _) = publication_fixture();
    configure_validation(fixture.path(), "string");
    let source = Repository::open(fixture.path()).unwrap();
    commit_all(&source, "Configure validation");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success();
    let remote = Repository::open_bare(destination.path()).unwrap();
    let before = remote
        .find_reference("refs/heads/deploy/production")
        .unwrap()
        .target()
        .unwrap();
    configure_validation(fixture.path(), "integer");
    commit_all(&source, "Require integer ConfigMap values in fixture");
    for dry_run in [false, true] {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["publish-tree", "--target", "production"]);
        if dry_run {
            command.arg("--dry-run");
        }
        command.assert().failure().stderr(predicate::str::contains("1 invalid"));
    }
    assert_eq!(
        remote
            .find_reference("refs/heads/deploy/production")
            .unwrap()
            .target()
            .unwrap(),
        before
    );
}

#[test]
fn validation_render_overrides_and_complete_scope_are_explicit() {
    let fixture = fixture();
    configure_validation(fixture.path(), "integer");
    let arguments = [
        "render",
        "applications/workloads/api.yaml",
        "--offline",
        "--target",
        "production",
    ];
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(arguments)
        .assert()
        .failure()
        .stdout(predicate::str::contains("kind: ConfigMap"))
        .stderr(predicate::str::contains("1 invalid"));
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(arguments)
        .arg("--no-validate")
        .assert()
        .success()
        .stdout(predicate::str::contains("kind: ConfigMap"));
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(arguments)
        .args(["--use-desired-crds", "--only-kind", "ConfigMap"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with resource filters"));
    fs::write(fixture.path().join("nyl.toml"), "").unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(arguments)
        .arg("--validate")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no validators configured"));
}

#[test]
fn inherited_cluster_capabilities_render_without_production_access() {
    let fixture = fixture();
    fs::write(fixture.path().join("config/clusters/production.yaml"),
        "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: production\nspec:\n  destination:\n    server: https://production.invalid\n  apiContractFrom:\n    clusterRef:\n      name: kasoku\n    mode: all\n").unwrap();
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path)
        .unwrap()
        .replace("name: kasoku", "name: production");
    fs::write(target_path, target).unwrap();
    let manifest_path = fixture.path().join("applications/workloads/api.yaml");
    fs::write(&manifest_path,
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: capabilities\ndata:\n  version: '{{ cluster.spec.kubernetes.kubeVersion }}'\n  server: '{{ cluster.spec.destination.server }}'\n").unwrap();
    let mut command = Command::cargo_bin("nyl").unwrap();
    command
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["render", "--offline", "--target", "production"])
        .arg(&manifest_path);
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("1.31.4"))
        .stdout(predicate::str::contains("https://production.invalid"));
    let cluster_path = fixture.path().join("config/clusters/kasoku.yaml");
    fs::write(
        &cluster_path,
        fs::read_to_string(&cluster_path).unwrap().replace("1.31.4", "1.32.0"),
    )
    .unwrap();
    command.assert().success().stdout(predicate::str::contains("1.32.0"));
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["capture", "cluster", "production"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("capture source Cluster kasoku"));
}

fn published_commit<'repo>(repository: &'repo Repository, branch: &str) -> git2::Commit<'repo> {
    repository
        .find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .peel_to_commit()
        .unwrap()
}

fn published_file(repository: &Repository, commit: &git2::Commit<'_>, path: &str) -> Vec<u8> {
    let entry = commit.tree().unwrap().get_path(std::path::Path::new(path)).unwrap();
    repository.find_blob(entry.id()).unwrap().content().to_vec()
}

#[test]
fn renders_plain_directory_applications_and_owned_layout() {
    let fixture = fixture();
    let output = fixture.path().join("deploy-worktree");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            ".",
            "--output-dir",
            "deploy-worktree",
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "deployment target production ready at deploy-worktree/production",
        ))
        .stderr(predicate::str::contains(
            "[1/1] Release workloads/api (applications/workloads/api.yaml)",
        ));

    let root = output.join("production");
    let resources = fs::read_to_string(root.join("workloads/api/resources.yaml")).unwrap();
    assert!(resources.contains("kind: ConfigMap"));
    assert!(resources.contains("kind: Namespace"));
    assert!(resources.contains(
        "# Nyl-Provenance: Source: applications/workloads/api.yaml (document 2)\n# Nyl-Provenance: Resource: v1 ConfigMap api/api"
    ));
    assert!(resources.contains(
        "# Nyl-Provenance: Source: applications/workloads/api.yaml (document 1)\n# Nyl-Provenance: Resource: k8s.gitops.nyl/v1 Release api/api\n# Nyl-Provenance: Generated: Namespace \"api\" for Release \"api\""
    ));
    assert!(resources.contains("Delete=confirm,Prune=confirm"));
    assert!(!root.join("_nyl/namespaces").exists());

    let project = fs::read_to_string(root.join("_nyl/catalog/projects/workloads.yaml")).unwrap();
    assert!(project.contains("https://charts.example.invalid"));
    assert!(project.contains("https://example.invalid/deploy.git"));
    assert!(!project.contains("ssh://git@example.invalid/deploy.git"));

    let application = fs::read_to_string(root.join("_nyl/catalog/applications/argocd-production/api.yaml")).unwrap();
    assert!(application.contains("targetRevision: deploy/production"));
    assert!(application.contains("path: production/workloads/api"));
    assert!(application.contains("recurse: true"));
    assert!(!application.contains("plugin:"));
    assert!(application.contains("resources-finalizer.argocd.argoproj.io"));
    assert!(application.contains("environment: production"));
    assert!(application.contains("server: https://kubernetes.default.svc"));
    assert_eq!(application.matches("- ApplyOutOfSyncOnly=true").count(), 1);
    assert_eq!(application.matches("- ServerSideApply=true").count(), 1);
    assert!(!fs::read_dir(root.join("_nyl/catalog/applications/argocd-production"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("nyl-namespace-")));

    assert!(root.join("_nyl/catalog/projects/workloads.yaml").is_file());
    let catalog = fs::read_to_string(root.join("_nyl/catalog/applications/argocd/production-catalog.yaml")).unwrap();
    assert!(catalog.contains("project: default"));
    assert!(catalog.contains("path: production/_nyl/catalog"));
    assert!(catalog.contains("targetRevision: deploy/production"));
    assert!(catalog.contains("Prune=confirm"));
    assert!(!catalog.contains("automated:"));
    assert_eq!(catalog.matches("- ApplyOutOfSyncOnly=true").count(), 1);
    assert_eq!(catalog.matches("- ServerSideApply=true").count(), 1);
    let index: serde_json::Value = serde_json::from_slice(&fs::read(root.join("_nyl/index.json")).unwrap()).unwrap();
    assert_eq!(index["version"], 2);
    assert_eq!(index["target"], "production");
    assert_eq!(index["cluster"], "kasoku");
    assert_eq!(index["publication"]["repository"], "deploy");
    assert!(index.get("profile").is_none());
    assert!(index.get("destination").is_none());
    for input in [
        "config/targets/production.yaml",
        "config/clusters/kasoku.yaml",
        "config/repositories/deploy.yaml",
    ] {
        assert!(index["inputs"].get(input).is_some(), "missing provenance input {input}");
    }

    // Renderer implementation files beneath an application source are not
    // semantic inputs unless the ApplicationGroup include patterns select them.
    let nested_cache = fixture.path().join("applications/workloads/.nyl/cache");
    fs::create_dir_all(&nested_cache).unwrap();
    fs::write(nested_cache.join("noise.yaml"), "cache implementation detail\n").unwrap();

    // A byte-identical second render is accepted and keeps ownership stable.
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("RUST_LOG", "nyl_render::gitops::tree=debug")
        .args([
            "render-tree",
            ".",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Reusing cached deployment target tree"))
        .stderr(predicate::str::contains("[1/1] Release").not());

    let cached_tree = read_tree(&root);
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            ".",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
            "--refresh",
        ])
        .assert()
        .success();
    assert_eq!(read_tree(&root), cached_tree);

    let index_before_live_context = fs::read(root.join("_nyl/index.json")).unwrap();
    let cluster_path = fixture.path().join("config/clusters/kasoku.yaml");
    let cluster = fs::read_to_string(&cluster_path).unwrap();
    fs::write(&cluster_path, format!("{cluster}  live:\n    context: kind-kasoku\n")).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            ".",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(
        fs::read(root.join("_nyl/index.json")).unwrap(),
        index_before_live_context
    );
}

#[test]
fn requires_target_selection_when_multiple_targets_are_configured() {
    let fixture = fixture();
    let production = fs::read_to_string(fixture.path().join("config/targets/production.yaml")).unwrap();
    fs::write(
        fixture.path().join("config/targets/staging.yaml"),
        production.replacen("name: production", "name: staging", 1),
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["render-tree", ".", "--output-dir", "deploy-worktree"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--target is required because multiple DeploymentTargets are configured: production, staging",
        ));
}

#[test]
fn rejects_foreign_ownership_index_before_rendering_releases() {
    let fixture = fixture();
    let output = fixture.path().join("deploy-worktree");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["render-tree", ".", "--output-dir", "deploy-worktree"])
        .assert()
        .success();

    let index_path = output.join("production/_nyl/index.json");
    let mut index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    index["target"] = serde_json::Value::String("another-target".to_owned());
    fs::write(&index_path, serde_json::to_vec_pretty(&index).unwrap()).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["render-tree", ".", "--output-dir", "deploy-worktree", "--refresh"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("belongs to target \"another-target\""))
        .stderr(predicate::str::contains("expected target \"production\""))
        .stderr(predicate::str::contains("Release workloads/api").not());
}

#[test]
fn reports_missing_project_source_repository() {
    let fixture = fixture();
    let project_path = fixture.path().join("config/projects/workloads.yaml");
    let project = fs::read_to_string(&project_path)
        .unwrap()
        .replace("name: deploy", "name: missing");
    fs::write(project_path, project).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            ".",
            "--target",
            "production",
            "--output-dir",
            "deploy-worktree",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "AppProjectDefinition references GitRepository \"missing\", but it was not found",
        ));
}

#[test]
fn no_cache_render_leaves_no_persistent_cache() {
    let fixture = fixture();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
            "--no-cache",
            "--progress",
            "off",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("[1/1] Release").not());

    assert!(!fixture.path().join(".nyl/cache/gitops").exists());
}

#[test]
fn warm_target_cache_reports_the_rendering_work_it_avoids() {
    let fixture = fixture();
    let chart = fixture.path().join("components/Test");
    fs::create_dir_all(chart.join("templates")).unwrap();
    fs::write(chart.join("Chart.yaml"), "apiVersion: v2\nname: test\nversion: 1.0.0\n").unwrap();
    fs::write(
        chart.join("templates/configmap.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: from-helm\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("applications/workloads/helm.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: helm
  namespace: helm
---
apiVersion: components.k8s.nyl/v1
kind: Test
metadata:
  name: helm
  namespace: helm
"#,
    )
    .unwrap();
    let output = fixture.path().join("deploy");
    let args = [
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        output.to_str().unwrap(),
        "--check",
        "--color",
        "never",
    ];

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Render statistics\n  Cache\n    Target tree         reused\n    Release renders     2 avoided\n    Helm renders        1 avoided",
        ));

    let api = fixture.path().join("applications/workloads/api.yaml");
    let contents = fs::read_to_string(&api).unwrap();
    fs::write(
        &api,
        contents.replace("  environment:", "  changed: yes\n  environment:"),
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Render statistics\n  Cache\n    Target tree         rebuilt\n    Release renders     1 reused · 1 rendered\n    Helm renders        1 avoided",
        ));
}

#[test]
fn colocated_gitops_resources_use_semantic_target_cache_dependencies() {
    let fixture = fixture();
    let mut gitops = colocate_gitops_resources(fixture.path());
    let output = fixture.path().join("deploy");
    let args = [
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        output.to_str().unwrap(),
        "--color",
        "never",
    ];

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success();
    let initial_index: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("production/_nyl/index.json")).unwrap()).unwrap();

    let contents = fs::read_to_string(&gitops).unwrap();
    fs::write(&gitops, format!("# Repository-local GitOps resources\n{contents}")).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains("Target tree         reused"));
    let comment_index: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("production/_nyl/index.json")).unwrap()).unwrap();
    assert_ne!(
        initial_index["inputs"]["gitops.yaml"],
        comment_index["inputs"]["gitops.yaml"]
    );
    assert_eq!(initial_index["files"], comment_index["files"]);

    let contents = fs::read_to_string(&gitops).unwrap();
    fs::write(
        &gitops,
        format!(
            "{contents}\n---\napiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: unused\nspec:\n  repoURL: https://example.invalid/unused.git\n"
        ),
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains("Target tree         reused"));

    let moved = fixture.path().join("repository-gitops.yaml");
    fs::rename(&gitops, &moved).unwrap();
    gitops = moved;
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains("Target tree         rebuilt"))
        .stderr(predicate::str::contains("Release renders     1 reused"));
    let moved_index: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("production/_nyl/index.json")).unwrap()).unwrap();
    assert!(moved_index["inputs"].get("repository-gitops.yaml").is_some());
    assert!(moved_index["inputs"].get("gitops.yaml").is_none());

    let contents = fs::read_to_string(&gitops).unwrap();
    fs::write(
        &gitops,
        contents.replace(
            "  annotations:\n    environment: production\n{% endif %}",
            "  annotations:\n    environment: changed\n{% endif %}",
        ),
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains("Target tree         rebuilt"))
        .stderr(predicate::str::contains("Release renders     1 reused"));
}

#[test]
fn completion_can_colour_the_target_and_relative_output_path() {
    let fixture = fixture();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            "deploy",
            "--color",
            "always",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[1;36mproduction\u{1b}[0m"))
        .stdout(predicate::str::contains("\u{1b}[32mdeploy/production\u{1b}[0m"));
}

#[test]
fn test_automatic_colour_is_retained_in_ci_and_respects_no_color() {
    let fixture = fixture();
    let args = [
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        "deploy",
        "--check",
        "--progress",
        "off",
    ];

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("CI", "true")
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .args(args)
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[1;36mproduction\u{1b}[0m"))
        .stderr(predicate::str::contains("\u{1b}[1mRender statistics\u{1b}[0m"));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("CI", "true")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .args(args)
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[").not())
        .stderr(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn changing_one_release_reuses_unchanged_release_artifacts() {
    let fixture = fixture();
    let worker = fixture.path().join("applications/workloads/worker.yaml");
    fs::write(
        &worker,
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: worker
  namespace: worker
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: worker
  namespace: worker
"#,
    )
    .unwrap();
    let output = fixture.path().join("deploy");
    let args = [
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        output.to_str().unwrap(),
        "--check",
    ];
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success();

    let api = fixture.path().join("applications/workloads/api.yaml");
    let contents = fs::read_to_string(&api).unwrap();
    fs::write(
        &api,
        contents.replace("  environment:", "  changed: yes\n  environment:"),
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("RUST_LOG", "nyl_render::render::session=debug")
        .args(args)
        .assert()
        .success()
        .stderr(
            predicate::str::contains("Reusing cached rendered Release").and(predicate::str::contains("worker.yaml")),
        );
}

#[test]
fn rerendered_release_reuses_unchanged_helm_output() {
    let fixture = fixture();
    let chart = fixture.path().join("components/Test");
    fs::create_dir_all(chart.join("templates")).unwrap();
    fs::write(chart.join("Chart.yaml"), "apiVersion: v2\nname: test\nversion: 1.0.0\n").unwrap();
    fs::write(
        chart.join("templates/configmap.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: from-helm\n",
    )
    .unwrap();
    let release = fixture.path().join("applications/workloads/helm.yaml");
    fs::write(
        &release,
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: helm
  namespace: helm
---
apiVersion: components.k8s.nyl/v1
kind: Test
metadata:
  name: helm
  namespace: helm
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: alongside
  namespace: helm
data:
  revision: first
"#,
    )
    .unwrap();
    let output = fixture.path().join("deploy");
    let args = [
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        output.to_str().unwrap(),
        "--check",
    ];
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success();

    let contents = fs::read_to_string(&release).unwrap();
    fs::write(&release, contents.replace("revision: first", "revision: second")).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("RUST_LOG", "nyl_render::helm::template=debug")
        .args(args)
        .assert()
        .success()
        .stderr(predicate::str::contains("Reusing cached Helm output"));
}

#[test]
fn explicit_argocd_instances_are_strict_and_drive_the_catalog() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("config/argocd-instances")).unwrap();
    fs::write(
        fixture.path().join("config/argocd-instances/central.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata:
  name: central
spec:
  clusterRef:
    name: kasoku
  namespace: gitops-system
"#,
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must set spec.argocdRef"));

    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap().replace(
        "  clusterRef:\n    name: kasoku\n",
        "  clusterRef:\n    name: kasoku\n  argocdRef:\n    name: central\n",
    );
    fs::write(target_path, target).unwrap();
    let output = fixture.path().join("deploy");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    let catalog =
        fs::read_to_string(output.join("production/_nyl/catalog/applications/gitops-system/production-catalog.yaml"))
            .unwrap();
    assert!(catalog.contains("namespace: gitops-system"));
    let project = fs::read_to_string(output.join("production/_nyl/catalog/projects/workloads.yaml")).unwrap();
    assert!(project.contains("namespace: gitops-system"));
}

#[test]
fn project_templates_generate_constrained_projects() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  projectRef: workloads\n",
        "  projectTemplate:\n    destinationNamespaces:\n      - api\n      - shared-*\n",
    );
    fs::write(group_path, group).unwrap();
    let output = fixture.path().join("deploy");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    let project = fs::read_to_string(output.join("production/_nyl/catalog/projects/workloads.yaml")).unwrap();
    assert!(project.contains("sourceRepos:"));
    assert!(project.contains("https://example.invalid/deploy.git"));
    assert!(project.contains("sourceNamespaces:"));
    assert!(project.contains("argocd-production"));
    assert!(project.contains("namespace: api"));
    assert!(project.contains("kind: Namespace"));
    assert!(project.contains("name: api"));
}

#[test]
fn project_templates_reject_release_namespace_expansion() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  projectRef: workloads\n",
        "  projectTemplate:\n    destinationNamespaces:\n      - platform\n",
    );
    fs::write(group_path, group).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "outside ApplicationGroup.spec.projectTemplate",
        ));
}

#[test]
fn shared_argocd_instances_require_explicit_cross_target_names() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("config/argocd-instances")).unwrap();
    fs::write(
        fixture.path().join("config/argocd-instances/central.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: ArgoCDInstance
metadata:
  name: central
spec:
  clusterRef:
    name: kasoku
"#,
    )
    .unwrap();
    let production_path = fixture.path().join("config/targets/production.yaml");
    let production = fs::read_to_string(&production_path).unwrap().replace(
        "  clusterRef:\n    name: kasoku\n",
        "  clusterRef:\n    name: kasoku\n  argocdRef:\n    name: central\n",
    );
    fs::write(production_path, production).unwrap();
    fs::write(
        fixture.path().join("config/targets/staging.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: staging
  labels:
    environment: production
spec:
  clusterRef:
    name: kasoku
  argocdRef:
    name: central
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/staging
    pathPrefix: staging
"#,
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("applicationNameTemplate"));
}

const STAGING_TARGET_ON_KASOKU: &str = r#"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: staging
  labels:
    environment: production
spec:
  clusterRef:
    name: kasoku
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/staging
    pathPrefix: staging
"#;

#[test]
fn test_validate_implicit_argocd_instances_on_one_cluster_require_cross_target_names() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/targets/staging.yaml"),
        STAGING_TARGET_ON_KASOKU,
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("on Cluster \"kasoku\""))
        .stderr(predicate::str::contains(
            "'${ target.metadata.name }-${ release.metadata.name }'",
        ));
}

#[test]
fn test_validate_names_the_template_fix_for_implied_app_project_collisions() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/targets/staging.yaml"),
        STAGING_TARGET_ON_KASOKU,
    )
    .unwrap();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  projectRef: workloads\n",
        "  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'\n",
    );
    fs::write(group_path, group).unwrap();
    validate(&fixture)
        .failure()
        .stderr(predicate::str::contains(
            "generate the same AppProject argocd/workloads",
        ))
        .stderr(predicate::str::contains(
            "declaring ApplicationGroup.spec.projectTemplate with a target-qualified name",
        ));
}

#[test]
fn test_render_tree_expands_application_name_template_values_per_release() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  projectRef: workloads\n",
        "  projectRef: workloads\n  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'\n",
    );
    fs::write(group_path, group).unwrap();
    let output = fixture.path().join("deploy");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
    let application: serde_json::Value = serde_saphyr::from_str(
        &fs::read_to_string(output.join("production/_nyl/catalog/applications/argocd-production/production-api.yaml"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(application["metadata"]["name"], "production-api");
    assert_eq!(application["metadata"]["namespace"], "argocd-production");
}

/// Replace the fixture's group with `frontend` and `backend`, each holding a
/// Release `api`, and add a staging target on Cluster `cluster`; production
/// selects `frontend` and staging selects `backend`.
fn two_groups_with_one_release_name(fixture: &TempDir, cluster: &str, application_name_template: Option<&str>) {
    let root = fixture.path();
    fs::remove_file(root.join("config/application-groups/workloads.yaml")).unwrap();
    for group in ["frontend", "backend"] {
        let template = application_name_template
            .map(|template| format!("  applicationNameTemplate: '{template}'\n"))
            .unwrap_or_default();
        fs::write(
            root.join(format!("config/application-groups/{group}.yaml")),
            format!(
                "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: {group}\n  labels:\n    tier: {group}\nspec:\n  applicationNamespace: argocd\n{template}"
            ),
        )
        .unwrap();
        fs::create_dir_all(root.join(format!("applications/{group}"))).unwrap();
        fs::write(
            root.join(format!("applications/{group}/api.yaml")),
            "apiVersion: k8s.gitops.nyl/v1\nkind: Release\nmetadata:\n  name: api\n  namespace: api\n",
        )
        .unwrap();
    }
    let production_path = root.join("config/targets/production.yaml");
    let production = fs::read_to_string(&production_path).unwrap().replace(
        "      environment: production\n  values:",
        "      tier: frontend\n  values:",
    );
    fs::write(production_path, production).unwrap();
    fs::write(
        root.join("config/targets/staging.yaml"),
        STAGING_TARGET_ON_KASOKU
            .replace("    name: kasoku\n", &format!("    name: {cluster}\n"))
            .replace(
                "  publication:\n",
                "  applicationGroupSelector:\n    matchLabels:\n      tier: backend\n  publication:\n",
            ),
    )
    .unwrap();
}

fn validate(fixture: &TempDir) -> assert_cmd::assert::Assert {
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
}

#[test]
fn test_validate_rejects_default_application_names_of_different_groups_on_one_cluster() {
    let fixture = fixture();
    two_groups_with_one_release_name(&fixture, "kasoku", None);
    validate(&fixture).failure().stderr(predicate::str::contains(
        "generate the same Argo CD Application argocd/api on Cluster \"kasoku\"",
    ));
}

#[test]
fn test_validate_rejects_application_name_templates_without_the_target() {
    let fixture = fixture();
    two_groups_with_one_release_name(&fixture, "kasoku", Some("${ release.metadata.name }-app"));
    validate(&fixture)
        .failure()
        .stderr(predicate::str::contains("Application argocd/api-app"));
}

#[test]
fn test_validate_accepts_target_qualified_application_name_templates() {
    let fixture = fixture();
    two_groups_with_one_release_name(
        &fixture,
        "kasoku",
        Some("${ target.metadata.name }-${ release.metadata.name }"),
    );
    validate(&fixture).success();
}

#[test]
fn test_validate_compares_clusters_by_argocd_destination() {
    let fixture = fixture();
    let cluster = fs::read_to_string(fixture.path().join("config/clusters/kasoku.yaml")).unwrap();
    fs::write(
        fixture.path().join("config/clusters/kasoku-admin.yaml"),
        cluster.replace("name: kasoku\n", "name: kasoku-admin\n"),
    )
    .unwrap();
    two_groups_with_one_release_name(&fixture, "kasoku-admin", None);
    validate(&fixture)
        .failure()
        .stderr(predicate::str::contains("Application argocd/api"));
}

#[test]
fn test_validate_rejects_distinct_argocd_instances_sharing_a_namespace() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("config/argocd-instances")).unwrap();
    for instance in ["argocd-a", "argocd-b"] {
        fs::write(
            fixture.path().join(format!("config/argocd-instances/{instance}.yaml")),
            format!(
                "apiVersion: k8s.gitops.nyl/v1\nkind: ArgoCDInstance\nmetadata:\n  name: {instance}\nspec:\n  clusterRef:\n    name: kasoku\n"
            ),
        )
        .unwrap();
    }
    let production_path = fixture.path().join("config/targets/production.yaml");
    let production = fs::read_to_string(&production_path).unwrap().replace(
        "  clusterRef:\n    name: kasoku\n",
        "  clusterRef:\n    name: kasoku\n  argocdRef:\n    name: argocd-a\n",
    );
    fs::write(production_path, production).unwrap();
    fs::write(
        fixture.path().join("config/targets/staging.yaml"),
        STAGING_TARGET_ON_KASOKU.replace(
            "  clusterRef:\n    name: kasoku\n",
            "  clusterRef:\n    name: kasoku\n  argocdRef:\n    name: argocd-b\n",
        ),
    )
    .unwrap();
    validate(&fixture)
        .failure()
        .stderr(predicate::str::contains("on Cluster \"kasoku\""))
        .stderr(predicate::str::contains("applicationNameTemplate"));
}

#[test]
fn force_repairs_missing_and_modified_owned_files() {
    let fixture = fixture();
    let output = fixture.path().join("deploy-worktree");
    let args = [
        "render-tree",
        ".",
        "--target",
        "production",
        "--output-dir",
        output.to_str().unwrap(),
    ];
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .success();

    let resources = output.join("production/workloads/api/resources.yaml");
    fs::remove_file(&resources).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .assert()
        .failure()
        .stderr(predicate::str::contains("is missing or unreadable"));
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .arg("--force")
        .assert()
        .success()
        .stderr(predicate::str::contains("Recreating missing owned rendered file"));
    assert!(resources.is_file());

    fs::write(&resources, "manual edit\n").unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(args)
        .arg("--force")
        .assert()
        .success()
        .stderr(predicate::str::contains("Replacing modified owned rendered file"));
    assert!(!fs::read_to_string(resources).unwrap().contains("manual edit"));
}

#[test]
fn lists_and_validates_targets() {
    let fixture = fixture();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["get", "targets"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "production  kasoku   deploy@deploy/production  production",
        ));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid"));
}

#[test]
fn target_rendering_requires_complete_cluster_capabilities() {
    let fixture = fixture();
    let cluster_path = fixture.path().join("config/clusters/kasoku.yaml");
    let cluster = fs::read_to_string(&cluster_path)
        .unwrap()
        .replace("    kubeVersion: 1.31.4\n", "")
        .replace(
            "    apiVersions:\n      - v1\n      - apps/v1\n",
            "    apiVersions: []\n",
        );
    fs::write(&cluster_path, cluster).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires spec.kubernetes.kubeVersion"));

    let cluster = fs::read_to_string(&cluster_path)
        .unwrap()
        .replace("  kubernetes:\n", "  kubernetes:\n    kubeVersion: 1.31.4\n");
    fs::write(cluster_path, cluster).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "requires non-empty spec.kubernetes.apiVersions",
        ));
}

#[test]
fn validation_rejects_overlapping_target_prefixes_on_one_revision() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/targets/overlap.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: overlap
spec:
  clusterRef:
    name: kasoku
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: production/nested
",
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("overlapping publication path prefixes"));
}

#[test]
fn operational_commands_reject_overlaps_through_repository_aliases() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/repositories/deploy-alias.yaml"),
        r"apiVersion: gitops.nyl/v1
kind: GitRepository
metadata:
  name: deploy-alias
spec:
  repoURL: https://example.invalid/deploy.git
",
    )
    .unwrap();
    fs::write(
        fixture.path().join("config/targets/overlap.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: overlap
spec:
  clusterRef:
    name: kasoku
  publication:
    repositoryRef:
      name: deploy-alias
    revision: deploy/production
    pathPrefix: production/nested
",
    )
    .unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("overlapping publication path prefixes"));
}

#[test]
fn validation_rejects_overlaps_through_publish_urls_and_branch_ref_aliases() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/repositories/deploy-publisher.yaml"),
        r"apiVersion: gitops.nyl/v1
kind: GitRepository
metadata:
  name: deploy-publisher
spec:
  repoURL: https://example.invalid/another-read-repository.git
  publishURL: ssh://git@example.invalid/deploy.git
",
    )
    .unwrap();
    fs::write(
        fixture.path().join("config/targets/overlap.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: overlap
spec:
  clusterRef:
    name: kasoku
  publication:
    repositoryRef:
      name: deploy-publisher
    revision: refs/heads/deploy/production
    pathPrefix: production/nested
",
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("overlapping publication path prefixes"));
}

#[test]
fn application_groups_cannot_override_the_target_cluster_destination() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/application-groups/named-cluster.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: named-cluster
spec:
  projectRef: workloads
  applicationNamespace: argocd
  destination:
    name: in-cluster
",
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown field `destination`"));
}

#[test]
fn applications_inherit_a_named_cluster_destination() {
    let fixture = fixture();
    let cluster_path = fixture.path().join("config/clusters/kasoku.yaml");
    let cluster = fs::read_to_string(&cluster_path)
        .unwrap()
        .replace("    server: https://kubernetes.default.svc", "    name: in-cluster");
    fs::write(cluster_path, cluster).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let application =
        fs::read_to_string(output.join("production/_nyl/catalog/applications/argocd-production/api.yaml")).unwrap();
    assert!(application.contains("name: in-cluster"));
    assert!(!application.contains("server: https://kubernetes.default.svc"));
}

#[test]
fn one_dedicated_application_owns_a_shared_namespace() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  destinationNamespace: shared\n  sharedNamespaces:\n    shared:\n      owner:\n        kind: Dedicated\n        applicationGroup: workloads\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    let api_path = fixture.path().join("applications/workloads/api.yaml");
    let api = fs::read_to_string(&api_path)
        .unwrap()
        .replace("  namespace: api\ndata:", "  namespace: shared\ndata:");
    fs::write(api_path, api).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/worker.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: worker
  namespace: worker
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: worker
  namespace: shared
",
    )
    .unwrap();
    let output = fixture.path().join("deploy");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let root = output.join("production");
    assert_eq!(fs::read_dir(root.join("_nyl/namespaces")).unwrap().count(), 1);
    for release in ["api", "worker"] {
        let resources = fs::read_to_string(root.join(format!("workloads/{release}/resources.yaml"))).unwrap();
        assert!(!resources.contains("kind: Namespace"));
    }
}

#[test]
fn one_release_can_own_a_shared_namespace() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  destinationNamespace: shared\n  sharedNamespaces:\n    shared:\n      owner:\n        kind: Release\n        applicationGroup: workloads\n        release: api\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    let api_path = fixture.path().join("applications/workloads/api.yaml");
    let api = fs::read_to_string(&api_path)
        .unwrap()
        .replace("  namespace: api\ndata:", "  namespace: shared\ndata:");
    fs::write(api_path, api).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/worker.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: worker
  namespace: worker
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: worker
  namespace: shared
",
    )
    .unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let root = output.join("production");
    let api = fs::read_to_string(root.join("workloads/api/resources.yaml")).unwrap();
    let worker = fs::read_to_string(root.join("workloads/worker/resources.yaml")).unwrap();
    assert!(api.contains("kind: Namespace"));
    assert!(!worker.contains("kind: Namespace"));
    assert!(!root.join("_nyl/namespaces").exists());
}

#[test]
fn external_shared_namespace_is_not_managed() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  destinationNamespace: kube-system\n  sharedNamespaces:\n    kube-system:\n      owner:\n        kind: External\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    let api_path = fixture.path().join("applications/workloads/api.yaml");
    let api = fs::read_to_string(&api_path)
        .unwrap()
        .replace("  namespace: api\ndata:", "  namespace: kube-system\ndata:");
    fs::write(api_path, api).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/worker.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: worker
  namespace: worker
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: worker
  namespace: kube-system
",
    )
    .unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let root = output.join("production");
    for release in ["api", "worker"] {
        let resources = fs::read_to_string(root.join(format!("workloads/{release}/resources.yaml"))).unwrap();
        assert!(!resources.contains("kind: Namespace"));
    }
    assert!(!root.join("_nyl/namespaces").exists());
}

#[test]
fn kubernetes_bootstrap_namespaces_are_external_by_default() {
    let fixture = fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "  namespace: api\n---",
        "  namespace: api\nspec:\n  additionalNamespaces: [default]\n---",
    ) + r"---
apiVersion: v1
kind: ConfigMap
metadata:
  name: uses-default
  namespace: default
";
    fs::write(release_path, release).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let workload = fs::read_to_string(output.join("production/workloads/api/resources.yaml")).unwrap();
    assert!(workload.contains("namespace: default"));
    assert!(!workload.contains("kind: Namespace\nmetadata:\n  name: default"));
}

#[test]
fn implicit_external_bootstrap_namespace_rejects_authored_namespace() {
    let fixture = fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "  namespace: api\n---",
        "  namespace: api\nspec:\n  additionalNamespaces: [default]\n---",
    ) + r"---
apiVersion: v1
kind: Namespace
metadata:
  name: default
";
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "renders Namespace \"default\", but its configured owner kind is External",
        ));
}

#[test]
fn explicit_owner_can_manage_a_bootstrap_namespace() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  sharedNamespaces:\n    default:\n      owner:\n        kind: Release\n        applicationGroup: workloads\n        release: api\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "  namespace: api\n---",
        "  namespace: api\nspec:\n  additionalNamespaces: [default]\n---",
    );
    fs::write(release_path, release).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let workload = fs::read_to_string(output.join("production/workloads/api/resources.yaml")).unwrap();
    assert!(workload.contains("name: default"));
    assert_eq!(workload.matches("kind: Namespace").count(), 2);
}

#[test]
fn shared_namespace_requires_explicit_policy() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  destinationNamespace: shared\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    let api_path = fixture.path().join("applications/workloads/api.yaml");
    let api = fs::read_to_string(&api_path)
        .unwrap()
        .replace("  namespace: api\ndata:", "  namespace: shared\ndata:");
    fs::write(api_path, api).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/worker.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: worker
  namespace: worker
",
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("consumed by multiple workload Applications"));
}

#[test]
fn non_owner_release_cannot_render_a_shared_namespace() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  destinationNamespace: shared\n  sharedNamespaces:\n    shared:\n      owner:\n        kind: Release\n        applicationGroup: workloads\n        release: api\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    let api_path = fixture.path().join("applications/workloads/api.yaml");
    let api = fs::read_to_string(&api_path)
        .unwrap()
        .replace("  namespace: api\ndata:", "  namespace: shared\ndata:");
    fs::write(api_path, api).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/worker.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: worker
  namespace: worker
---
apiVersion: v1
kind: Namespace
metadata:
  name: shared
",
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "renders shared Namespace \"shared\", but ownership is delegated to another Release",
        ));
}

#[test]
fn external_namespace_cannot_be_rendered_by_a_release() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  applicationNamespace:",
        "  destinationNamespace: kube-system\n  sharedNamespaces:\n    kube-system:\n      owner:\n        kind: External\n  applicationNamespace:",
    );
    fs::write(group_path, group).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/api.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
---
apiVersion: v1
kind: Namespace
metadata:
  name: kube-system
",
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "renders Namespace \"kube-system\", but its configured owner kind is External",
        ));
}

#[test]
fn broad_release_policy_cannot_override_platform_owned_application_fields() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.metadata.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedPaths: ['spec.**']\n{% if target.metadata.labels.environment == 'production' %}",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "metadata:\n  name: api\n  namespace: api\n",
        "metadata:\n  name: api\n  namespace: api\nspec:\n  argocd:\n    applicationOverride:\n      spec:\n        destination:\n          server: https://attacker.invalid\n",
    );
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("platform-owned"));
}

#[test]
fn broad_release_policy_cannot_add_argocd_multi_sources() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.metadata.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedPaths: ['spec.**']\n{% if target.metadata.labels.environment == 'production' %}",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "metadata:\n  name: api\n  namespace: api\n",
        "metadata:\n  name: api\n  namespace: api\nspec:\n  argocd:\n    applicationOverride:\n      spec:\n        sources:\n          - repoURL: https://attacker.invalid/repository.git\n            path: manifests\n",
    );
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("platform-owned"));
}

#[test]
fn release_can_append_explicitly_allowed_sync_options() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.metadata.labels.environment == 'production' %}",
        "  syncPolicy:\n    syncOptions: [ApplyOutOfSyncOnly=true]\n  releaseCustomization:\n    allowedSyncOptions: [ApplyOutOfSyncOnly=true, RespectIgnoreDifferences=false]\n{% if target.metadata.labels.environment == 'production' %}",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "metadata:\n  name: api\n  namespace: api\n",
        "metadata:\n  name: api\n  namespace: api\nspec:\n  argocd:\n    applicationOverride:\n      spec:\n        syncPolicy:\n          +syncOptions:\n            - ApplyOutOfSyncOnly=true\n            - RespectIgnoreDifferences=false\n",
    );
    fs::write(release_path, release).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let application =
        fs::read_to_string(output.join("production/_nyl/catalog/applications/argocd-production/api.yaml")).unwrap();
    assert_eq!(application.matches("- ApplyOutOfSyncOnly=true").count(), 1);
    assert!(application.contains("- RespectIgnoreDifferences=false"));
    assert!(!application.contains("+syncOptions"));
}

#[test]
fn release_can_replace_the_default_sync_option_with_an_allowed_value() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.metadata.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedSyncOptions: [ApplyOutOfSyncOnly=false]\n{% if target.metadata.labels.environment == 'production' %}",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "metadata:\n  name: api\n  namespace: api\n",
        "metadata:\n  name: api\n  namespace: api\nspec:\n  argocd:\n    applicationOverride:\n      spec:\n        syncPolicy:\n          +syncOptions:\n            - ApplyOutOfSyncOnly=false\n",
    );
    fs::write(release_path, release).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let application =
        fs::read_to_string(output.join("production/_nyl/catalog/applications/argocd-production/api.yaml")).unwrap();
    assert_eq!(application.matches("- ApplyOutOfSyncOnly=false").count(), 1);
    assert!(!application.contains("ApplyOutOfSyncOnly=true"));
}

#[test]
fn release_cannot_append_an_unapproved_sync_option() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.metadata.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedSyncOptions: [RespectIgnoreDifferences=true]\n{% if target.metadata.labels.environment == 'production' %}",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "metadata:\n  name: api\n  namespace: api\n",
        "metadata:\n  name: api\n  namespace: api\nspec:\n  argocd:\n    applicationOverride:\n      spec:\n        syncPolicy:\n          +syncOptions:\n            - RespectIgnoreDifferences=false\n",
    );
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "is not allowed to add Argo CD sync option \"RespectIgnoreDifferences=false\"",
        ));
}

#[test]
fn allowed_sync_options_do_not_allow_replacing_group_sync_options() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.metadata.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedSyncOptions: [RespectIgnoreDifferences=false]\n{% if target.metadata.labels.environment == 'production' %}",
    );
    fs::write(group_path, group).unwrap();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "metadata:\n  name: api\n  namespace: api\n",
        "metadata:\n  name: api\n  namespace: api\nspec:\n  argocd:\n    applicationOverride:\n      spec:\n        syncPolicy:\n          syncOptions:\n            - RespectIgnoreDifferences=false\n",
    );
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("platform-owned"));
}

#[test]
fn workload_cannot_own_a_namespace_other_than_its_destination() {
    let fixture = fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap()
        + r"---
apiVersion: v1
kind: Namespace
metadata:
  name: another-namespace
";
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unexpected namespace \"another-namespace\""));
}

#[test]
fn namespace_scope_validation_collects_issues_across_releases() {
    let fixture = fixture();
    let api_path = fixture.path().join("applications/workloads/api.yaml");
    let api = fs::read_to_string(&api_path).unwrap()
        + r"---
apiVersion: v1
kind: ConfigMap
metadata:
  name: misplaced-api-config
  namespace: monitoring
---
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: misplaced-api-monitor
  namespace: monitoring
";
    fs::write(api_path, api).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/coredns.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: coredns
  namespace: argocd
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: coredns
  namespace: kube-system
"#,
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
            "--check",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains(
                "Rendered namespace scope validation found 3 issues across 2 releases",
            )
                .and(predicate::str::contains(
                    "Release \"api\" (2 issues)\n  Allowed namespaces: \"api\"\n  Unexpected namespace \"monitoring\" (2 resources):\n    - ConfigMap \"misplaced-api-config\"\n    - ServiceMonitor \"misplaced-api-monitor\"",
                ))
                .and(predicate::str::contains(
                    "Release \"coredns\" (1 issue)\n  Allowed namespaces: \"argocd\"\n  Unexpected namespace \"kube-system\" (1 resource):\n    - Deployment \"coredns\"",
                )),
        );
}

#[test]
fn additional_namespace_stays_with_its_workload_when_rendered() {
    let fixture = fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "  namespace: api\n---",
        "  namespace: api\nspec:\n  additionalNamespaces: [monitoring]\n---",
    ) + r"---
apiVersion: v1
kind: Namespace
metadata:
  name: monitoring
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: metrics
  namespace: monitoring
";
    fs::write(release_path, release).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let root = output.join("production");
    let workload = fs::read_to_string(root.join("workloads/api/resources.yaml")).unwrap();
    assert!(workload.contains("namespace: monitoring"));
    assert_eq!(workload.matches("kind: Namespace").count(), 2);
    assert!(workload.contains("Prune=confirm"));
    assert!(workload.contains("Delete=confirm"));
    assert!(!root.join("_nyl/namespaces").exists());
}

#[test]
fn additional_namespace_is_synthesized_when_missing() {
    let fixture = fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "  namespace: api\n---",
        "  namespace: api\nspec:\n  additionalNamespaces: [monitoring]\n---",
    );
    fs::write(release_path, release).unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let workload = fs::read_to_string(output.join("production/workloads/api/resources.yaml")).unwrap();
    assert!(workload.contains("name: api"));
    assert!(workload.contains("name: monitoring"));
    assert_eq!(workload.matches("kind: Namespace").count(), 2);
    assert_eq!(workload.matches("Prune=confirm").count(), 2);
    assert_eq!(workload.matches("Delete=confirm").count(), 2);
    assert!(!output.join("production/_nyl/namespaces").exists());
}

#[test]
fn release_include_preserves_explicit_secret_manifest() {
    let fixture = fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "  namespace: api\n---",
        "  namespace: api\nspec:\n  include: [fragments/*.yaml]\n---",
    );
    fs::write(&release_path, release).unwrap();
    fs::create_dir(fixture.path().join("applications/workloads/fragments")).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/fragments/secret.yaml"),
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: included\n  namespace: api\n",
    )
    .unwrap();
    let output = fixture.path().join("deploy");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    let resources = fs::read_to_string(output.join("production/workloads/api/resources.yaml")).unwrap();
    assert!(resources.contains("kind: Secret"));
    assert!(resources.contains("name: included"));
}

#[test]
fn publishes_a_new_publication_branch_with_cas_workflow() {
    let (fixture, destination, _seed, source_commit) = publication_fixture();
    let source_commit_string = source_commit.to_string();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Published deployment target production"))
        .stdout(predicate::str::contains("  Repository: "))
        .stdout(predicate::str::contains("  Branch: deploy/production"))
        .stdout(predicate::str::contains("  Commit: "));

    let destination_repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&destination_repository, "deploy/production");
    let message = commit.message().unwrap();
    assert!(message.starts_with("Render deployment target production\n\n"));
    assert!(message.contains("Nyl-Source-Repository: https://example.invalid/source.git"));
    assert!(message.contains(&format!("Nyl-Source-Commit: {source_commit}")));
    assert!(message.contains("Nyl-Deployment-Target: production"));
    assert!(message.contains("Nyl-Cluster: kasoku"));
    let tree = commit.tree().unwrap();
    assert!(tree
        .get_path(std::path::Path::new("production/workloads/api/resources.yaml"))
        .is_ok());
    assert!(tree
        .get_path(std::path::Path::new(
            "production/_nyl/catalog/applications/argocd-production/api.yaml"
        ))
        .is_ok());
    assert!(tree
        .get_path(std::path::Path::new("production/_nyl/index.json"))
        .is_ok());

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success()
        .stdout(predicate::str::contains("is already published"))
        .stdout(predicate::str::contains("  Branch: deploy/production"))
        .stdout(predicate::str::contains(format!("  Commit: {}", commit.id())));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--against",
            "published",
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Deployment target   production"))
        .stderr(predicate::str::contains(
            "Repository        https://example.invalid/source.git",
        ))
        .stderr(predicate::str::contains(format!("Commit            {source_commit}")))
        .stderr(predicate::str::contains("Working tree      clean"))
        .stderr(predicate::str::contains("Published baseline"))
        .stderr(predicate::str::contains("Revision          deploy/production"))
        .stderr(predicate::str::contains(format!("Commit            {}", commit.id())))
        .stderr(predicate::str::contains("Path              production"))
        .stderr(predicate::str::contains("has no rendered differences"));

    let empty_diff = fixture.path().join("artifacts/no-differences.diff");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--output",
            empty_diff.to_str().unwrap(),
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(format!(
            "Diff output         {}",
            empty_diff.display()
        )))
        .stderr(predicate::str::contains("has no rendered differences"));
    assert_eq!(fs::metadata(&empty_diff).unwrap().len(), 0);

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--against",
            "source",
            "--source-ref",
            &source_commit_string,
            "--source-repository",
            fixture.path().to_str().unwrap(),
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Source baseline"))
        .stderr(predicate::str::contains(format!("Revision          {source_commit}")))
        .stderr(predicate::str::contains(format!("Commit            {source_commit}")))
        .stderr(predicate::str::contains("  Desired publication\n    Cluster"))
        .stderr(predicate::str::contains("  Baseline publication\n    Cluster"));

    let application_source = fixture.path().join("applications/workloads/api.yaml");
    let changed = fs::read_to_string(&application_source)
        .unwrap()
        .replace("environment: '{{ values.environment }}'", "environment: changed");
    fs::write(application_source, changed).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--against",
            "published",
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("+  environment: changed"))
        .stderr(predicate::str::contains("Working tree      dirty"))
        .stderr(predicate::str::contains("1 changed · 0 added · 1 modified · 0 deleted"));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["diff-tree", "--target", "production", "--catalog"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("View                Argo CD catalog"))
        .stderr(predicate::str::contains("has no rendered differences"));

    let application_diff = fixture.path().join("artifacts/api.diff");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--application",
            "argocd-production/api",
            "--output",
            application_diff.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "View                Applications argocd-production/api",
        ))
        .stderr(predicate::str::contains("1 changed · 0 added · 1 modified · 0 deleted"));
    let application_diff_contents = fs::read_to_string(&application_diff).unwrap();
    assert!(application_diff_contents.contains("workloads/api/resources.yaml"));
    assert!(application_diff_contents.contains("+  environment: changed"));
    assert!(!application_diff_contents.contains("_nyl/catalog/projects"));

    let failing_diff = fixture.path().join("artifacts/failing.diff");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--application",
            "argocd-production/api",
            "--output",
            failing_diff.to_str().unwrap(),
            "--fail-on-diff",
        ])
        .assert()
        .failure();
    assert!(fs::read_to_string(&failing_diff)
        .unwrap()
        .contains("+  environment: changed"));

    let preserved_diff = fixture.path().join("artifacts/preserved.diff");
    fs::write(&preserved_diff, "preserve me\n").unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--application",
            "missing/application",
            "--output",
            preserved_diff.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("exists on neither side of the comparison"));
    assert_eq!(fs::read_to_string(preserved_diff).unwrap(), "preserve me\n");

    let project_source = fixture.path().join("config/projects/workloads.yaml");
    let changed_project = fs::read_to_string(&project_source)
        .unwrap()
        .replace("namespace: '*'", "namespace: api");
    fs::write(project_source, changed_project).unwrap();
    let catalog_diff = fixture.path().join("artifacts/catalog.diff");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "diff-tree",
            "--target",
            "production",
            "--catalog",
            "--output",
            catalog_diff.to_str().unwrap(),
        ])
        .assert()
        .success();
    let catalog_diff_contents = fs::read_to_string(catalog_diff).unwrap();
    assert!(catalog_diff_contents.contains("_nyl/catalog/projects/workloads.yaml"));
    assert!(!catalog_diff_contents.contains("workloads/api/resources.yaml"));
}

#[test]
fn publish_tree_default_accepts_irrelevant_worktree_changes() {
    let (fixture, destination, _seed, source_commit) = publication_fixture();
    fs::write(fixture.path().join("untracked-publication-note.txt"), "local note\n").unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Published deployment target production"));

    let destination = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&destination, "deploy/production");
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&destination, &commit, "production/_nyl/index.json")).unwrap();
    assert_eq!(index["sourceCommit"], source_commit.to_string());
    assert_eq!(index["dirty"], false);
}

#[test]
fn publish_tree_default_rejects_changes_that_affect_the_rendered_target() {
    let (fixture, destination, _seed, _source_commit) = publication_fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "environment: '{{ values.environment }}'",
        "environment: locally-modified",
    );
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Local changes affect deployment target \"production\"",
        ))
        .stderr(predicate::str::contains("modified workloads/api/resources.yaml"))
        .stderr(predicate::str::contains("--allow-dirty"));

    let destination = Repository::open_bare(destination.path()).unwrap();
    assert!(destination.find_reference("refs/heads/deploy/production").is_err());
}

#[test]
fn publish_tree_allow_dirty_records_nonreproducible_provenance() {
    let (fixture, destination, _seed, source_commit) = publication_fixture();
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path).unwrap().replace(
        "environment: '{{ values.environment }}'",
        "environment: locally-modified",
    );
    fs::write(release_path, release).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production", "--allow-dirty"])
        .assert()
        .success();

    let destination = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&destination, "deploy/production");
    assert!(commit.message().unwrap().contains("Nyl-Source-Dirty: true"));
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&destination, &commit, "production/_nyl/index.json")).unwrap();
    assert_eq!(index["sourceCommit"], source_commit.to_string());
    assert_eq!(index["dirty"], true);
    let resources = String::from_utf8(published_file(
        &destination,
        &commit,
        "production/workloads/api/resources.yaml",
    ))
    .unwrap();
    assert!(resources.contains("environment: locally-modified"));
}

#[test]
fn publish_tree_require_clean_rejects_irrelevant_worktree_changes() {
    let (fixture, _destination, _seed, _source_commit) = publication_fixture();
    fs::write(fixture.path().join("untracked-publication-note.txt"), "local note\n").unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production", "--require-clean"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "publish-tree --require-clean requires a clean source worktree",
        ));
}

#[test]
fn publish_tree_cleanliness_overrides_are_mutually_exclusive() {
    Command::cargo_bin("nyl")
        .unwrap()
        .args(["publish-tree", "--allow-dirty", "--require-clean"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "the argument '--allow-dirty' cannot be used with '--require-clean'",
        ));
}

#[test]
fn diff_tree_normalization_controls_patch_reports_and_exit_status() {
    let (fixture, destination, _seed, _) = publication_fixture();
    let source_path = fixture.path().join("applications/workloads/api.yaml");
    let source = fs::read_to_string(&source_path).unwrap();
    fs::write(&source_path, format!("{source}  config: |\n    first\n    second\n")).unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Multiline configuration");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success();

    let checkout = TempDir::new().unwrap();
    let repository = git2::build::RepoBuilder::new()
        .branch("deploy/production")
        .remote_create(|repository, name, url| {
            // Published file hashes and raw diffs require the committed bytes on every platform.
            repository.config()?.set_bool("core.autocrlf", false)?;
            repository.remote(name, url)
        })
        .clone(destination.path().to_str().unwrap(), checkout.path())
        .unwrap();
    let root = checkout.path().join("production");
    let path = root.join("workloads/api/resources.yaml");
    let rendered = fs::read_to_string(&path).unwrap();
    assert!(rendered.contains("config: |\n    first\n    second\n"), "{rendered}");
    let documents = nyl::yaml::parse_yaml_documents_k8s_compatible(&rendered).unwrap();
    let quoted = documents
        .iter()
        .map(|document| nyl::yaml::serialize_yaml_document(document).unwrap())
        .collect::<Vec<_>>()
        .join("---\n");
    fs::write(path, &quoted).unwrap();
    let index_path = root.join("_nyl/index.json");
    let mut index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    index["files"]["workloads/api/resources.yaml"] = nyl_core::digest::sha256_hex(quoted.as_bytes()).into();
    fs::write(index_path, serde_json::to_vec_pretty(&index).unwrap()).unwrap();
    commit_all(&repository, "Quoted publication configuration");
    repository
        .find_remote("origin")
        .unwrap()
        .push(&["refs/heads/deploy/production:refs/heads/deploy/production"], None)
        .unwrap();

    for raw in [false, true] {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(60))
            .args([
                "diff-tree",
                "--target",
                "production",
                "--fail-on-diff",
                "--progress",
                "off",
                "--output",
                "comparison.diff",
                "--stats-output",
                "json:comparison.json",
            ]);
        if raw {
            command.arg("--raw");
            command.assert().failure();
        } else {
            command.assert().success();
        }
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.path().join("comparison.json")).unwrap()).unwrap();
        assert_eq!(report["comparison"]["mode"], if raw { "raw" } else { "normalized" });
        assert_eq!(report["diff"]["has_changes"], raw);
        assert_eq!(report["diff"]["files_changed"], usize::from(raw));
        assert_eq!(
            !fs::read(fixture.path().join("comparison.diff")).unwrap().is_empty(),
            raw
        );
    }
}

#[test]
fn diff_tree_exports_complete_reports_and_controls_stderr_independently() {
    let (fixture, _destination, _seed, source_commit) = publication_fixture();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success();
    let application = fixture.path().join("applications/workloads/api.yaml");
    let changed = fs::read_to_string(&application)
        .unwrap()
        .replace("environment: '{{ values.environment }}'", "environment: changed");
    fs::write(application, changed).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .env("CI", "true")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("TERM", "xterm")
        .args([
            "diff-tree",
            "--target",
            "production",
            "--progress",
            "off",
            "--stats-files",
            "--output",
            "artifacts/rendered.diff",
            "--stats-output",
            "text:artifacts/report.txt",
            "--stats-output",
            "markdown:artifacts/comment.md",
            "--stats-output",
            "json:artifacts/report.json",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("\x1b[32m+1\x1b[0m"))
        .stderr(predicate::str::contains("Render statistics"));
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.path().join("artifacts/report.json")).unwrap()).unwrap();
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["comparison"]["target"], "production");
    assert_eq!(
        json["comparison"]["desired_source"]["commit"],
        source_commit.to_string()
    );
    assert_eq!(json["comparison"]["desired_source"]["dirty"], true);
    assert_eq!(json["comparison"]["baseline"]["mode"], "published");
    assert_eq!(json["diff"]["files_changed"], 1);
    assert_eq!(json["diff"]["lines_added"], 1);
    assert_eq!(json["diff"]["lines_removed"], 1);
    assert_eq!(json["diff"]["files"][0]["path"], "workloads/api/resources.yaml");
    assert_eq!(json["render"]["sources"]["git_ref_refresh"], 1);
    for path in ["artifacts/report.txt", "artifacts/comment.md", "artifacts/report.json"] {
        let contents = fs::read_to_string(fixture.path().join(path)).unwrap();
        assert!(!contents.contains('\x1b'), "{path} must be ANSI-free in auto mode");
    }
    let markdown = fs::read_to_string(fixture.path().join("artifacts/comment.md")).unwrap();
    assert!(markdown.contains("| workloads/api/resources\\.yaml | modified | +1 | −1 |"));
    assert!(markdown.contains("<summary>Render statistics</summary>\n\n```text\n"));
    #[cfg(unix)]
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--stats-output",
            "markdown:-",
            "--output",
            "/dev/null",
            "--progress",
            "off",
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "## Nyl deployment check · production — 1 file changed\n",
        ))
        .stdout(predicate::str::contains("**1 file changed · +1 −1 lines**"))
        .stderr(predicate::str::contains("Rendered tree comparison").not());
    assert!(fs::read_to_string(fixture.path().join("artifacts/rendered.diff"))
        .unwrap()
        .contains("+  environment: changed"));

    let result = Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--target",
            "production",
            "--progress",
            "off",
            "--no-stats-stderr",
            "--output",
            "artifacts/rendered.diff",
            "--stats-output",
            "json:-",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("Rendered tree comparison").not())
        .stderr(predicate::str::contains("Render statistics").not())
        .get_output()
        .stdout
        .clone();
    let copied: serde_json::Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(copied["diff"], json["diff"]);

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--target",
            "production",
            "--progress",
            "off",
            "--no-stats-stderr",
            "--color",
            "always",
            "--output",
            "artifacts/failure.diff",
            "--fail-on-diff",
            "--stats-output",
            "text:artifacts/colored.txt",
            "--stats-output",
            "json:artifacts/failure.json",
            "--stats-output",
            "markdown:artifacts/failure.md",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("has rendered differences"));
    assert!(fs::read_to_string(fixture.path().join("artifacts/colored.txt"))
        .unwrap()
        .contains("\x1b[32m+1\x1b[0m"));
    let failed: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.path().join("artifacts/failure.json")).unwrap()).unwrap();
    assert_eq!(failed["diff"], json["diff"]);
    assert!(!fs::read_to_string(fixture.path().join("artifacts/failure.md"))
        .unwrap()
        .contains('\x1b'));
    assert!(fs::read_to_string(fixture.path().join("artifacts/failure.diff"))
        .unwrap()
        .contains("+  environment: changed"));

    for selection in [&["--catalog"][..], &["--application", "argocd-production/api"][..]] {
        let output = Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(60))
            .args([
                "diff-tree",
                "--progress",
                "off",
                "--no-stats-stderr",
                "--output",
                "artifacts/selected.diff",
                "--stats-output",
                "json:-",
            ])
            .args(selection)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let selected: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(selected["diff"]["has_changes"], selection[0] == "--application");
        if selection[0] == "--catalog" {
            assert_eq!(selected["diff"]["files"], serde_json::json!([]));
            assert_eq!(selected["diff"]["lines_added"], 0);
            assert!(fs::read(fixture.path().join("artifacts/selected.diff"))
                .unwrap()
                .is_empty());
        } else {
            assert_eq!(selected["diff"], json["diff"]);
        }
    }

    let source_output = Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--against",
            "source",
            "--source-ref",
            &source_commit.to_string(),
            "--source-repository",
            fixture.path().to_str().unwrap(),
            "--progress",
            "off",
            "--no-stats-stderr",
            "--output",
            "artifacts/source.diff",
            "--stats-output",
            "json:-",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let source: serde_json::Value = serde_json::from_slice(&source_output).unwrap();
    assert_eq!(source["comparison"]["baseline"]["mode"], "source");
    assert_eq!(source["comparison"]["baseline"]["commit"], source_commit.to_string());
    assert_eq!(source["diff"], json["diff"]);
}

#[test]
fn diff_tree_rejects_output_conflicts_before_rendering() {
    let temp = TempDir::new().unwrap();
    for args in [
        vec!["--stats-output", "json:-"],
        vec!["--output", "report", "--stats-output", "text:./report"],
        vec!["--stats-output", "json:report", "--stats-output", "markdown:./report"],
        vec!["--stats-output", "yaml:report"],
    ] {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(temp.path())
            .timeout(std::time::Duration::from_secs(10))
            .arg("diff-tree")
            .args(args)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty());
    }
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn diff_tree_failed_comparisons_export_current_failure_and_protect_output_aliases() {
    let (fixture, _destination, _seed, source_commit) = publication_fixture();
    fs::write(fixture.path().join("report.json"), "preserve me").unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--against",
            "source",
            "--source-ref",
            &source_commit.to_string(),
            "--source-repository",
            fixture.path().to_str().unwrap(),
            "--application",
            "missing/app",
            "--stats-output",
            "json:report.json",
        ])
        .assert()
        .failure();
    let contents = fs::read_to_string(fixture.path().join("report.json")).unwrap();
    let report: serde_json::Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(report["stages"]["comparison"], "failed");
    assert_eq!(report["diff"], serde_json::Value::Null);

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--against",
            "source",
            "--source-ref",
            &source_commit.to_string(),
            "--source-repository",
            fixture.path().to_str().unwrap(),
            "--no-stats-stderr",
            "--progress",
            "off",
            "--output",
            "artifacts/rendered.diff",
            "--stats-output",
            "json:report.json/impossible",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());
    assert_eq!(
        fs::read_to_string(fixture.path().join("report.json")).unwrap(),
        contents
    );
}

#[test]
fn validation_json_stdout_is_complete_and_preserves_cached_provenance() {
    let fixture = fixture();
    configure_validation(fixture.path(), "string");
    let output = TempDir::new().unwrap();
    let mut previous = None;
    for check in [false, true] {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["render-tree", "--output-dir"])
            .arg(output.path())
            .args(["--validation-output", "json:-", "--no-validation-stderr"]);
        if check {
            command.arg("--check");
        }
        let result = command.assert().success();
        let report: serde_json::Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
        assert_eq!(report["version"], 1);
        assert_eq!(report["complete"], true);
        assert_eq!(report["status"], "valid");
        let resources = report["resources"].as_array().unwrap();
        assert_eq!(report["summary"]["valid"].as_u64().unwrap() as usize, resources.len());
        let configmap = resources.iter().find(|r| r["resource"]["kind"] == "ConfigMap").unwrap();
        assert_eq!(configmap["provenance"][0]["type"], "source");
        assert_eq!(configmap["provenance"][0]["path"], "applications/workloads/api.yaml");
        assert_eq!(configmap["provenance"][0]["document"], 2);
        assert!(resources.iter().any(|r| r["provenance"][0]["type"] == "generated"));
        if let Some(previous) = previous {
            assert_eq!(report, previous);
        }
        previous = Some(report);
    }
}

#[test]
fn validation_remote_provenance_survives_artifact_and_tree_cache_hits() {
    let fixture = fixture();
    configure_validation(fixture.path(), "string");
    let remote = TempDir::new().unwrap();
    let repository = Repository::init(remote.path()).unwrap();
    fs::create_dir(remote.path().join("releases")).unwrap();
    fs::copy(
        fixture.path().join("applications/workloads/api.yaml"),
        remote.path().join("releases/api.yaml"),
    )
    .unwrap();
    commit_all(&repository, "Remote release");
    let commit = repository.head().unwrap().target().unwrap().to_string();
    let url = reqwest::Url::from_directory_path(remote.path()).unwrap().to_string();
    let group = fixture.path().join("config/application-groups/workloads.yaml");
    fs::write(
        &group,
        format!(
            "{}\n  source:\n    repository: {{repoURL: '{url}'}}\n    revision: HEAD\n    commit: '{commit}'\n    path: releases\n",
            fs::read_to_string(&group).unwrap()
        ),
    )
    .unwrap();
    let output = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let mut previous = None;
    for _ in 0..2 {
        let result = Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .env("NYL_CACHE_DIR", cache.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["render-tree", "--output-dir"])
            .arg(output.path())
            .args(["--validation-output", "json:-", "--no-validation-stderr"])
            .assert()
            .success();
        let report: serde_json::Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
        let resources = report["resources"].as_array().unwrap();
        for kind in ["ConfigMap", "Namespace"] {
            let resource = resources.iter().find(|r| r["resource"]["kind"] == kind).unwrap();
            let frames = resource["provenance"].as_array().unwrap();
            assert_eq!(frames[0]["repository"], url);
            assert_eq!(frames[0]["revision"], commit);
            assert_eq!(frames[1]["path"], "releases/api.yaml");
            if kind == "Namespace" {
                assert!(frames.iter().any(|frame| frame["type"] == "generated"));
            }
        }
        if let Some(previous) = previous {
            assert_eq!(report, previous);
        }
        previous = Some(report);
    }
}

#[test]
fn validation_reports_export_findings_and_skipped_resources_without_losing_render_output() {
    let fixture = fixture();
    configure_validation(fixture.path(), "integer");
    let config = fixture.path().join("nyl.toml");
    fs::write(
        &config,
        format!("{}\nskip=['v1/Namespace']\n", fs::read_to_string(&config).unwrap()),
    )
    .unwrap();
    let output = TempDir::new().unwrap();
    let reports = TempDir::new().unwrap();
    let json = reports.path().join("validation.json");
    let text = reports.path().join("validation.txt");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("CI", "true")
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR")
        .timeout(std::time::Duration::from_secs(30))
        .args(["render-tree", "--output-dir"])
        .arg(output.path())
        .arg("--validation-output")
        .arg(format!("json:{}", json.display()))
        .arg("--validation-output")
        .arg(format!("text:{}", text.display()))
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "\x1b[1;36mkubeconform · kasoku · Kubernetes 1.31.4\x1b[0m",
        ))
        .stderr(predicate::str::contains("\x1b[1;31mFAIL\x1b[0m"))
        .stderr(predicate::str::contains(
            "\x1b[1m/data/environment\x1b[0m: got string, want integer",
        ));
    let report: serde_json::Value = serde_json::from_slice(&fs::read(&json).unwrap()).unwrap();
    assert_eq!(report["status"], "invalid");
    assert_eq!(report["complete"], true);
    assert_eq!(report["summary"]["invalid"], 1);
    assert!(report["summary"]["skipped"].as_u64().unwrap() > 0);
    let resources = report["resources"].as_array().unwrap();
    let invalid = resources.iter().find(|r| r["status"] == "invalid").unwrap();
    assert_eq!(invalid["findings"][0]["path"], "/data/environment");
    assert_eq!(invalid["schemaOrigin"]["type"], "local");
    assert_eq!(invalid["renderedLocation"]["path"], "workloads/api/resources.yaml");
    assert!(output.path().join("production/workloads/api/resources.yaml").is_file());
    let exported = fs::read_to_string(text).unwrap();
    assert!(exported.contains("Source:        applications/workloads/api.yaml"));
    assert!(!exported.contains('\x1b'));
    assert!(!report.to_string().contains("\\u001b"));
}

#[test]
fn validation_exports_reject_source_output_and_stdout_collisions() {
    let fixture = fixture();
    configure_validation(fixture.path(), "string");
    let output = TempDir::new().unwrap();
    let source = fixture.path().join("applications/workloads/api.yaml");
    let original = fs::read(&source).unwrap();
    let schema = fixture.path().join("schemas/v1/configmap_v1.json");
    let original_schema = fs::read(&schema).unwrap();
    for destination in [source.clone(), schema.clone(), output.path().join("validation.json")] {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["render-tree", "--output-dir"])
            .arg(output.path())
            .arg("--validation-output")
            .arg(format!("json:{}", destination.display()))
            .assert()
            .failure()
            .stderr(predicate::str::contains("overwrite source or managed output"));
    }
    assert_eq!(fs::read(source).unwrap(), original);
    assert_eq!(fs::read(schema).unwrap(), original_schema);
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args([
            "render",
            "applications/workloads/api.yaml",
            "--offline",
            "--target",
            "production",
            "--validation-output",
            "json:-",
        ])
        .assert()
        .failure()
        .stdout("");
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["render-tree", "--output-dir"])
        .arg(output.path())
        .args(["--validation-output", "json:-", "--validation-output", "text:-"])
        .assert()
        .failure()
        .stdout("");
}

#[test]
fn diff_tree_exports_validation_failures_and_independent_comparison_results() {
    let (fixture, _destination, _seed, _) = publication_fixture();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["publish-tree"])
        .assert()
        .success();
    configure_validation(fixture.path(), "string");
    let schemas = fixture.path().join("schemas/postgresql.cnpg.io");
    fs::create_dir_all(&schemas).unwrap();
    fs::write(
        schemas.join("cluster_v1.json"),
        r#"{"type":"object","properties":{"spec":{"type":"object","properties":{"affinity":{"type":"object"}}}}}"#,
    )
    .unwrap();
    for (namespace, name) in [("rise", "rise-db"), ("rise-dash", "dash-db")] {
        fs::write(fixture.path().join(format!("applications/workloads/{namespace}.yaml")), format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: Release\nmetadata: {{name: {namespace}, namespace: {namespace}}}\n---\napiVersion: postgresql.cnpg.io/v1\nkind: Cluster\nmetadata: {{name: {name}, namespace: {namespace}}}\nspec:\n  affinity: null\n"
        )).unwrap();
    }
    let source = Repository::open(fixture.path()).unwrap();
    commit_all(&source, "Resources requiring affinity objects");
    let artifacts = TempDir::new().unwrap();
    for (case, extra) in [
        ("published", Vec::<&str>::new()),
        (
            "unchanged",
            vec![
                "--against",
                "source",
                "--source-ref",
                "HEAD",
                "--source-repository",
                fixture.path().to_str().unwrap(),
            ],
        ),
        ("catalog", vec!["--catalog"]),
        (
            "missing-baseline",
            vec![
                "--against",
                "source",
                "--source-ref",
                "missing-ref",
                "--source-repository",
                fixture.path().to_str().unwrap(),
            ],
        ),
    ] {
        let patch = artifacts.path().join(format!("{case}.diff"));
        let json = artifacts.path().join(format!("{case}.json"));
        let markdown = artifacts.path().join(format!("{case}.md"));
        let separate = artifacts.path().join(format!("{case}-validation.json"));
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(60))
            .args([
                "diff-tree",
                "--progress",
                "off",
                "--stats-files",
                "--stats-patch",
                "--no-validation-stderr",
                "--no-stats-stderr",
            ])
            .args(extra)
            .arg("--output")
            .arg(&patch)
            .args(["--stats-output", &format!("markdown:{}", markdown.display())])
            .args(["--stats-output", &format!("json:{}", json.display())])
            .args(["--validation-output", &format!("json:{}", separate.display())])
            .assert()
            .failure();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(json).unwrap()).unwrap();
        let validation: serde_json::Value = serde_json::from_slice(&fs::read(separate).unwrap()).unwrap();
        assert_eq!(report["validation"]["report"], validation);
        assert_eq!(report["validation"]["status"], "invalid", "{report}");
        assert_eq!(validation["complete"], true);
        assert_eq!(validation["summary"]["invalid"], 2);
        let body = fs::read_to_string(markdown).unwrap();
        for resource in [
            "Cluster <code>rise/rise-db</code>",
            "Cluster <code>rise-dash/dash-db</code>",
            "/spec/affinity",
            "got null, want object",
            "Source: <code>applications/workloads/",
            "Rendered    workloads/",
        ] {
            assert!(body.contains(resource), "{case}: missing {resource}\n{body}");
        }
        if case == "missing-baseline" {
            assert_eq!(report["stages"]["comparison"], "failed");
            assert_eq!(report["diff"], serde_json::Value::Null);
            assert!(!patch.exists());
            assert!(body.contains("Diff unavailable"));
        } else {
            assert_eq!(report["stages"]["comparison"], "completed");
            assert_eq!(report["diff"]["has_changes"], case != "unchanged");
            assert_eq!(fs::read(patch).unwrap().is_empty(), case == "unchanged");
        }
        if case == "catalog" {
            assert!(body.contains("Diff scope: Argo CD catalog"));
            assert!(body.contains("Validation scope: complete desired target"));
        }
    }

    for (format, suppress_stderr) in [
        (None, false),
        (None, true),
        (Some("text"), false),
        (Some("markdown"), false),
        (Some("json"), false),
    ] {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(60))
            .args(["diff-tree", "--color", "never", "--progress", "off", "--output"])
            .arg(artifacts.path().join("terminal.diff"));
        if let Some(format) = format {
            command.args(["--stats-output", &format!("{format}:-")]);
        }
        if suppress_stderr {
            command.arg("--no-stats-stderr");
        }
        let result = command.assert().failure();
        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        let terminal_reports = usize::from(format.is_none() && !suppress_stderr);
        assert_eq!(stderr.matches("Rendered tree comparison").count(), terminal_reports);
        for identity in ["Cluster rise/rise-db", "Cluster rise-dash/dash-db"] {
            assert_eq!(
                stderr.matches(&format!("FAIL  {identity}")).count(),
                terminal_reports,
                "{stderr}"
            );
        }
        assert!(stderr.contains("kubeconform validating"), "{stderr}");
        assert!(stderr.contains("Validation failed"), "{stderr}");
        let stdout = String::from_utf8_lossy(&result.get_output().stdout);
        match format {
            Some("json") => {
                let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(report["validation"]["report"]["summary"]["invalid"], 2);
            }
            Some("markdown") => {
                assert!(stdout.starts_with("## Nyl deployment check"));
                assert!(stdout.contains("Cluster <code>rise/rise-db</code>"));
            }
            Some("text") => {
                assert!(stdout.starts_with("Rendered tree comparison"));
                assert_eq!(stdout.matches("FAIL  Cluster rise/rise-db").count(), 1);
            }
            None => assert!(stdout.is_empty()),
            _ => unreachable!(),
        }
    }
}

#[test]
fn diff_tree_reports_discovery_render_and_validation_operation_failures() {
    for stage in ["discovery", "render", "validation"] {
        let (fixture, _destination, _seed, _) = publication_fixture();
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["publish-tree"])
            .assert()
            .success();
        match stage {
            "discovery" => fs::write(fixture.path().join("nyl.toml"), "invalid = [").unwrap(),
            "render" => {
                let path = fixture.path().join("applications/workloads/api.yaml");
                let template = fs::read_to_string(&path)
                    .unwrap()
                    .replace("{{ values.environment }}", "{{ values.environment | missing_filter }}");
                fs::write(path, template).unwrap();
            }
            "validation" => {
                configure_validation(fixture.path(), "string");
                fs::remove_file(fixture.path().join("schemas/v1/configmap_v1.json")).unwrap();
            }
            _ => unreachable!(),
        }
        let artifacts = TempDir::new().unwrap();
        let patch = artifacts.path().join("rendered.diff");
        let json = artifacts.path().join("report.json");
        let markdown = artifacts.path().join("comment.md");
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(60))
            .args(["diff-tree", "--progress", "off", "--output"])
            .arg(&patch)
            .args(["--stats-output", &format!("json:{}", json.display())])
            .args(["--stats-output", &format!("markdown:{}", markdown.display())])
            .assert()
            .failure();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(json).unwrap()).unwrap();
        let body = fs::read_to_string(markdown).unwrap();
        if stage == "validation" {
            assert_eq!(report["validation"]["status"], "error");
            assert_eq!(report["validation"]["report"]["complete"], false);
            assert!(
                report["validation"]["report"]["summary"]["notChecked"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_eq!(report["stages"]["comparison"], "completed");
            assert!(patch.exists());
        } else {
            assert_eq!(report["stages"][stage], "failed");
            assert_eq!(report["validation"]["status"], "not_run");
            assert_eq!(report["stages"]["comparison"], "not_run");
            assert_eq!(report["diff"], serde_json::Value::Null);
            assert!(!patch.exists());
            assert!(body.contains("Diff unavailable"));
        }
    }
}

#[test]
fn diff_tree_keeps_exporting_after_a_report_write_fails() {
    let (fixture, _destination, _seed, _) = publication_fixture();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["publish-tree"])
        .assert()
        .success();
    configure_validation(fixture.path(), "string");
    let artifacts = TempDir::new().unwrap();
    for separate in [false, true] {
        let directory = artifacts.path().join(if separate { "validation" } else { "combined" });
        fs::create_dir(&directory).unwrap();
        let blocking_file = directory.join("parent");
        let blocked_output = blocking_file.join("child.json");
        let json = directory.join("complete.json");
        let option = if separate {
            "--validation-output"
        } else {
            "--stats-output"
        };
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["diff-tree", "--output"])
            .arg(directory.join("rendered.diff"))
            .args([option, &format!("text:{}", blocking_file.display())])
            .args([option, &format!("json:{}", blocked_output.display())])
            .args(["--stats-output", &format!("json:{}", json.display())])
            .assert()
            .failure();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(json).unwrap()).unwrap();
        assert_eq!(report["stages"]["comparison"], "completed");
        assert_eq!(report["validation"]["report"]["status"], "valid", "{report}");
        assert!(blocking_file.is_file());
    }
}

#[test]
fn desired_crds_default_to_tree_validation_and_support_opt_out() {
    let fixture = fixture();
    configure_validation(fixture.path(), "string");
    for (group, file, schema) in [
        (
            "apiextensions.k8s.io",
            "customresourcedefinition_v1.json",
            serde_json::json!({"type":"object"}),
        ),
        ("example.com", "widget_v1.json", serde_json::json!({"type":"object"})),
    ] {
        fs::create_dir_all(fixture.path().join("schemas").join(group)).unwrap();
        fs::write(
            fixture.path().join("schemas").join(group).join(file),
            schema.to_string(),
        )
        .unwrap();
    }
    let path = fixture.path().join("applications/workloads/api.yaml");
    let manifests = format!(
        "{}\n---\n{}\n---\n{}\n",
        fs::read_to_string(&path).unwrap(),
        serde_json::json!({"apiVersion":"apiextensions.k8s.io/v1","kind":"CustomResourceDefinition","metadata":{"name":"widgets.example.com"},"spec":{"group":"example.com","scope":"Namespaced","names":{"kind":"Widget","plural":"widgets"},"versions":[{"name":"v1","served":true,"storage":true,"schema":{"openAPIV3Schema":{"type":"object","properties":{"spec":{"type":"object","properties":{"count":{"type":"integer"}}}}}}}]}}),
        serde_json::json!({"apiVersion":"example.com/v1","kind":"Widget","metadata":{"name":"example","namespace":"api"},"spec":{"count":"invalid"}})
    );
    fs::write(&path, manifests).unwrap();
    let output = TempDir::new().unwrap();
    for flags in [vec![], vec!["--no-use-desired-crds"], vec!["--no-validate"]] {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args(["render-tree", "--target", "production", "--output-dir"])
            .arg(output.path())
            .args(&flags);
        if flags.is_empty() {
            command.assert().failure().stderr(predicate::str::contains("1 invalid"));
        } else {
            command.assert().success();
        }
    }
    for explicit in [false, true] {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .args([
                "render",
                "applications/workloads/api.yaml",
                "--offline",
                "--target",
                "production",
            ]);
        if explicit {
            command
                .arg("--use-desired-crds")
                .assert()
                .failure()
                .stderr(predicate::str::contains("1 invalid"));
        } else {
            command.assert().success();
        }
    }
}

#[test]
fn vendor_all_preserves_targeted_collections_and_prunes_unselected_versions() {
    use sha2::Digest as _;
    let fixture = fixture();
    configure_validation(fixture.path(), "string");
    let config_path = fixture.path().join("nyl.toml");
    let config = format!(
        "{}\n[vendor]\nmode='required'\n",
        fs::read_to_string(&config_path)
            .unwrap()
            .replace("vendor-used", "vendor-all")
    );
    fs::write(&config_path, &config).unwrap();
    let root = fixture.path().join("vendor/schemas");
    fs::create_dir_all(root.join("blobs")).unwrap();
    let write_blob = |value: &serde_json::Value| {
        let bytes = serde_json::to_vec(value).unwrap();
        let hash = hex::encode(sha2::Sha256::digest(&bytes));
        fs::write(root.join("blobs").join(format!("{hash}.json")), bytes).unwrap();
        hash
    };
    let mut schemas = BTreeMap::new();
    let mut collections = BTreeMap::new();
    let revision = "07b64c5376535fbbd6fb9910621e1a41f7613c14";
    for version in ["1.30.0", "1.31.4"] {
        let directory = format!(
            "https://raw.githubusercontent.com/yannh/kubernetes-json-schema/{revision}/v{version}-standalone-strict/"
        );
        for file in ["configmap-v1.json", "secret-v1.json"] {
            schemas.insert(
                format!("{directory}{file}"),
                write_blob(&serde_json::json!({"type":"object","description":format!("{version}/{file}")})),
            );
        }
        collections.insert(
            directory.clone(),
            write_blob(&serde_json::json!({"directory":directory,"files":["configmap-v1.json","secret-v1.json"]})),
        );
    }
    let index_path = root.join("builtins.json");
    fs::write(
        &index_path,
        serde_json::to_vec(&serde_json::json!({"version":1,"schemas":schemas,"collections":collections})).unwrap(),
    )
    .unwrap();
    let vendor = |arguments: &[&str]| {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .timeout(std::time::Duration::from_secs(30))
            .arg("vendor")
            .args(arguments);
        command
    };
    vendor(&["--target", "production"]).assert().success();
    let index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["collections"].as_object().unwrap().len(), 2);
    vendor(&["--check"]).assert().success();
    vendor(&["--prune"]).assert().success();
    let index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["collections"].as_object().unwrap().len(), 1);
    assert_eq!(index["schemas"].as_object().unwrap().len(), 2);
    let secret = format!("https://raw.githubusercontent.com/yannh/kubernetes-json-schema/{revision}/v1.31.4-standalone-strict/secret-v1.json");
    let hash = index["schemas"][&secret].as_str().unwrap();
    fs::remove_file(root.join("blobs").join(format!("{hash}.json"))).unwrap();
    vendor(&["--check"]).assert().failure();
    let mut index = index;
    let invalid = write_blob(&serde_json::json!({"$ref":"https://example.invalid/schema.json"}));
    index["schemas"][&secret] = serde_json::json!(invalid);
    let before = serde_json::to_vec(&index).unwrap();
    fs::write(&index_path, &before).unwrap();
    vendor(&["--target", "production"]).assert().failure();
    assert_eq!(fs::read(&index_path).unwrap(), before);
}

#[test]
fn created_release_is_rendered_by_the_group_that_owns_its_directory() {
    let fixture = fixture();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "create",
            "release",
            "web",
            "--group",
            "workloads",
            "--additional-namespaces",
            "web-jobs,web-cache",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("applications/workloads/web.yaml"));

    let release = fs::read_to_string(fixture.path().join("applications/workloads/web.yaml")).unwrap();
    assert!(release.contains("namespace: web"));
    assert!(release.contains("- web-jobs"));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            ".",
            "--output-dir",
            "deploy-worktree",
            "--color",
            "never",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Release workloads/web (applications/workloads/web.yaml)",
        ));

    let root = fixture.path().join("deploy-worktree/production");
    assert!(root
        .join("_nyl/catalog/applications/argocd-production/web.yaml")
        .is_file());
}

#[test]
fn group_without_a_declared_project_generates_a_permissive_app_project() {
    let fixture = fixture();
    // The group declares neither projectRef nor projectTemplate.
    fs::remove_file(fixture.path().join("config/projects/workloads.yaml")).unwrap();
    fs::write(
        fixture.path().join("config/application-groups/workloads.yaml"),
        r"apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: workloads
  labels:
    environment: production
spec:
  applicationNamespace: argocd
",
    )
    .unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            ".",
            "--output-dir",
            "deploy-worktree",
            "--color",
            "never",
        ])
        .assert()
        .success();

    let root = fixture.path().join("deploy-worktree/production");
    let project = nyl::yaml::parse_yaml_value_k8s_compatible(
        &fs::read_to_string(root.join("_nyl/catalog/projects/workloads.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(project["metadata"]["name"], "workloads");
    assert_eq!(project["metadata"]["namespace"], "argocd");
    assert_eq!(
        project["spec"]["destinations"],
        serde_json::json!([{"namespace": "*", "server": "https://kubernetes.default.svc"}])
    );
    assert_eq!(
        project["spec"]["clusterResourceWhitelist"],
        serde_json::json!([{"group": "*", "kind": "*"}])
    );
    // The in-cluster destination and the publication repository stay fixed.
    assert_eq!(
        project["spec"]["sourceRepos"],
        serde_json::json!(["https://example.invalid/deploy.git"])
    );

    let application = fs::read_to_string(root.join("_nyl/catalog/applications/argocd/api.yaml")).unwrap();
    assert!(application.contains("project: workloads"));
}

/// Where a nested project keeps its Release files.
enum NestedReleases {
    /// `nyl/applications/<group>`, the group's default source.
    Subdirectory,
    /// `applications/<group>` beside `nyl/`, named by this `spec.source.path`.
    Sibling(&'static str),
}

/// Move the fixture's project into `nyl/`, so `nyl.toml` sits beside the
/// configuration, and place the Releases as `releases` says.
fn nested_fixture(releases: NestedReleases) -> TempDir {
    let fixture = fixture();
    let root = fixture.path();
    fs::create_dir(root.join("nyl")).unwrap();
    fs::rename(root.join("nyl.toml"), root.join("nyl/nyl.toml")).unwrap();
    fs::rename(root.join("config"), root.join("nyl/config")).unwrap();
    match releases {
        NestedReleases::Subdirectory => fs::rename(root.join("applications"), root.join("nyl/applications")).unwrap(),
        NestedReleases::Sibling(path) => {
            let group = root.join("nyl/config/application-groups/workloads.yaml");
            let contents = fs::read_to_string(&group).unwrap().replace(
                "  projectRef: workloads\n",
                &format!("  projectRef: workloads\n  source:\n    path: {path}\n"),
            );
            fs::write(group, contents).unwrap();
        }
    }
    fixture
}

/// Render the fixture's target from the worktree root into a fresh directory.
fn render_from_worktree_root(fixture: &TempDir) -> (TempDir, BTreeMap<PathBuf, Vec<u8>>) {
    let output = TempDir::new().unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["render-tree", "--output-dir"])
        .arg(output.path())
        .args(["--color", "never", "--no-cache"])
        .assert()
        .success();
    let tree = read_tree(&output.path().join("production"));
    (output, tree)
}

#[test]
fn test_render_tree_with_nested_project_and_release_subdirectory_matches_root_layout() {
    let (_root_output, root_layout) = render_from_worktree_root(&fixture());
    let (_nested_output, nested_layout) = render_from_worktree_root(&nested_fixture(NestedReleases::Subdirectory));

    assert_eq!(nested_layout, root_layout);
}

#[test]
fn test_render_tree_with_nested_project_reads_sibling_releases_by_either_path_form() {
    let (_relative_output, relative) =
        render_from_worktree_root(&nested_fixture(NestedReleases::Sibling("../applications/workloads")));
    let (_rooted_output, rooted) =
        render_from_worktree_root(&nested_fixture(NestedReleases::Sibling("/applications/workloads")));

    // Only the group file differs between the two forms, so only its input
    // digest in the ownership index may differ.
    let index_path = PathBuf::from("_nyl/index.json");
    let without_index = |tree: &BTreeMap<PathBuf, Vec<u8>>| {
        tree.iter()
            .filter(|(path, _)| **path != index_path)
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(without_index(&relative), without_index(&rooted));
    let input_keys = |tree: &BTreeMap<PathBuf, Vec<u8>>| {
        let index: serde_json::Value = serde_json::from_slice(&tree[&index_path]).unwrap();
        index["inputs"].as_object().unwrap().keys().cloned().collect::<Vec<_>>()
    };
    assert_eq!(input_keys(&relative), input_keys(&rooted));
    let resources = String::from_utf8(relative[&PathBuf::from("workloads/api/resources.yaml")].clone()).unwrap();
    assert!(
        resources.contains("# Nyl-Provenance: Source: /applications/workloads/api.yaml (document 2)"),
        "{resources}"
    );
    let index: serde_json::Value = serde_json::from_slice(&relative[&index_path]).unwrap();
    assert!(
        index["inputs"].get("/applications/workloads/api.yaml").is_some(),
        "{index}"
    );
    assert!(
        index["inputs"].get("config/targets/production.yaml").is_some(),
        "{index}"
    );
}

#[test]
fn test_render_tree_rejects_group_source_outside_the_worktree() {
    let fixture = nested_fixture(NestedReleases::Sibling("../../outside"));
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--check",
            "--no-cache",
            "--color",
            "never",
            "--output-dir",
        ])
        .arg(fixture.path().join("deploy"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("resolves outside the Git worktree"));
}

/// Move the fixture's project directory from `from` to `to` (both relative to
/// the worktree root; empty for the root itself).
fn move_project(root: &std::path::Path, from: &str, to: &str) {
    fs::create_dir_all(root.join(to)).unwrap();
    for entry in ["nyl.toml", "config", "applications"] {
        fs::rename(root.join(from).join(entry), root.join(to).join(entry)).unwrap();
    }
}

/// Diff the fixture's target against `commit`, run from `directory`.
fn diff_against_source(
    root: &std::path::Path,
    directory: &str,
    commit: &str,
    extra: &[&str],
) -> assert_cmd::assert::Assert {
    let artifacts = TempDir::new().unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(root.join(directory))
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--against",
            "source",
            "--source-ref",
            commit,
            "--source-repository",
        ])
        .arg(root)
        .args(extra)
        .args([
            "--progress",
            "off",
            "--no-stats-stderr",
            "--stats-output",
            "json:-",
            "--output",
        ])
        .arg(artifacts.path().join("source.diff"))
        .assert()
}

fn baseline_location(assert: assert_cmd::assert::Assert) -> (String, String) {
    let report: serde_json::Value = serde_json::from_slice(&assert.success().get_output().stdout).unwrap();
    let baseline = &report["comparison"]["baseline"];
    (
        baseline["project_path"].as_str().unwrap().to_owned(),
        baseline["project_location"].as_str().unwrap().to_owned(),
    )
}

#[test]
fn test_diff_tree_against_source_follows_a_project_move_across_its_merge() {
    let fixture = fixture();
    let root = fixture.path();
    let repository = Repository::open(root).unwrap();
    let head = |repository: &Repository| repository.head().unwrap().peel_to_commit().unwrap().id().to_string();
    move_project(root, "", "infra/nyl-config");
    commit_all(&repository, "Project in infra/nyl-config");
    let before_move = head(&repository);
    move_project(root, "infra/nyl-config", "platform");
    fs::write(
        root.join("platform/nyl.toml"),
        "[project]\nprevious_paths = [\"/infra/nyl-config\"]\n",
    )
    .unwrap();
    commit_all(&repository, "Move the project to platform");
    let after_move = head(&repository);

    // The pull request that moves the project finds the baseline through the
    // earlier location it records.
    let located = baseline_location(diff_against_source(root, "platform", &before_move, &[]));
    assert_eq!(located, ("infra/nyl-config".to_owned(), "previous_path".to_owned()));

    // After the merge, the baseline has the project at the current location, and
    // the leftover setting and option change nothing.
    let located = baseline_location(diff_against_source(
        root,
        "platform",
        &after_move,
        &["--source-project-path", "infra/nyl-config"],
    ));
    assert_eq!(located, ("platform".to_owned(), "same".to_owned()));

    // Without a recorded location the baseline is not found, unless the
    // invocation names a candidate.
    fs::write(root.join("platform/nyl.toml"), "").unwrap();
    diff_against_source(root, "platform", &before_move, &[])
        .failure()
        .stderr(predicate::str::contains("/platform").and(predicate::str::contains("previous_paths")));
    let located = baseline_location(diff_against_source(
        root,
        "platform",
        &before_move,
        &[
            "--source-project-path",
            "gone",
            "--source-project-path",
            "infra/nyl-config",
        ],
    ));
    assert_eq!(located, ("infra/nyl-config".to_owned(), "candidate".to_owned()));
}

#[test]
fn test_publish_tree_verifies_an_uncommitted_project_move_against_the_committed_location() {
    let (fixture, destination, _seed, source_commit) = publication_fixture();
    let root = fixture.path();
    move_project(root, "", "platform");
    fs::write(root.join("platform/nyl.toml"), "[project]\nprevious_paths = [\"/\"]\n").unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(root.join("platform"))
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success();

    let destination = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&destination, "deploy/production");
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&destination, &commit, "production/_nyl/index.json")).unwrap();
    assert_eq!(index["sourceCommit"], source_commit.to_string());
    assert_eq!(index["dirty"], false);
}

#[test]
fn test_publish_tree_removes_the_clean_head_worktree_when_the_committed_project_is_not_found() {
    let (fixture, _destination, _seed, _source_commit) = publication_fixture();
    let root = fixture.path();
    // Moved without recording the earlier location, so HEAD has no project
    // at any location the lookup tries.
    move_project(root, "", "platform");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(root.join("platform"))
        .args(["publish-tree", "--target", "production"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not locate the committed project"));

    let worktrees = Repository::open(root).unwrap().worktrees().unwrap();
    assert_eq!(worktrees.len(), 0, "{:?}", worktrees.iter().collect::<Vec<_>>());
}

const API_RELEASE_WITH_INPUTS: &str = r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  include: [extra.yaml]
  inputs:
    image:
      type: string
      description: Immutable image reference
    replicas:
      type: integer
      default: 2
    database:
      type: object
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: api
  namespace: api
data:
  image: '{{ inputs.image }}'
  replicas: '{{ inputs.replicas }}'
  host: '{{ inputs.database.host }}'
  bindingsVisible: '{{ target.spec.releaseInputs is defined }}'
"#;

fn with_api_inputs(fixture: &TempDir, bindings: &str) {
    fs::write(
        fixture.path().join("applications/workloads/api.yaml"),
        API_RELEASE_WITH_INPUTS,
    )
    .unwrap();
    fs::write(
        fixture.path().join("applications/workloads/extra.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: api-extra\n  namespace: api\ndata:\n  image: '{{ inputs.image }}'\n",
    )
    .unwrap();
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap();
    fs::write(target_path, format!("{target}  releaseInputs:\n{bindings}")).unwrap();
}

fn render_production(fixture: &TempDir) -> assert_cmd::assert::Assert {
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args([
            "render-tree",
            "--target",
            "production",
            "--output-dir",
            fixture.path().join("deploy").to_str().unwrap(),
        ])
        .assert()
}

fn nyl_render(fixture: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .arg("render")
        .args(args)
        .assert()
}

fn with_production_api_bindings(fixture: &TempDir) {
    fs::create_dir_all(fixture.path().join("environments")).unwrap();
    fs::write(
        fixture.path().join("environments/database.yaml"),
        "database:\n  host: db.production.internal\n",
    )
    .unwrap();
    with_api_inputs(
        fixture,
        "    workloads/api:\n      image:\n        value: registry.example.com/api@sha256:4f0c\n      database:\n        fromFile:\n          path: environments/database.yaml\n          pointer: /database\n",
    );
}

#[test]
fn test_render_with_target_applies_the_bindings_render_tree_applies() {
    let fixture = fixture();
    with_production_api_bindings(&fixture);
    let output = nyl_render(&fixture, &["--target", "production", "applications/workloads/api.yaml"]).success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    for expected in [
        "image: registry.example.com/api@sha256:4f0c",
        "replicas: \"2\"",
        "host: db.production.internal",
    ] {
        assert!(stdout.contains(expected), "{expected}: {stdout}");
    }
}

#[test]
fn test_render_overrides_win_over_input_files_and_bindings() {
    let fixture = fixture();
    with_production_api_bindings(&fixture);
    fs::write(fixture.path().join("overrides.yaml"), "image: from-file\nreplicas: 5\n").unwrap();
    let output = nyl_render(
        &fixture,
        &[
            "--target",
            "production",
            "--inputs",
            "overrides.yaml",
            "--input",
            "image=\"from-flag\"",
            "applications/workloads/api.yaml",
        ],
    )
    .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    for expected in ["image: from-flag", "replicas: \"5\"", "host: db.production.internal"] {
        assert!(stdout.contains(expected), "{expected}: {stdout}");
    }
    nyl_render(
        &fixture,
        &[
            "--target",
            "production",
            "--input",
            "unknown=1",
            "applications/workloads/api.yaml",
        ],
    )
    .failure()
    .stderr(predicate::str::contains(
        "set unknown that Release \"api\" does not declare",
    ));
}

#[test]
fn test_render_without_target_uses_defaults_and_overrides_only() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("scratch")).unwrap();
    fs::write(
        fixture.path().join("scratch/local.yaml"),
        "apiVersion: k8s.gitops.nyl/v1\nkind: Release\nmetadata:\n  name: local\n  namespace: local\nspec:\n  inputs:\n    image: {type: string}\n    replicas: {type: integer, default: 2}\n---\napiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: local\n  namespace: local\ndata:\n  image: '{{ inputs.image }}'\n  replicas: '{{ inputs.replicas }}'\n",
    )
    .unwrap();
    nyl_render(&fixture, &["scratch/local.yaml"])
        .failure()
        .stderr(predicate::str::contains("requires input \"image\""));
    let output = nyl_render(&fixture, &["--input", "image=\"local\"", "scratch/local.yaml"]).success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout).into_owned();
    assert!(
        stdout.contains("image: local") && stdout.contains("replicas: \"2\""),
        "{stdout}"
    );
}

#[test]
fn test_render_rejects_a_binding_for_an_undeclared_input_like_render_tree() {
    let fixture = fixture();
    with_api_inputs(
        &fixture,
        "    workloads/api:\n      imagee: {value: typo}\n      image: {value: x}\n      database: {value: {host: h}}\n",
    );
    nyl_render(&fixture, &["--target", "production", "applications/workloads/api.yaml"])
        .failure()
        .stderr(predicate::str::contains(
            "spec.releaseInputs.\"workloads/api\".imagee binds an input Release workloads/api does not declare",
        ));
}

#[test]
fn test_render_group_choice_follows_render_tree_file_selection() {
    let fixture = fixture();
    with_production_api_bindings(&fixture);
    // A Git-ignored copy inside the group's source is not rendered by
    // render-tree, so its bindings do not apply implicitly.
    fs::write(
        fixture.path().join(".gitignore"),
        "applications/workloads/api-local.yaml\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("applications/workloads/api-local.yaml"),
        API_RELEASE_WITH_INPUTS,
    )
    .unwrap();
    nyl_render(
        &fixture,
        &["--target", "production", "applications/workloads/api-local.yaml"],
    )
    .failure()
    .stderr(predicate::str::contains("--application-group"));
    // --defaults-only resolves no group source, so another selected group
    // with a missing source directory does not fail it.
    fs::write(
        fixture.path().join("config/application-groups/broken.yaml"),
        "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: broken\n  labels:\n    environment: production\nspec:\n  projectRef: workloads\n  applicationNamespace: argocd\n  source:\n    path: applications/missing\n",
    )
    .unwrap();
    nyl_render(
        &fixture,
        &[
            "--target",
            "production",
            "--defaults-only",
            "--input",
            "image=\"x\"",
            "--input",
            "database={\"host\": \"h\"}",
            "applications/workloads/api-local.yaml",
        ],
    )
    .success();
}

#[test]
fn test_render_group_flags_need_a_target() {
    let fixture = fixture();
    with_production_api_bindings(&fixture);
    nyl_render(&fixture, &["--defaults-only", "applications/workloads/api.yaml"])
        .failure()
        .stderr(predicate::str::contains(
            "choose among a target's bindings; pass --target",
        ));
}

#[test]
fn test_render_does_not_read_the_publication_branch_for_overridden_inputs() {
    let fixture = fixture();
    // The fixture's publication repository is unreachable.
    with_api_inputs(
        &fixture,
        "    workloads/api:\n      image: {fromPublication: {path: state.json}}\n      database: {value: {host: h}}\n",
    );
    let output = nyl_render(
        &fixture,
        &[
            "--target",
            "production",
            "--input",
            "image=\"override\"",
            "applications/workloads/api.yaml",
        ],
    )
    .success();
    assert!(String::from_utf8_lossy(&output.get_output().stdout).contains("image: override"));
}

#[test]
fn test_render_requires_a_group_choice_for_a_release_outside_the_targets_groups() {
    let fixture = fixture();
    with_production_api_bindings(&fixture);
    fs::create_dir_all(fixture.path().join("scratch")).unwrap();
    fs::write(fixture.path().join("scratch/api.yaml"), API_RELEASE_WITH_INPUTS).unwrap();
    fs::write(
        fixture.path().join("scratch/extra.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: api-extra\n  namespace: api\n",
    )
    .unwrap();
    nyl_render(&fixture, &["--target", "production", "scratch/api.yaml"])
        .failure()
        .stderr(predicate::str::contains("--defaults-only"));
    // The group's bindings apply when it is named.
    let output = nyl_render(
        &fixture,
        &[
            "--target",
            "production",
            "--application-group",
            "workloads",
            "scratch/api.yaml",
        ],
    )
    .success();
    assert!(String::from_utf8_lossy(&output.get_output().stdout).contains("host: db.production.internal"));
    nyl_render(
        &fixture,
        &[
            "--target",
            "production",
            "--defaults-only",
            "--input",
            "image=\"x\"",
            "--input",
            "database={\"host\": \"h\"}",
            "scratch/api.yaml",
        ],
    )
    .success();
}

#[test]
fn release_inputs_render_from_values_files_and_defaults() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("environments")).unwrap();
    fs::write(
        fixture.path().join("environments/database.yaml"),
        "database:\n  host: db.production.internal\n",
    )
    .unwrap();
    with_api_inputs(
        &fixture,
        r#"    workloads/api:
      image:
        value: registry.example.com/api@sha256:4f0c
      database:
        fromFile:
          path: environments/database.yaml
          pointer: /database
"#,
    );
    render_production(&fixture).success();

    let tree = read_tree(&fixture.path().join("deploy/production"));
    let manifests = tree
        .iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        manifests.contains("image: registry.example.com/api@sha256:4f0c"),
        "{manifests}"
    );
    assert!(manifests.contains("replicas: \"2\""), "{manifests}");
    assert!(manifests.contains("host: db.production.internal"), "{manifests}");
    assert!(manifests.contains("bindingsVisible: \"false\""), "{manifests}");
    assert!(manifests.contains("name: api-extra"), "{manifests}");
    assert_eq!(manifests.matches("registry.example.com/api@sha256:4f0c").count(), 2);

    let index: serde_json::Value = serde_json::from_slice(&tree[&PathBuf::from("_nyl/index.json")]).unwrap();
    let inputs = index["inputs"].as_object().unwrap();
    assert!(inputs.contains_key("environments/database.yaml"), "{inputs:?}");
    for input in ["image", "replicas", "database"] {
        let digest = inputs[&format!("@input/workloads/api/{input}")].as_str().unwrap();
        assert_eq!(digest.len(), 64);
    }
}

#[test]
fn release_input_changes_reach_rendered_output_through_the_cache() {
    let fixture = fixture();
    fs::write(fixture.path().join("database.json"), r#"{"host": "one"}"#).unwrap();
    with_api_inputs(
        &fixture,
        "    workloads/api:\n      image: {value: a}\n      database: {fromFile: {path: database.json}}\n",
    );
    render_production(&fixture).success();
    fs::write(fixture.path().join("database.json"), r#"{"host": "two"}"#).unwrap();
    render_production(&fixture).success();
    let tree = read_tree(&fixture.path().join("deploy/production"));
    let rendered = tree
        .iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect::<String>();
    assert!(rendered.contains("host: two"), "{rendered}");
}

#[test]
fn release_input_problems_are_reported_together_before_rendering() {
    let fixture = fixture();
    with_api_inputs(
        &fixture,
        r#"    workloads/api:
      replicas: {value: "3"}
      unknown: {value: 1}
      database: {fromUnit: {unit: database, output: facts}}
    workloads/missing:
      image: {value: x}
"#,
    );
    render_production(&fixture)
        .failure()
        .stderr(predicate::str::contains(
            "DeploymentTarget \"production\" has invalid Release inputs",
        ))
        .stderr(predicate::str::contains(
            "requires input \"image\" (Immutable image reference)",
        ))
        .stderr(predicate::str::contains("expected integer, got string"))
        .stderr(predicate::str::contains("does not declare"))
        .stderr(predicate::str::contains("fromUnit needs orchestrated execution"))
        .stderr(predicate::str::contains("\"workloads/missing\" names no Release"));
    assert!(!fixture.path().join("deploy").exists());
}

#[test]
fn release_input_bindings_of_disabled_groups_are_ignored() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap();
    fs::write(group_path, format!("{group}  enabled: false\n")).unwrap();
    with_api_inputs(&fixture, "    workloads/api:\n      image: {value: x}\n");
    render_production(&fixture).success();
}

#[test]
fn publication_bindings_of_disabled_groups_never_read_the_branch() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap();
    fs::write(group_path, format!("{group}  enabled: false\n")).unwrap();
    // The fixture's publication repository is unreachable.
    with_api_inputs(
        &fixture,
        "    workloads/api:\n      image: {fromPublication: {path: state.json}}\n",
    );
    render_production(&fixture).success();
}

#[test]
fn release_input_files_under_at_prefixed_directories_enter_the_index() {
    let fixture = fixture();
    fs::create_dir_all(fixture.path().join("@shared")).unwrap();
    fs::write(fixture.path().join("@shared/database.json"), r#"{"host": "db"}"#).unwrap();
    with_api_inputs(
        &fixture,
        "    workloads/api:\n      image: {value: a}\n      database: {fromFile: {path: '@shared/database.json'}}\n",
    );
    render_production(&fixture).success();
    let tree = read_tree(&fixture.path().join("deploy/production"));
    let index: serde_json::Value = serde_json::from_slice(&tree[&PathBuf::from("_nyl/index.json")]).unwrap();
    assert!(
        index["inputs"]["@shared/database.json"].is_string(),
        "{}",
        index["inputs"]
    );
}

#[test]
fn release_input_files_must_be_visible_to_git() {
    let fixture = fixture();
    fs::write(fixture.path().join(".gitignore"), "local.json\n").unwrap();
    fs::write(fixture.path().join("local.json"), r#"{"host": "db"}"#).unwrap();
    with_api_inputs(
        &fixture,
        "    workloads/api:\n      image: {value: a}\n      database: {fromFile: {path: local.json}}\n",
    );
    render_production(&fixture)
        .failure()
        .stderr(predicate::str::contains("names no Git-visible YAML or JSON file"));
}

#[test]
fn release_inputs_must_be_declared_literally() {
    let fixture = fixture();
    with_api_inputs(&fixture, "    workloads/api:\n      image: {value: x}\n");
    let release_path = fixture.path().join("applications/workloads/api.yaml");
    let release = fs::read_to_string(&release_path)
        .unwrap()
        .replace("      default: 2\n", "      default: '{{ values.replicas }}'\n");
    fs::write(release_path, release).unwrap();
    render_production(&fixture)
        .failure()
        .stderr(predicate::str::contains("must declare spec.inputs literally"));
}

#[test]
fn remote_application_groups_receive_resolved_inputs_only() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("image.json"),
        r#"{"image": "registry.example.com/api@sha256:remote"}"#,
    )
    .unwrap();
    let remote = TempDir::new().unwrap();
    let repository = Repository::init(remote.path()).unwrap();
    fs::create_dir(remote.path().join("releases")).unwrap();
    fs::write(
        remote.path().join("releases/api.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  inputs:
    image: {type: string}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: api
  namespace: api
data:
  image: '{{ inputs.image }}'
"#,
    )
    .unwrap();
    commit_all(&repository, "Remote release");
    let commit = repository.head().unwrap().target().unwrap().to_string();
    let url = reqwest::Url::from_directory_path(remote.path()).unwrap().to_string();
    let group = fixture.path().join("config/application-groups/workloads.yaml");
    fs::write(
        &group,
        format!(
            "{}\n  source:\n    repository: {{repoURL: '{url}'}}\n    revision: HEAD\n    commit: '{commit}'\n    path: releases\n",
            fs::read_to_string(&group).unwrap()
        ),
    )
    .unwrap();
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap();
    fs::write(
        target_path,
        format!("{target}  releaseInputs:\n    workloads/api:\n      image: {{fromFile: {{path: image.json, pointer: /image}}}}\n"),
    )
    .unwrap();
    let cache = TempDir::new().unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("NYL_CACHE_DIR", cache.path())
        .timeout(std::time::Duration::from_secs(30))
        .args(["render-tree", "--target", "production", "--output-dir"])
        .arg(fixture.path().join("deploy"))
        .assert()
        .success();
    let tree = read_tree(&fixture.path().join("deploy/production"));
    let rendered = tree
        .iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect::<String>();
    assert!(
        rendered.contains("image: registry.example.com/api@sha256:remote"),
        "{rendered}"
    );
}

/// A local state repository with a history on branch `deploy/dev`.
struct StateRepository {
    directory: TempDir,
    repository: Repository,
}

impl StateRepository {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let repository = Repository::init(directory.path()).unwrap();
        Self { directory, repository }
    }

    fn url(&self) -> String {
        reqwest::Url::from_directory_path(self.directory.path())
            .unwrap()
            .to_string()
    }

    /// Commit `contents` at `path` on top of HEAD and point `branch` at it.
    fn commit(&self, path: &str, contents: &str, message: &str, branch: &str) -> String {
        let file = self.directory.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, contents).unwrap();
        commit_all(&self.repository, message);
        let commit = self.repository.head().unwrap().peel_to_commit().unwrap();
        self.repository.branch(branch, &commit, true).unwrap();
        commit.id().to_string()
    }
}

fn with_image_binding(fixture: &TempDir, bindings: &str) {
    fs::write(
        fixture.path().join("applications/workloads/api.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  inputs:
    image: {type: string}
    tag: {type: string, default: none}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: api
  namespace: api
data:
  image: '{{ inputs.image }}'
  tag: '{{ inputs.tag }}'
"#,
    )
    .unwrap();
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap();
    fs::write(
        target_path,
        format!("{target}  releaseInputs:\n    workloads/api:\n{bindings}"),
    )
    .unwrap();
}

fn update_source_locks(fixture: &TempDir, cache: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("NYL_CACHE_DIR", cache.path())
        .timeout(std::time::Duration::from_secs(60))
        .args(["update", "source-locks"])
        .args(args)
        .assert()
}

#[test]
fn test_render_tree_from_git_reads_the_locked_commit() {
    let fixture = fixture();
    let state = StateRepository::new();
    let first = state.commit(
        "dev/images.json",
        r#"{"api": "registry.example.com/api@sha256:one"}"#,
        "One",
        "deploy/dev",
    );
    state.commit(
        "dev/images.json",
        r#"{"api": "registry.example.com/api@sha256:two"}"#,
        "Two",
        "deploy/dev",
    );
    let url = state.url();
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: deploy/dev\n          commit: {first}\n          path: dev/images.json\n          pointer: /api\n"
        ),
    );
    let cache = TempDir::new().unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("NYL_CACHE_DIR", cache.path())
        .timeout(std::time::Duration::from_secs(60))
        .args(["render-tree", "--target", "production", "--output-dir"])
        .arg(fixture.path().join("deploy"))
        .assert()
        .success();
    let tree = read_tree(&fixture.path().join("deploy/production"));
    let rendered = tree
        .iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect::<String>();
    assert!(
        rendered.contains("image: registry.example.com/api@sha256:one"),
        "{rendered}"
    );
    let index: serde_json::Value = serde_json::from_slice(&tree[&PathBuf::from("_nyl/index.json")]).unwrap();
    assert!(
        index["inputs"]
            .as_object()
            .unwrap()
            .contains_key(&format!("@git/{url}@{first}/dev/images.json")),
        "{index}"
    );
}

#[test]
fn test_vendor_captures_from_git_locks_for_required_offline_renders() {
    let fixture = fixture();
    fs::write(fixture.path().join("nyl.toml"), "[vendor]\nmode='required'\n").unwrap();
    let state = StateRepository::new();
    let commit = state.commit(
        "dev/images.json",
        r#"{"api": "registry.example.com/api@sha256:vendored"}"#,
        "Images",
        "deploy/dev",
    );
    let url = state.url();
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: deploy/dev\n          commit: {commit}\n          path: dev/images.json\n          pointer: /api\n"
        ),
    );
    let nyl = |cache: &TempDir, args: &[&str]| {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .env("NYL_CACHE_DIR", cache.path())
            .timeout(std::time::Duration::from_secs(60))
            .args(args)
            .assert()
    };
    let output = fixture.path().join("deploy");
    let render = [
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        output.to_str().unwrap(),
    ];

    // Required mode never reads a locked file from the network.
    nyl(&TempDir::new().unwrap(), &render)
        .failure()
        .stderr(predicate::str::contains(format!(
            ".fromGit: Remote artifact {url}@{commit}#dev/images.json is not present in the required vendor lock; run 'nyl vendor'"
        )));
    nyl(&TempDir::new().unwrap(), &["vendor"]).success();
    let lock = fs::read_to_string(fixture.path().join("vendor/lock.yaml")).unwrap();
    assert!(lock.contains("kind: git-blob"), "{lock}");
    nyl(&TempDir::new().unwrap(), &["vendor", "--check"]).success();

    // With the repository gone and an empty cache, the snapshot alone renders.
    drop(state);
    nyl(&TempDir::new().unwrap(), &render).success();
    let rendered = read_tree(&output.join("production"))
        .into_iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<String>();
    assert!(
        rendered.contains("image: registry.example.com/api@sha256:vendored"),
        "{rendered}"
    );
}

#[test]
fn test_update_source_locks_moves_from_git_groups_and_keeps_other_revisions() {
    let fixture = fixture();
    let state = StateRepository::new();
    let first = state.commit(
        "dev/images.json",
        r#"{"api": "one", "tag": "one"}"#,
        "One",
        "deploy/dev",
    );
    state
        .repository
        .branch(
            "pinned",
            &state
                .repository
                .find_commit(git2::Oid::from_str(&first).unwrap())
                .unwrap(),
            true,
        )
        .unwrap();
    let second = state.commit(
        "dev/images.json",
        r#"{"api": "two", "tag": "two"}"#,
        "Two",
        "deploy/dev",
    );
    let url = state.url();
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: deploy/dev\n          commit: {first}\n          path: dev/images.json\n          pointer: /api\n      tag:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: pinned\n          commit: {first}\n          path: dev/images.json\n          pointer: /tag\n"
        ),
    );
    let cache = TempDir::new().unwrap();
    update_source_locks(&fixture, &cache, &["--target", "production", "--check"])
        .failure()
        .stdout(predicate::str::contains(format!("resolves to {second}")));

    update_source_locks(&fixture, &cache, &["--target", "production"]).success();
    let target = fs::read_to_string(fixture.path().join("config/targets/production.yaml")).unwrap();
    assert!(
        target.contains(&format!("revision: deploy/dev\n          commit: {second}")),
        "{target}"
    );
    assert!(
        target.contains(&format!("revision: pinned\n          commit: {first}")),
        "{target}"
    );

    update_source_locks(&fixture, &cache, &["--target", "production", "--check"]).success();
}

#[test]
fn test_update_source_locks_moves_publication_prefix_locks_to_the_branch_head() {
    let fixture = fixture();
    let state = StateRepository::new();
    state.commit(
        "dev/state/images.json",
        r#"{"api": "published"}"#,
        "Publish\n\nNyl-Deployment-Target: dev\n",
        "deploy/dev",
    );
    let head = state.commit(
        "dev/state/images.json",
        r#"{"api": "unpublished"}"#,
        "Write back",
        "deploy/dev",
    );
    let url = state.url();
    fs::write(
        fixture.path().join("config/targets/dev.yaml"),
        format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: dev\nspec:\n  clusterRef:\n    name: kasoku\n  applicationGroupSelector:\n    matchLabels:\n      environment: dev\n  publication:\n    repository: {{repoURL: '{url}'}}\n    revision: deploy/dev\n    pathPrefix: dev\n"
        ),
    )
    .unwrap();
    let zero = "0".repeat(40);
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: deploy/dev\n          commit: '{zero}'\n          path: dev/state/images.json\n          pointer: /api\n      tag:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: deploy/dev\n          commit: \"{zero}\"\n          path: shared/tag.json\n"
        ),
    );
    let cache = TempDir::new().unwrap();
    update_source_locks(&fixture, &cache, &["--target", "production"]).success();
    let target = fs::read_to_string(fixture.path().join("config/targets/production.yaml")).unwrap();
    // Choosing what another target runs is promotion's job: a lock on a file
    // in dev's publication prefix follows the branch head like any lock.
    assert_eq!(target.matches(&head).count(), 2, "{target}");
}

#[test]
fn test_render_tree_from_git_repository_ref_records_the_repository_from_a_subdirectory() {
    let fixture = fixture();
    let state = StateRepository::new();
    let commit = state.commit("images.json", r#"{"api": "one"}"#, "One", "main");
    fs::write(
        fixture.path().join("config/repositories/state.yaml"),
        format!(
            "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: state\nspec:\n  repoURL: '{}'\n",
            state.url()
        ),
    )
    .unwrap();
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repositoryRef: {{name: state}}\n          revision: main\n          commit: {commit}\n          path: images.json\n          pointer: /api\n"
        ),
    );
    let cache = TempDir::new().unwrap();
    // The GitRepository resource path is project-relative; the render cache
    // must read it from the project, not from the working directory.
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path().join("applications"))
        .env("NYL_CACHE_DIR", cache.path())
        .timeout(std::time::Duration::from_secs(60))
        .args(["render-tree", "--target", "production", "--output-dir"])
        .arg(fixture.path().join("deploy"))
        .assert()
        .success();
    let tree = read_tree(&fixture.path().join("deploy/production"));
    let rendered = tree
        .iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect::<String>();
    assert!(rendered.contains("image: one"), "{rendered}");
}

#[test]
fn test_render_tree_from_git_reports_an_unavailable_locked_commit() {
    let fixture = fixture();
    let state = StateRepository::new();
    state.commit("images.json", r#"{"api": "one"}"#, "One", "main");
    let url = state.url();
    let missing = "f".repeat(40);
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: main\n          commit: {missing}\n          path: images.json\n          pointer: /api\n"
        ),
    );
    let cache = TempDir::new().unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("NYL_CACHE_DIR", cache.path())
        .timeout(std::time::Duration::from_secs(60))
        .args(["render-tree", "--target", "production", "--check", "--output-dir"])
        .arg(fixture.path().join("deploy"))
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!("at locked commit {missing}")))
        .stderr(predicate::str::contains("must already be in the local Git cache"));
}

#[test]
fn test_validate_rejects_application_name_templates_without_an_expression() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "  projectRef: workloads\n",
        "  projectRef: workloads\n  applicationNameTemplate: '{% raw %}{{ release.metadata.name }}{% endraw %}'\n",
    );
    fs::write(group_path, group).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["render-tree", "--target", "production", "--check", "--output-dir"])
        .arg(fixture.path().join("deploy"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("must contain at least one ${ … } expression"));
}

#[test]
fn test_update_source_locks_filtered_updates_agree_with_unfiltered_checks() {
    let fixture = fixture();
    let state = StateRepository::new();
    state.commit(
        "dev/state/images.json",
        r#"{"api": "published"}"#,
        "Publish\n\nNyl-Deployment-Target: dev\n",
        "deploy/dev",
    );
    let head = state.commit("releases/placeholder.txt", "x", "Write back", "deploy/dev");
    let url = state.url();
    let zero = "0".repeat(40);
    fs::write(
        fixture.path().join("config/targets/dev.yaml"),
        format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: dev\nspec:\n  clusterRef:\n    name: kasoku\n  applicationGroupSelector:\n    matchLabels:\n      environment: dev\n  publication:\n    repository: {{repoURL: '{url}'}}\n    revision: deploy/dev\n    pathPrefix: dev\n"
        ),
    )
    .unwrap();
    fs::write(
        fixture.path().join("config/application-groups/workloads.yaml"),
        format!(
            "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: workloads\n  labels:\n    environment: production\nspec:\n  projectRef: workloads\n  applicationNamespace: argocd\n  source:\n    repository: {{repoURL: '{url}'}}\n    revision: deploy/dev\n    commit: '{zero}'\n    path: releases\n"
        ),
    )
    .unwrap();
    with_image_binding(
        &fixture,
        &format!(
            "      image:\n        fromGit:\n          repository: {{repoURL: '{url}'}}\n          revision: deploy/dev\n          commit: '{zero}'\n          path: dev/state/images.json\n          pointer: /api\n"
        ),
    );
    let cache = TempDir::new().unwrap();

    // The filter decides only which locks move; every lock moves to the head.
    update_source_locks(&fixture, &cache, &["workloads"]).success();
    let group = fs::read_to_string(fixture.path().join("config/application-groups/workloads.yaml")).unwrap();
    assert!(group.contains(&head), "{group}");
    let target = fs::read_to_string(fixture.path().join("config/targets/production.yaml")).unwrap();
    assert!(target.contains(&format!("commit: '{zero}'")), "{target}");

    update_source_locks(&fixture, &cache, &["--target", "production"]).success();
    let target = fs::read_to_string(fixture.path().join("config/targets/production.yaml")).unwrap();
    assert!(target.contains(&format!("commit: '{head}'")), "{target}");
    update_source_locks(&fixture, &cache, &["--check"]).success();
}

/// Commit `contents` at `path` on the publication branch `deploy/production`
/// through the seed clone and push it, as a tool outside Nyl would.
fn push_publication_state(seed: &TempDir, path: &str, contents: &str) -> git2::Oid {
    push_publication_change(seed, path, Some(contents))
}

/// Commit `contents` at `path`, or its deletion, on `deploy/production`.
fn push_publication_change(seed: &TempDir, path: &str, contents: Option<&str>) -> git2::Oid {
    let repository = Repository::open(seed.path()).unwrap();
    let branch = "deploy/production";
    let remote_branch = repository
        .find_remote("origin")
        .unwrap()
        .fetch(
            &[format!("+refs/heads/{branch}:refs/remotes/origin/{branch}")],
            None,
            None,
        )
        .ok()
        .and_then(|()| repository.find_reference(&format!("refs/remotes/origin/{branch}")).ok())
        .and_then(|reference| reference.peel_to_commit().ok());
    let base = remote_branch.unwrap_or_else(|| {
        repository
            .find_reference("refs/heads/main")
            .unwrap()
            .peel_to_commit()
            .unwrap()
    });
    // Detach first: the branch may be checked out from an earlier push.
    repository.set_head_detached(base.id()).unwrap();
    repository.branch(branch, &base, true).unwrap();
    repository.set_head(&format!("refs/heads/{branch}")).unwrap();
    repository
        .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let file = seed.path().join(path);
    if let Some(contents) = contents {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, contents).unwrap();
    } else {
        fs::remove_file(file).unwrap();
    }
    commit_all(&repository, "Write state");
    repository
        .find_remote("origin")
        .unwrap()
        .push(&[format!("+refs/heads/{branch}:refs/heads/{branch}")], None)
        .unwrap();
    let head = repository.head().unwrap().peel_to_commit().unwrap().id();
    head
}

/// Bind the `api` Release's `image` input from publication state and commit
/// the source change.
fn with_publication_binding(fixture: &TempDir, binding: &str) {
    with_image_binding(fixture, &format!("      image:\n        fromPublication:\n{binding}"));
    commit_all(&Repository::open(fixture.path()).unwrap(), "Bind publication state");
}

fn publish_production(fixture: &TempDir) -> assert_cmd::assert::Assert {
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production"])
        .assert()
}

fn published_api_manifests(destination: &TempDir) -> String {
    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    let tree = commit.tree().unwrap();
    let mut rendered = String::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
        if root.starts_with("production/workloads/api") && entry.kind() == Some(git2::ObjectType::Blob) {
            rendered.push_str(&String::from_utf8_lossy(
                repository.find_blob(entry.id()).unwrap().content(),
            ));
        }
        git2::TreeWalkResult::Ok
    })
    .unwrap();
    rendered
}

/// One target renders from every non-orchestrated binding kind through both
/// tree commands, and publishes the carried state with the manifests rendered
/// from it in one commit on top of the committed state.
#[test]
fn test_tree_commands_render_every_non_orchestrated_input_kind() {
    let (fixture, destination, seed, _) = publication_fixture();
    let cache = TempDir::new().unwrap();
    let locked = StateRepository::new();
    let commit = locked.commit("sizing.json", r#"{"web": {"tier": "large"}}"#, "Sizing", "main");
    push_publication_state(
        &seed,
        "production/state/committed.json",
        r#"{"api": "committed-value"}"#,
    );
    fs::write(
        fixture.path().join("applications/workloads/api.yaml"),
        r#"apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  inputs:
    static: {type: string}
    file: {type: string}
    locked: {type: string}
    committed: {type: string}
    carried: {type: string}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: api
  namespace: api
data:
  static: '{{ inputs.static }}'
  file: '{{ inputs.file }}'
  locked: '{{ inputs.locked }}'
  committed: '{{ inputs.committed }}'
  carried: '{{ inputs.carried }}'
"#,
    )
    .unwrap();
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap();
    fs::write(
        &target_path,
        format!(
            "{target}  releaseInputs:\n    workloads/api:\n      static:\n        value: static-value\n      file:\n        fromFile:\n          path: config/values/file.json\n          pointer: /api\n      locked:\n        fromGit:\n          repository: {{repoURL: '{}'}}\n          revision: main\n          commit: '{commit}'\n          path: sizing.json\n          pointer: /web/tier\n      committed:\n        fromPublication:\n          path: state/committed.json\n          pointer: /api\n      carried:\n        fromPublication:\n          path: state/carried.json\n          pointer: /api\n          carryFileFromWorktree: build/carried.json\n",
            locked.url()
        ),
    )
    .unwrap();
    fs::create_dir_all(fixture.path().join("config/values")).unwrap();
    fs::write(
        fixture.path().join("config/values/file.json"),
        r#"{"api": "file-value"}"#,
    )
    .unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Bind every input kind");
    let nyl = |args: &[&str]| {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .env("NYL_CACHE_DIR", cache.path())
            .timeout(std::time::Duration::from_secs(60))
            .args(args)
            .assert()
    };
    let output_dir = fixture.path().join("deploy");
    fs::create_dir_all(fixture.path().join("build")).unwrap();
    fs::write(fixture.path().join("build/carried.json"), r#"{"api": "carried-value"}"#).unwrap();

    let expected = [
        "static: static-value",
        "file: file-value",
        "locked: large",
        "committed: committed-value",
        "carried: carried-value",
    ];
    nyl(&[
        "render-tree",
        "--target",
        "production",
        "--output-dir",
        output_dir.to_str().unwrap(),
    ])
    .success();
    let rendered = read_tree(&fixture.path().join("deploy/production"))
        .into_iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
        .collect::<String>();
    for line in expected {
        assert!(rendered.contains(line), "{line} missing from:\n{rendered}");
    }

    let base = published_commit(&Repository::open_bare(destination.path()).unwrap(), "deploy/production").id();
    nyl(&["publish-tree", "--target", "production"]).success();
    let published = published_api_manifests(&destination);
    for line in expected {
        assert!(published.contains(line), "{line} missing from:\n{published}");
    }
    let repository = Repository::open_bare(destination.path()).unwrap();
    let head = published_commit(&repository, "deploy/production");
    // One commit on the base holds the manifests, the carried state rendered
    // into them, and the committed state it left in place.
    assert_eq!(head.parent_id(0).unwrap(), base);
    assert_eq!(
        published_file(&repository, &head, "production/state/committed.json"),
        br#"{"api": "committed-value"}"#
    );
    assert_eq!(
        published_file(&repository, &head, "production/state/carried.json"),
        br#"{"api": "carried-value"}"#
    );
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&repository, &head, "production/_nyl/index.json")).unwrap();
    for key in [
        "@input/workloads/api/static",
        "@input/workloads/api/file",
        "@input/workloads/api/locked",
        "@publication/state/committed.json",
        "@carried/state/carried.json",
    ] {
        assert!(index["inputs"].get(key).is_some(), "{key} missing from {index}");
    }
}

/// Output of a project without Release inputs, as the release before Release
/// inputs rendered it; only `sourceCommit` in the index varies per run.
/// Regenerate deliberately with `render-tree --target production` on this
/// fixture when an intended change alters it.
#[test]
fn test_render_tree_without_inputs_matches_the_pre_inputs_output() {
    let fixture = fixture();
    render_production(&fixture).success();
    let mut rendered = read_tree(&fixture.path().join("deploy/production"));
    let index = rendered.get_mut(&PathBuf::from("_nyl/index.json")).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(index).unwrap();
    value["sourceCommit"] = serde_json::Value::Null;
    *index = serde_json::to_vec_pretty(&value).unwrap();
    let golden =
        read_tree(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/render-tree-without-inputs"));
    assert_eq!(
        rendered.keys().collect::<Vec<_>>(),
        golden.keys().collect::<Vec<_>>(),
        "rendered file set changed"
    );
    for (path, bytes) in &golden {
        assert_eq!(
            String::from_utf8_lossy(&rendered[path]),
            String::from_utf8_lossy(bytes),
            "{} changed",
            path.display()
        );
    }
}

/// A state push that lands while `publish-tree` renders makes publication
/// fail instead of publishing manifests rendered from older state. A fake
/// `helm` on the command's PATH pushes the state during `helm template`.
#[cfg(unix)]
#[test]
fn test_publish_tree_fails_when_state_is_pushed_while_rendering() {
    use std::os::unix::fs::PermissionsExt;

    let (fixture, destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "read-before-render"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");
    let release = fixture.path().join("applications/workloads/api.yaml");
    let bundle = fs::read_to_string(&release).unwrap();
    fs::write(
        &release,
        format!("{bundle}---\napiVersion: k8s.nyl/v1\nkind: HelmChart\nmetadata:\n  name: chart\n  namespace: api\nspec:\n  chart:\n    name: ./charts/chart\n"),
    )
    .unwrap();
    fs::create_dir_all(fixture.path().join("charts/chart/templates")).unwrap();
    fs::write(
        fixture.path().join("charts/chart/Chart.yaml"),
        "apiVersion: v2\nname: chart\nversion: 0.1.0\n",
    )
    .unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Render a chart");

    let bin = TempDir::new().unwrap();
    let helm = bin.path().join("helm");
    fs::write(
        &helm,
        format!(
            "#!/bin/sh\nset -e\ncase \"$1\" in\n  version) echo v3.15.0 ;;\n  template)\n    cd '{seed}'\n    git checkout -q deploy/production\n    git pull -q origin deploy/production\n    echo '{{\"api\": \"pushed-while-rendering\"}}' > production/state/images.json\n    git -c user.name=ci -c user.email=ci@example.invalid commit -qam 'Concurrent state' >&2\n    git push -q origin deploy/production >&2\n    printf 'apiVersion: v1\\nkind: ConfigMap\\nmetadata:\\n  name: from-chart\\n  namespace: api\\n' ;;\nesac\n",
            seed = seed.path().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&helm, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), std::env::var("PATH").unwrap_or_default());

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .env("PATH", path)
        .timeout(std::time::Duration::from_secs(60))
        .args(["publish-tree", "--target", "production"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("another writer pushed while rendering"));
    // The branch holds the concurrent state and no publication built on it.
    let repository = Repository::open_bare(destination.path()).unwrap();
    let head = published_commit(&repository, "deploy/production");
    assert_eq!(head.summary().unwrap(), Some("Concurrent state"));
    assert!(head
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("production/_nyl/index.json"))
        .is_err());
}

#[test]
fn test_vendor_check_reads_cached_publication_state_offline() {
    let (fixture, destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:state"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");
    fs::write(fixture.path().join("nyl.toml"), "[vendor]\nmode='required'\n").unwrap();
    let nyl = |cache: &TempDir, args: &[&str]| {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .env("NYL_CACHE_DIR", cache.path())
            .timeout(std::time::Duration::from_secs(60))
            .args(args)
            .assert()
    };
    let cache = TempDir::new().unwrap();
    nyl(&cache, &["vendor"]).success();
    // Nothing from the publication branch enters the snapshot.
    let lock = fs::read_to_string(fixture.path().join("vendor/lock.yaml")).unwrap();
    assert!(!lock.contains("images.json"), "{lock}");

    // Offline, the check renders the cached state the next render reads.
    fs::remove_dir_all(destination.path()).unwrap();
    nyl(&cache, &["vendor", "--check"])
        .success()
        .stderr(predicate::str::contains("(cached head; the refresh failed)"));
    // With no cached copy either, the bootstrap rule applies and names why.
    nyl(&TempDir::new().unwrap(), &["vendor", "--check"])
        .failure()
        .stderr(predicate::str::contains("state file state/images.json is unavailable"));
}

#[test]
fn test_render_offline_reads_publication_state_at_the_cached_head() {
    let (fixture, destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:cached"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");
    let cache = TempDir::new().unwrap();
    let render = |offline: bool| {
        let mut command = Command::cargo_bin("nyl").unwrap();
        command
            .current_dir(fixture.path())
            .env("NYL_CACHE_DIR", cache.path())
            .args(["render", "--target", "production"]);
        if offline {
            command.arg("--offline");
        }
        command.arg("applications/workloads/api.yaml").assert()
    };
    render(false).success();
    fs::remove_dir_all(destination.path()).unwrap();
    let output = render(true)
        .success()
        .stderr(predicate::str::contains("(cached head, --offline)"));
    assert!(String::from_utf8_lossy(&output.get_output().stdout).contains("registry.example.com/api@sha256:cached"));
}

#[test]
fn test_validate_reads_cached_publication_state_offline() {
    let (fixture, destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:state"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");
    let validate = |cache: &TempDir| {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .env("NYL_CACHE_DIR", cache.path())
            .timeout(std::time::Duration::from_secs(60))
            .arg("validate")
            .assert()
    };
    let cache = TempDir::new().unwrap();
    validate(&cache).success();

    // Offline, validation falls back to the cached branch head.
    fs::remove_dir_all(destination.path()).unwrap();
    validate(&cache)
        .success()
        .stdout(predicate::str::contains("GitOps configuration is valid"));
}

#[test]
fn test_publish_tree_renders_committed_publication_state_at_the_base_commit() {
    let (fixture, destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:committed"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");

    publish_production(&fixture)
        .success()
        .stdout(predicate::str::contains("Published deployment target production"));
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:committed"));
    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&repository, &commit, "production/_nyl/index.json")).unwrap();
    // The committed state file stays in the tree, unowned, and its digest is provenance.
    assert!(published_file(&repository, &commit, "production/state/images.json").starts_with(b"{"));
    assert!(index["files"].get("state/images.json").is_none(), "{index}");
    assert!(
        index["inputs"].get("@publication/state/images.json").is_some(),
        "{index}"
    );

    publish_production(&fixture)
        .success()
        .stdout(predicate::str::contains("is already published"));
}

#[test]
fn test_diff_tree_accepts_an_unindexed_prefix_only_with_declared_state() {
    let (fixture, _destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:committed"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");
    let diff = || {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .args([
                "diff-tree",
                "--target",
                "production",
                "--against",
                "published",
                "--color",
                "never",
            ])
            .assert()
    };
    // Before the first publication the prefix holds only declared state.
    diff()
        .success()
        .stdout(predicate::str::contains("production/workloads/api"));

    // A file no binding declares means the prefix belongs to something else.
    push_publication_state(&seed, "production/hand-written.yaml", "kind: ConfigMap\n");
    diff()
        .failure()
        .stderr(predicate::str::contains("has no ownership index"))
        .stderr(predicate::str::contains("- hand-written.yaml"));
}

#[test]
fn test_diff_tree_source_baseline_reads_its_own_publication_after_a_move() {
    let (fixture, _destination, seed, _) = publication_fixture();
    push_publication_state(
        &seed,
        "production/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:before"}"#,
    );
    push_publication_state(
        &seed,
        "moved/state/images.json",
        r#"{"api": "registry.example.com/api@sha256:after"}"#,
    );
    with_publication_binding(&fixture, "          path: state/images.json\n          pointer: /api\n");
    let baseline = Repository::open(fixture.path())
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    let target = fixture.path().join("config/targets/production.yaml");
    let moved = fs::read_to_string(&target)
        .unwrap()
        .replace("pathPrefix: production", "pathPrefix: moved");
    fs::write(&target, moved).unwrap();

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .timeout(std::time::Duration::from_secs(60))
        .args([
            "diff-tree",
            "--target",
            "production",
            "--against",
            "source",
            "--source-ref",
            &baseline,
            "--source-repository",
            fixture.path().to_str().unwrap(),
            "--progress",
            "off",
            "--color",
            "never",
        ])
        .assert()
        .success()
        // Each side reads the state of the prefix it publishes to.
        .stdout(predicate::str::contains(
            "-  image: registry.example.com/api@sha256:before",
        ))
        .stdout(predicate::str::contains(
            "+  image: registry.example.com/api@sha256:after",
        ))
        .stderr(predicate::str::contains("WARNING (publication_moved)"))
        .stderr(predicate::str::contains("#move-a-publication"));
}

#[test]
fn test_publish_tree_carries_state_through_a_dirty_worktree_check() {
    let (fixture, destination, _seed, _) = publication_fixture();
    with_publication_binding(
        &fixture,
        "          path: state/images.json\n          pointer: /api\n          carryFileFromWorktree: build/images.json\n",
    );
    fs::create_dir_all(fixture.path().join("build")).unwrap();
    let carried = r#"{"api": "registry.example.com/api@sha256:carried"}"#;
    fs::write(fixture.path().join("build/images.json"), carried).unwrap();
    // An unrelated local change makes the worktree dirty, so publish-tree
    // verifies against a clean render of HEAD, which must see the carry file.
    fs::write(fixture.path().join("untracked-note.txt"), "local note\n").unwrap();

    publish_production(&fixture).success();
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:carried"));
    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    assert_eq!(
        published_file(&repository, &commit, "production/state/images.json"),
        carried.as_bytes()
    );
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&repository, &commit, "production/_nyl/index.json")).unwrap();
    assert!(index["files"].get("state/images.json").is_some(), "{index}");
    assert!(index["inputs"].get("@carried/state/images.json").is_some(), "{index}");
    assert_eq!(index["dirty"], false);
}

#[test]
fn test_publish_tree_adopts_an_existing_state_file_when_carry_is_declared() {
    let (fixture, destination, seed, _) = publication_fixture();
    let existing = r#"{"api": "registry.example.com/api@sha256:existing"}"#;
    push_publication_state(&seed, "production/state/images.json", existing);
    with_publication_binding(
        &fixture,
        "          path: state/images.json\n          pointer: /api\n          carryFileFromWorktree: build/images.json\n",
    );

    // No carry file: the base copy is written back and becomes owned.
    publish_production(&fixture).success();
    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    assert_eq!(
        published_file(&repository, &commit, "production/state/images.json"),
        existing.as_bytes()
    );
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&repository, &commit, "production/_nyl/index.json")).unwrap();
    assert!(index["files"].get("state/images.json").is_some(), "{index}");
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:existing"));
}

#[test]
fn test_publish_tree_keeps_state_committed_after_carry_is_dropped() {
    let (fixture, destination, _seed, _) = publication_fixture();
    with_publication_binding(
        &fixture,
        "          path: state/images.json\n          pointer: /api\n          carryFileFromWorktree: build/images.json\n",
    );
    fs::create_dir_all(fixture.path().join("build")).unwrap();
    let carried = r#"{"api": "registry.example.com/api@sha256:carried"}"#;
    fs::write(fixture.path().join("build/images.json"), carried).unwrap();
    publish_production(&fixture).success();

    // Without carry the file is committed state that other tools maintain:
    // the target renders from it and releases ownership instead of deleting it.
    fs::remove_file(fixture.path().join("build/images.json")).unwrap();
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap();
    fs::write(
        &target_path,
        target.replace("          carryFileFromWorktree: build/images.json\n", ""),
    )
    .unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Stop carrying state");
    publish_production(&fixture).success();
    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    assert_eq!(
        published_file(&repository, &commit, "production/state/images.json"),
        carried.as_bytes()
    );
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&repository, &commit, "production/_nyl/index.json")).unwrap();
    assert!(index["files"].get("state/images.json").is_none(), "{index}");
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:carried"));
}

#[test]
fn test_publish_tree_republishes_owned_files_under_line_ending_conversion() {
    let (fixture, destination, _seed, _) = publication_fixture();
    // A user Git config that converts line endings on checkout, as on Windows.
    let home = TempDir::new().unwrap();
    fs::write(home.path().join(".gitconfig"), "[core]\n\tautocrlf = true\n").unwrap();
    let publish = || {
        Command::cargo_bin("nyl")
            .unwrap()
            .current_dir(fixture.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .args(["publish-tree", "--target", "production"])
            .assert()
    };
    with_image_binding(
        &fixture,
        "      image:\n        value: registry.example.com/api@sha256:first\n",
    );
    commit_all(&Repository::open(fixture.path()).unwrap(), "First image");
    publish().success();

    // Republishing reconciles over the owned files the clone checked out.
    let target = fixture.path().join("config/targets/production.yaml");
    let changed = fs::read_to_string(&target)
        .unwrap()
        .replace("sha256:first", "sha256:second");
    fs::write(&target, changed).unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Second image");
    publish().success();
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:second"));
}

#[test]
fn test_publish_tree_deletes_carried_state_when_the_binding_is_removed() {
    let (fixture, destination, _seed, _) = publication_fixture();
    with_publication_binding(
        &fixture,
        "          path: state/images.json\n          pointer: /api\n          carryFileFromWorktree: build/images.json\n",
    );
    fs::create_dir_all(fixture.path().join("build")).unwrap();
    fs::write(
        fixture.path().join("build/images.json"),
        r#"{"api": "registry.example.com/api@sha256:carried"}"#,
    )
    .unwrap();
    publish_production(&fixture).success();

    // No binding names the path any more, so the target stops producing it.
    with_image_binding(
        &fixture,
        "      image:\n        value: registry.example.com/api@sha256:fixed\n",
    );
    let target_path = fixture.path().join("config/targets/production.yaml");
    let target = fs::read_to_string(&target_path).unwrap();
    let (before, after) = target.split_once("  releaseInputs:\n").unwrap();
    let (_, replacement) = after.split_once("  releaseInputs:\n").unwrap();
    fs::write(&target_path, format!("{before}  releaseInputs:\n{replacement}")).unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Drop publication binding");
    publish_production(&fixture).success();

    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    assert!(commit
        .tree()
        .unwrap()
        .get_path(std::path::Path::new("production/state/images.json"))
        .is_err());
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:fixed"));
}

#[test]
fn test_publish_tree_keeps_carried_state_while_its_group_is_disabled() {
    let (fixture, destination, _seed, _) = publication_fixture();
    // The project cache holds Git worktrees once the branch exists.
    fs::write(fixture.path().join(".gitignore"), ".nyl/\n").unwrap();
    with_publication_binding(
        &fixture,
        "          path: state/images.json\n          pointer: /api\n          carryFileFromWorktree: build/images.json\n",
    );
    fs::create_dir_all(fixture.path().join("build")).unwrap();
    let carried = r#"{"api": "registry.example.com/api@sha256:carried"}"#;
    fs::write(fixture.path().join("build/images.json"), carried).unwrap();
    publish_production(&fixture).success();
    fs::remove_file(fixture.path().join("build/images.json")).unwrap();

    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap();
    fs::write(&group_path, format!("{group}  enabled: false\n")).unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Disable workloads");
    publish_production(&fixture).success();
    let repository = Repository::open_bare(destination.path()).unwrap();
    let commit = published_commit(&repository, "deploy/production");
    assert_eq!(
        published_file(&repository, &commit, "production/state/images.json"),
        carried.as_bytes()
    );

    // Re-enabled, the carried binding adopts the kept file and renders from it.
    fs::write(&group_path, group).unwrap();
    commit_all(&Repository::open(fixture.path()).unwrap(), "Enable workloads");
    publish_production(&fixture).success();
    assert!(published_api_manifests(&destination).contains("image: registry.example.com/api@sha256:carried"));
    let commit = published_commit(&repository, "deploy/production");
    let index: serde_json::Value =
        serde_json::from_slice(&published_file(&repository, &commit, "production/_nyl/index.json")).unwrap();
    assert!(index["files"].get("state/images.json").is_some(), "{index}");
}

#[test]
fn test_publish_tree_rejects_an_owned_state_file_deleted_outside_nyl() {
    let (fixture, _destination, seed, _) = publication_fixture();
    with_publication_binding(
        &fixture,
        "          path: state/images.json\n          pointer: /api\n          carryFileFromWorktree: build/images.json\n",
    );
    fs::create_dir_all(fixture.path().join("build")).unwrap();
    fs::write(
        fixture.path().join("build/images.json"),
        r#"{"api": "registry.example.com/api@sha256:carried"}"#,
    )
    .unwrap();
    publish_production(&fixture).success();

    push_publication_change(&seed, "production/state/images.json", None);
    fs::remove_file(fixture.path().join("build/images.json")).unwrap();
    publish_production(&fixture).failure().stderr(predicate::str::contains(
        "deleted from the publication branch outside Nyl",
    ));
}

#[test]
fn test_render_tree_leaves_publication_inputs_unbound_before_the_branch_exists() {
    let fixture = fixture();
    let (destination, _seed) = seeded_bare_repository();
    fs::write(
        fixture.path().join("config/repositories/deploy.yaml"),
        format!(
            "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: deploy\nspec:\n  repoURL: {}\n",
            destination.path().display()
        ),
    )
    .unwrap();
    with_image_binding(
        &fixture,
        "      image:\n        value: registry.example.com/api@sha256:fixed\n      tag:\n        fromPublication:\n          path: state/tag.json\n",
    );
    render_production(&fixture)
        .success()
        .stderr(predicate::str::contains("does not exist yet"));
    let tree = read_tree(&fixture.path().join("deploy/production"));
    let rendered = tree
        .iter()
        .filter(|(path, _)| path.starts_with("workloads/api"))
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
        .collect::<String>();
    // The Release default applies while the state file does not exist.
    assert!(rendered.contains("tag: none"), "{rendered}");
}
