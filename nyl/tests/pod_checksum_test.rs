//! Pod template checksums: `k8s.nyl/checksum` on a Pod template makes Nyl write
//! `k8s.nyl/checksum-sha256`, derived from the ConfigMaps and Secrets the
//! Release renders, the same way for every command. The rules themselves are
//! tested beside their implementation; these tests cover the commands.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use git2::Repository;
use serde_json::Value;
use tempfile::TempDir;

const CHECKSUM: &str = "k8s.nyl/checksum";
const CHECKSUM_SHA256: &str = "k8s.nyl/checksum-sha256";
const KEY_ONLY: &str = "ConfigMap/glance-config#glance.yml";

/// A Release holding a ConfigMap and the Deployment that mounts it.
fn glance_release(config_data: &str, config_labels: &str, checksum: &str) -> String {
    format!(
        r#"apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: glance
  namespace: glance
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: glance
spec:
  selector:
    matchLabels:
      app: glance
  template:
    metadata:
      labels:
        app: glance
      annotations:
        {CHECKSUM}: "{checksum}"
    spec:
      containers:
        - name: glance
          image: glanceapp/glance:v0.7.0
      volumes:
        - name: config
          configMap:
            name: glance-config
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: glance-config
  labels:
{config_labels}
data:
{config_data}
"#
    )
}

const LABELS: &str = "    app: glance\n    team: platform";
const DATA: &str = "  glance.yml: |\n    server:\n      port: 8080\n  theme.css: 'body {}'";

fn render_args(file: &str) -> Vec<&str> {
    vec![
        "render",
        "--offline",
        "--kube-version",
        "1.31.0",
        "--kube-api-versions",
        "v1,apps/v1",
        file,
    ]
}

fn documents(yaml: &str) -> Vec<Value> {
    serde_saphyr::from_multiple::<Value>(yaml).unwrap()
}

fn deployment_annotations(documents: &[Value]) -> Value {
    documents
        .iter()
        .find(|document| document["kind"] == "Deployment")
        .expect("a Deployment is rendered")
        .pointer("/spec/template/metadata/annotations")
        .expect("the Pod template has annotations")
        .clone()
}

/// A project holding `release.yaml`.
fn project(release: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nyl.toml"), "").unwrap();
    fs::write(temp.path().join("release.yaml"), release).unwrap();
    temp
}

fn render_in(project: &Path, args: &[&str]) -> std::process::Output {
    Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(project)
        .args(args)
        .output()
        .unwrap()
}

/// Render `release` in a fresh project and return the standard output.
fn render(release: &str) -> String {
    let project = project(release);
    let output = render_in(project.path(), &render_args("release.yaml"));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn checksum_of(rendered: &str) -> String {
    let annotations = deployment_annotations(&documents(rendered));
    annotations[CHECKSUM_SHA256].as_str().unwrap().to_owned()
}

#[test]
fn render_is_stable_and_the_checksum_follows_the_selected_data_alone() {
    let release = glance_release(DATA, LABELS, KEY_ONLY);
    let first = render(&release);
    assert_eq!(render(&release), first, "the output is stable");
    let annotations = deployment_annotations(&documents(&first));
    assert_eq!(annotations[CHECKSUM], KEY_ONLY, "the annotation is kept as written");
    let digest = checksum_of(&first);
    assert_eq!(digest.len(), 64, "{digest}");

    let digest_for = |data: &str, labels: &str| checksum_of(&render(&glance_release(data, labels, KEY_ONLY)));

    // Metadata, and the order, quoting, and block style of keys, are not content.
    assert_eq!(digest_for(DATA, "    team: other\n    extra: label"), digest, "labels");
    let reordered = "  theme.css: \"body {}\"\n  glance.yml: |\n    server:\n      port: 8080";
    assert_eq!(digest_for(reordered, LABELS), digest, "key order and quoting");
    let folded = "  theme.css: 'body {}'\n  glance.yml: \"server:\\n  port: 8080\\n\"";
    assert_eq!(digest_for(folded, LABELS), digest, "block scalar versus quoted string");

    // The selected value is content, to the byte.
    let edited = "  glance.yml: |\n    server:\n      port: 9090\n  theme.css: 'body {}'";
    assert_ne!(digest_for(edited, LABELS), digest, "glance.yml");
    let stripped = "  glance.yml: |-\n    server:\n      port: 8080\n  theme.css: 'body {}'";
    assert_ne!(digest_for(stripped, LABELS), digest, "a trailing newline");

    // Other keys are not selected.
    let theme = "  glance.yml: |\n    server:\n      port: 8080\n  theme.css: 'body { color: red }'";
    assert_eq!(digest_for(theme, LABELS), digest, "theme.css");
}

#[test]
fn render_reports_an_unresolved_reference_with_its_source() {
    let project = project(&glance_release(DATA, LABELS, "ConfigMap/glance-settings"));
    let output = render_in(project.path(), &render_args("release.yaml"));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "Source: release.yaml (document 2)\nResource: apps/v1 Deployment glance\n\
             Caused by: NYL-KUBERNETES-CHECKSUM-NOT-FOUND"
        ),
        "{stderr}"
    );
}

#[test]
fn a_partial_render_fails_instead_of_dropping_the_digest() {
    let project = project(&glance_release(DATA, LABELS, "auto"));
    let mut args = render_args("release.yaml");
    args.extend(["--only-source-kind", "Deployment"]);
    let output = render_in(project.path(), &args);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("NYL-KUBERNETES-CHECKSUM-PARTIAL"), "{stderr}");
}

