use std::fs;

use assert_cmd::Command;
use git2::Repository;
use predicates::prelude::*;
use tempfile::TempDir;

fn fixture() -> TempDir {
    let temp = TempDir::new().unwrap();
    Repository::init(temp.path()).unwrap();
    fs::write(
        temp.path().join("nyl.toml"),
        "[profile.production.values]\nenvironment = \"production\"\n",
    )
    .unwrap();
    for directory in [
        "config/repositories",
        "config/targets",
        "config/projects",
        "config/application-groups",
        "applications/workloads",
    ] {
        fs::create_dir_all(temp.path().join(directory)).unwrap();
    }
    fs::write(
        temp.path().join("config/repositories/deploy.yaml"),
        r#"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitRepository
metadata:
  name: deploy
spec:
  repoURL: https://example.invalid/deploy.git
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/targets/production.yaml"),
        r#"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: production
  labels:
    environment: production
spec:
  profile: production
  destination:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: production
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("config/projects/workloads.yaml"),
        r#"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
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
        r#"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGroup
metadata:
  name: workloads
spec:
  targetSelector:
    matchLabels:
      environment: production
  projectRef: workloads
  applicationNamespace: 'argocd-{{ target.labels.environment }}'
  destination:
    server: https://kubernetes.default.svc
{% if target.labels.environment == 'production' %}
  annotations:
    environment: production
{% endif %}
"#,
    )
    .unwrap();
    fs::write(
        temp.path().join("applications/workloads/api.yaml"),
        r#"apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
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
    assert!(!resources.contains("kind: Namespace"));
    let namespace_directory = fs::read_dir(root.join("_nyl/namespaces"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let namespace = fs::read_to_string(namespace_directory.join("resources.yaml")).unwrap();
    assert!(namespace.contains("kind: Namespace"));
    assert!(namespace.contains("Delete=confirm,Prune=confirm"));

    let application = fs::read_to_string(root.join("_nyl/catalog/applications/argocd-production/api.yaml")).unwrap();
    assert!(application.contains("targetRevision: deploy/production"));
    assert!(application.contains("path: production/workloads/api"));
    assert!(application.contains("recurse: true"));
    assert!(!application.contains("plugin:"));
    assert!(application.contains("resources-finalizer.argocd.argoproj.io"));
    assert!(application.contains("environment: production"));
    assert!(fs::read_dir(root.join("_nyl/catalog/applications/argocd-production"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().starts_with("nyl-namespace-")));

    assert!(root.join("_nyl/catalog/projects/workloads.yaml").is_file());
    assert!(root.join("_nyl/index.json").is_file());

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
            "production\tproduction\tdeploy@deploy/production\tproduction",
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
fn validation_rejects_overlapping_target_prefixes_on_one_revision() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/targets/overlap.yaml"),
        r"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: overlap
spec:
  profile: production
  destination:
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
        .stderr(predicate::str::contains("overlapping path prefixes"));
}

#[test]
fn operational_commands_reject_overlaps_through_repository_aliases() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/repositories/deploy-alias.yaml"),
        r"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
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
        r"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: overlap
spec:
  profile: production
  destination:
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
        .stderr(predicate::str::contains("overlapping path prefixes"));
}

#[test]
fn validation_rejects_overlaps_through_publish_urls_and_branch_ref_aliases() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/repositories/deploy-publisher.yaml"),
        r"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
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
        r"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: overlap
spec:
  profile: production
  destination:
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
        .stderr(predicate::str::contains("overlapping path prefixes"));
}

#[test]
fn target_rejects_mixed_cluster_identity_representations() {
    let fixture = fixture();
    fs::write(
        fixture.path().join("config/application-groups/named-cluster.yaml"),
        r"apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
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
        .stderr(predicate::str::contains(
            "mixes ApplicationGroup destination.server and destination.name",
        ));
}

#[test]
fn one_dedicated_application_owns_a_shared_namespace() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "    server: https://kubernetes.default.svc\n",
        "    server: https://kubernetes.default.svc\n    namespace: shared\n",
    );
    fs::write(group_path, group).unwrap();
    fs::write(
        fixture.path().join("applications/workloads/worker.yaml"),
        r"apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
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
fn broad_release_policy_cannot_override_platform_owned_application_fields() {
    let fixture = fixture();
    let group_path = fixture.path().join("config/application-groups/workloads.yaml");
    let group = fs::read_to_string(&group_path).unwrap().replace(
        "{% if target.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedPaths: ['spec.**']\n{% if target.labels.environment == 'production' %}",
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
        "{% if target.labels.environment == 'production' %}",
        "  releaseCustomization:\n    allowedPaths: ['spec.**']\n{% if target.labels.environment == 'production' %}",
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
        .stderr(predicate::str::contains("outside its destination namespace"));
}

#[test]
fn publishes_a_new_destination_branch_with_cas_workflow() {
    let fixture = fixture();
    let (destination, _seed) = seeded_bare_repository();
    fs::write(
        fixture.path().join("config/repositories/deploy.yaml"),
        format!(
            "apiVersion: gitops.nyl.niklasrosenstein.github.com/v1\nkind: GitRepository\nmetadata:\n  name: deploy\nspec:\n  repoURL: {}\n  publishURL: {}\n",
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
