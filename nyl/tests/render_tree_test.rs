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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        "# Nyl-Provenance: Source: applications/workloads/api.yaml (document 1)\n# Nyl-Provenance: Resource: gitops.nyl/v1 Release api/api\n# Nyl-Provenance: Generated: Namespace \"api\" for Release \"api\""
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
        .env("RUST_LOG", "nyl::gitops::tree=debug")
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
        r#"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: helm
  namespace: helm
---
apiVersion: components.nyl.niklasrosenstein.github.com/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        .env("RUST_LOG", "nyl::render::session=debug")
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
        r#"apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: helm
  namespace: helm
---
apiVersion: components.nyl.niklasrosenstein.github.com/v1
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
        .env("RUST_LOG", "nyl::helm::template=debug")
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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
            "production\tkasoku\tdeploy@deploy/production\tproduction",
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r"apiVersion: gitops.nyl/v1
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
        r#"apiVersion: gitops.nyl/v1
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
        .stderr(predicate::str::contains(
            "Rendered tree comparison\n  Deployment target   production\n  View                entire rendered tree\n  Desired source",
        ))
        .stderr(predicate::str::contains(format!(
            "    Repository        https://example.invalid/source.git\n    Commit            {source_commit}\n    Working tree      clean"
        )))
        .stderr(predicate::str::contains("  Published baseline"))
        .stderr(predicate::str::contains("    Revision          deploy/production"))
        .stderr(predicate::str::contains(format!("    Commit            {}", commit.id())))
        .stderr(predicate::str::contains("    Path              production"))
        .stderr(predicate::str::contains("  Diff output         stdout\n\n"))
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
        .stderr(predicate::str::contains("  Desired publication\n    Repository"))
        .stderr(predicate::str::contains("  Baseline publication\n    Repository"));

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
        .stderr(predicate::str::contains("Rendered differences: 1 file(s)"));

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
        .stderr(predicate::str::contains("Wrote rendered diff"));
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