/// A Kubernetes GitOps project with one target that renders `release` as
/// `applications/workloads/glance.yaml`; `group_spec` adds ApplicationGroup fields.
fn gitops_project(release: &str, group_spec: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    Repository::init(root).unwrap();
    fs::write(root.join("nyl.toml"), "").unwrap();
    let group = format!(
        "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: workloads\n  labels:\n    \
         environment: production\nspec:\n  projectRef: workloads\n  applicationNamespace: argocd\n{group_spec}"
    );
    let files = [
        (
            "config/repositories/deploy.yaml",
            "apiVersion: gitops.nyl/v1\nkind: GitRepository\nmetadata:\n  name: deploy\nspec:\n  \
             repoURL: https://example.invalid/deploy.git\n  publishURL: ssh://git@example.invalid/deploy.git\n",
        ),
        (
            "config/clusters/prod.yaml",
            "apiVersion: k8s.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  destination:\n    \
             server: https://kubernetes.default.svc\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions:\n      \
             - v1\n      - apps/v1\n      - v1/ConfigMap\n      - v1/Namespace\n      - apps/v1/Deployment\n    \
             clusterScopedKinds:\n      - core/Namespace\n",
        ),
        (
            "config/targets/production.yaml",
            "apiVersion: k8s.gitops.nyl/v1\nkind: DeploymentTarget\nmetadata:\n  name: production\n  labels:\n    \
             environment: production\nspec:\n  reconciler: {argocd: {}}\n  clusterRef:\n    name: prod\n  \
             applicationGroupSelector:\n    matchLabels:\n      environment: production\n  publication:\n    \
             repositoryRef:\n      name: deploy\n    revision: deploy/production\n    pathPrefix: production\n",
        ),
        (
            "config/projects/workloads.yaml",
            "apiVersion: k8s.gitops.nyl/v1\nkind: AppProjectDefinition\nmetadata:\n  name: workloads\nspec:\n  \
             management: Rendered\n  sourceRepositoryRefs:\n    - name: deploy\n  manifest:\n    \
             apiVersion: argoproj.io/v1alpha1\n    kind: AppProject\n    metadata:\n      name: workloads\n      \
             namespace: argocd\n    spec:\n      sourceRepos:\n        - https://example.invalid/deploy.git\n      \
             destinations:\n        - server: https://kubernetes.default.svc\n          namespace: '*'\n",
        ),
        ("config/application-groups/workloads.yaml", group.as_str()),
        ("applications/workloads/glance.yaml", release),
    ];
    for (path, content) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    temp
}

fn read_release_documents(directory: &Path) -> Vec<Value> {
    let mut paths = walkdir::WalkDir::new(directory)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .iter()
        .flat_map(|path| documents(&fs::read_to_string(path).unwrap()))
        .collect()
}

/// The checksum `render --target` and `render-tree` write for the project's
/// Release, rendering `render-tree` twice to cover the render cache.
fn target_checksums(project: &Path) -> Vec<(&'static str, String)> {
    let mut checksums = Vec::new();
    // Taken from the compiled target, and rendered alone with the group's namespace.
    for (name, flag) in [
        ("render with a target", None),
        ("render --release-only", Some("--release-only")),
    ] {
        let mut args = vec!["render", "--target", "production", "--no-cache"];
        args.extend(flag);
        args.push("applications/workloads/glance.yaml");
        let output = render_in(project, &args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        checksums.push((name, checksum_of(&String::from_utf8(output.stdout).unwrap())));
    }

    for directory in ["tree-cold", "tree-cached"] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .current_dir(project)
            .env("RUST_LOG", "nyl_render::gitops::tree=debug")
            .args(["render-tree", ".", "--target", "production", "--output-dir", directory])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let reused = String::from_utf8_lossy(&output.stderr).contains("Reusing cached deployment target tree");
        assert_eq!(reused, directory == "tree-cached", "{directory}: cache use");
        let tree = read_release_documents(&project.join(directory).join("production/workloads/glance"));
        let name = if directory == "tree-cold" {
            "render-tree"
        } else {
            "render-tree from the cache"
        };
        checksums.push((
            name,
            deployment_annotations(&tree)[CHECKSUM_SHA256]
                .as_str()
                .unwrap()
                .to_owned(),
        ));
    }
    checksums
}

#[test]
fn render_and_render_tree_write_the_same_checksum_cold_and_from_the_cache() {
    let release = glance_release(DATA, LABELS, "auto");
    let project = gitops_project(&release, "");
    let plain = checksum_of(&render(&release));

    for (name, checksum) in target_checksums(project.path()) {
        assert_eq!(checksum, plain, "{name}");
    }
}

#[test]
fn references_resolve_in_the_groups_destination_namespace() {
    // The group deploys the Release (namespace `glance`) to `prod`, so the
    // ConfigMap that names `prod` is the one the Deployment, which names none, mounts.
    let release = glance_release(DATA, LABELS, "auto").replace(
        "\nmetadata:\n  name: glance-config\n",
        "\nmetadata:\n  name: glance-config\n  namespace: prod\n",
    );
    let project = gitops_project(&release, "  destinationNamespace: prod\n");
    let expected = checksum_of(&render(&glance_release(DATA, LABELS, "auto")));

    for (name, checksum) in target_checksums(project.path()) {
        assert_eq!(checksum, expected, "{name}");
    }
}
