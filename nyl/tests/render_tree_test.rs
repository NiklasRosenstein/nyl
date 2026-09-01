use std::fs;

use assert_cmd::Command;
use git2::Repository;
use predicates::prelude::*;
use tempfile::TempDir;

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
kind: GitOpsTarget
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
  manifest:
    apiVersion: argoproj.io/v1alpha1
    kind: AppProject
    metadata:
      name: workloads
      namespace: argocd
    spec:
      sourceRepos:
        - https://example.invalid/deploy.git
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
            "--target",
            "production",
            "--output-dir",
            output.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Rendered GitOps target production"));

    let root = output.join("production");
    let resources = fs::read_to_string(root.join("workloads/api/resources.yaml")).unwrap();
    assert!(resources.contains("kind: ConfigMap"));
    assert!(resources.contains("kind: Namespace"));
    assert!(resources.contains("Delete=confirm,Prune=confirm"));
    assert!(!root.join("_nyl/namespaces").exists());

    let application = fs::read_to_string(root.join("_nyl/catalog/applications/argocd-production/api.yaml")).unwrap();
    assert!(application.contains("targetRevision: deploy/production"));
    assert!(application.contains("path: production/workloads/api"));
    assert!(application.contains("recurse: true"));
    assert!(!application.contains("plugin:"));
    assert!(application.contains("resources-finalizer.argocd.argoproj.io"));
    assert!(application.contains("environment: production"));
    assert!(application.contains("server: https://kubernetes.default.svc"));
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
    assert!(catalog.contains("enabled: true"));
    assert!(catalog.contains("prune: false"));
    assert!(catalog.contains("selfHeal: true"));
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

    // A byte-identical second render is accepted and keeps ownership stable.
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
        .args(["validate", "gitops"])
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
kind: GitOpsTarget
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
        .args(["validate", "gitops"])
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
        .args(["target", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "production\tkasoku\tdeploy@deploy/production\tproduction",
        ));

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["validate", "gitops"])
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
kind: GitOpsTarget
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
        .args(["validate", "gitops"])
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
kind: GitOpsTarget
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
  publishURL: https://example.invalid/deploy.git
",
    )
    .unwrap();
    fs::write(
        fixture.path().join("config/targets/overlap.yaml"),
        r"apiVersion: gitops.nyl/v1
kind: GitOpsTarget
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
        .args(["validate", "gitops"])
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
        .stderr(predicate::str::contains("outside its allowed namespaces"));
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
fn release_include_adds_plain_manifest_to_rendered_tree() {
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
    assert!(resources.contains("name: included"));
}

#[test]
fn publishes_a_new_publication_branch_with_cas_workflow() {
    let fixture = fixture();
    let (destination, _seed) = seeded_bare_repository();
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
    commit_all(&source, "Source");

    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["publish-tree", "--target", "production"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Published GitOps target production"));

    let destination_repository = Repository::open_bare(destination.path()).unwrap();
    let commit = destination_repository
        .find_reference("refs/heads/deploy/production")
        .unwrap()
        .peel_to_commit()
        .unwrap();
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
        .args(["diff-tree", "--target", "production", "--against", "published"])
        .assert()
        .success()
        .stdout(predicate::str::contains("has no rendered differences"));

    let application_source = fixture.path().join("applications/workloads/api.yaml");
    let changed = fs::read_to_string(&application_source)
        .unwrap()
        .replace("environment: '{{ values.environment }}'", "environment: changed");
    fs::write(application_source, changed).unwrap();
    Command::cargo_bin("nyl")
        .unwrap()
        .current_dir(fixture.path())
        .args(["diff-tree", "--target", "production", "--against", "published"])
        .assert()
        .success()
        .stdout(predicate::str::contains("+  environment: changed"));
}
