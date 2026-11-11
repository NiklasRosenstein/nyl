//! Pod template checksums: `k8s.nyl/checksum` on a Pod template makes Nyl write
//! `k8s.nyl/checksum-sha256`, derived from the ConfigMaps and Secrets the
//! Release renders, the same way for every command.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use git2::Repository;
use serde_json::Value;
use tempfile::TempDir;

const CHECKSUM: &str = "k8s.nyl/checksum";
const CHECKSUM_SHA256: &str = "k8s.nyl/checksum-sha256";

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

/// Render `release` in a fresh project and return the standard output.
fn render(release: &str) -> String {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nyl.toml"), "").unwrap();
    fs::write(temp.path().join("release.yaml"), release).unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(render_args("release.yaml"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn checksum_of(rendered: &str) -> String {
    let annotations = deployment_annotations(&documents(rendered));
    annotations[CHECKSUM_SHA256].as_str().unwrap().to_owned()
}

#[test]
fn render_is_stable_and_the_checksum_follows_the_data_alone() {
    for checksum in ["ConfigMap/glance-config#glance.yml", "ConfigMap/glance-config", "auto"] {
        let release = glance_release(DATA, LABELS, checksum);
        let first = render(&release);
        assert_eq!(render(&release), first, "{checksum}: the output is stable");

        let annotations = deployment_annotations(&documents(&first));
        assert_eq!(annotations[CHECKSUM], checksum, "the annotation is kept as written");
        let digest = annotations[CHECKSUM_SHA256].as_str().unwrap();
        assert_eq!(digest.len(), 64, "{digest}");
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()), "{digest}");

        // Metadata, and the order and formatting of keys, are not content.
        let reordered = "  theme.css: \"body {}\"\n  glance.yml: |\n    server:\n      port: 8080";
        let relabelled = "    team: other\n    app: glance\n    extra: label";
        assert_eq!(
            checksum_of(&render(&glance_release(DATA, relabelled, checksum))),
            digest,
            "{checksum}: labels"
        );
        assert_eq!(
            checksum_of(&render(&glance_release(reordered, LABELS, checksum))),
            digest,
            "{checksum}: key order and quoting"
        );
        let stripped = "  glance.yml: |-\n    server:\n      port: 8080\n  theme.css: 'body {}'";
        assert_ne!(
            checksum_of(&render(&glance_release(stripped, LABELS, checksum))),
            digest,
            "{checksum}: a value's trailing newline is content"
        );

        // Only the data changes the checksum, and only the selected data.
        let edited = "  glance.yml: |\n    server:\n      port: 9090\n  theme.css: 'body {}'";
        assert_ne!(
            checksum_of(&render(&glance_release(edited, LABELS, checksum))),
            digest,
            "{checksum}: glance.yml"
        );
        let theme = "  glance.yml: |\n    server:\n      port: 8080\n  theme.css: 'body { color: red }'";
        let theme_checksum = checksum_of(&render(&glance_release(theme, LABELS, checksum)));
        assert_eq!(
            theme_checksum == digest,
            checksum.ends_with("#glance.yml"),
            "{checksum}: theme.css is hashed unless a key was selected"
        );
    }
}

#[test]
fn render_reports_an_unresolved_reference_with_its_source_and_fix() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("nyl.toml"), "").unwrap();
    fs::write(
        temp.path().join("release.yaml"),
        glance_release(DATA, LABELS, "ConfigMap/glance-settings"),
    )
    .unwrap();
    let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
        .current_dir(temp.path())
        .args(render_args("release.yaml"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "Source: release.yaml (document 2)",
        "Resource: apps/v1 Deployment glance\nCaused by: Invalid Deployment: NYL-KUBERNETES-CHECKSUM-NOT-FOUND",
        "NYL-KUBERNETES-CHECKSUM-NOT-FOUND",
        "ConfigMap/glance-settings",
        "Hint:",
    ] {
        assert!(stderr.contains(expected), "{expected} is missing:\n{stderr}");
    }
}

/// A Kubernetes GitOps project with one target that renders `release` as
/// `applications/workloads/glance.yaml`.
fn gitops_project(release: &str) -> TempDir {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    Repository::init(root).unwrap();
    fs::write(root.join("nyl.toml"), "").unwrap();
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
        (
            "config/application-groups/workloads.yaml",
            "apiVersion: k8s.gitops.nyl/v1\nkind: ApplicationGroup\nmetadata:\n  name: workloads\n  labels:\n    \
             environment: production\nspec:\n  projectRef: workloads\n  applicationNamespace: argocd\n",
        ),
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

#[test]
fn render_and_render_tree_write_the_same_checksum_cold_and_from_the_cache() {
    let release = glance_release(DATA, LABELS, "auto");
    let project = gitops_project(&release);
    let root = project.path();

    let plain = checksum_of(&render(&release));
    assert_eq!(plain.len(), 64);

    // The first run renders; the second is served from the render cache, which
    // stores the checksum with the rendered Release.
    for run in ["cold", "cached"] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .current_dir(root)
            .args(["render", "--target", "production", "applications/workloads/glance.yaml"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let targeted = checksum_of(&String::from_utf8(output.stdout).unwrap());
        assert_eq!(targeted, plain, "render with a target, {run}");

        let directory = format!("tree-{run}");
        Command::new(assert_cmd::cargo::cargo_bin!("nyl"))
            .current_dir(root)
            .args(["render-tree", ".", "--target", "production", "--output-dir", &directory])
            .assert()
            .success();
        let tree = read_release_documents(&root.join(&directory).join("production/workloads/glance"));
        let published = deployment_annotations(&tree)[CHECKSUM_SHA256]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(published, plain, "render-tree, {run}");
    }
}
