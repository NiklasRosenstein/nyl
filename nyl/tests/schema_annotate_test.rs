use std::fs;
use std::path::Path;

use assert_cmd::Command;
use nyl::validation::store;
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

const CLUSTER: &str = "apiVersion: k8s.gitops.nyl/v1\nkind: Cluster\nmetadata:\n  name: prod\nspec:\n  destination:\n    server: https://kubernetes.default.svc\n  kubernetes:\n    kubeVersion: 1.31.4\n    apiVersions: [v1]\n";

/// A project with vendored editor schemas and a vendored Widget CRD.
fn project() -> TempDir {
    let directory = TempDir::new().unwrap();
    git2::Repository::init(directory.path()).unwrap();
    fs::write(directory.path().join("nyl.toml"), "[editor]\nschemas = \"vendored\"\n").unwrap();
    fs::create_dir_all(directory.path().join("config")).unwrap();
    fs::write(directory.path().join("config/cluster.yaml"), CLUSTER).unwrap();
    fs::create_dir_all(directory.path().join("apps/web")).unwrap();
    fs::write(
        directory.path().join("apps/web/widget.yaml"),
        "apiVersion: example.com/v1\nkind: Widget\nmetadata:\n  name: web\nspec:\n  count: 1\n---\napiVersion: unknown.example/v1\nkind: Gadget\n",
    )
    .unwrap();
    // A Helm chart's templates belong to the chart, not the project.
    fs::create_dir_all(directory.path().join("charts/web/templates")).unwrap();
    fs::write(
        directory.path().join("charts/web/Chart.yaml"),
        "apiVersion: v2\nname: web\nversion: 0.1.0\n",
    )
    .unwrap();
    fs::write(
        directory.path().join("charts/web/templates/configmap.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\n",
    )
    .unwrap();

    let vendor = directory.path().join("vendor");
    let crd = json!({
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.example.com"},
        "spec": {"group": "example.com", "scope": "Namespaced", "names": {"kind": "Widget"},
            "versions": [{"name": "v1", "served": true, "storage": true, "schema": {"openAPIV3Schema": {
                "type": "object", "properties": {"spec": {"type": "object", "properties": {"count": {"type": "integer"}}}}}}}]}
    });
    let capabilities = nyl::resources::ClusterKubernetesCapabilities {
        kube_version: Some("1.31.4".into()),
        api_versions: vec!["v1".into()],
        cluster_scoped_kinds: Vec::new(),
        vendored_crds: false,
    };
    let (index, blobs) = store::prepare_capture("prod", &capabilities, &[crd]).unwrap();
    for bytes in blobs.values() {
        store::write_blob(&vendor, bytes).unwrap();
    }
    store::atomic_write(
        &store::cluster_index_path(&vendor, "prod").unwrap(),
        &index.to_bytes().unwrap(),
    )
    .unwrap();

    let repository = git2::Repository::open(directory.path()).unwrap();
    let mut git_index = repository.index().unwrap();
    git_index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    git_index.write().unwrap();
    directory
}

fn annotate(directory: &Path, check: bool) -> assert_cmd::assert::Assert {
    let mut command = Command::cargo_bin("nyl").unwrap();
    command.current_dir(directory).args(["schema", "annotate"]);
    if check {
        command.arg("--check");
    }
    command.assert()
}

#[test]
fn test_schema_annotate_points_documents_at_vendored_schemas_and_checks_drift() {
    let directory = project();
    annotate(directory.path(), true)
        .failure()
        .stderr(predicate::str::contains("apps/web/widget.yaml"));

    annotate(directory.path(), false).success();
    let widget = fs::read_to_string(directory.path().join("apps/web/widget.yaml")).unwrap();
    // Each document gets its own comment; unknown kinds are left alone.
    assert_eq!(
        widget,
        "# yaml-language-server: $schema=../../vendor/schemas/editor/crds/example.com/Widget_v1.json\napiVersion: example.com/v1\nkind: Widget\nmetadata:\n  name: web\nspec:\n  count: 1\n---\napiVersion: unknown.example/v1\nkind: Gadget\n"
    );
    let cluster = fs::read_to_string(directory.path().join("config/cluster.yaml")).unwrap();
    assert!(cluster.starts_with(
        "# yaml-language-server: $schema=../vendor/schemas/editor/nyl/k8s.gitops.nyl/v1/cluster.schema.json\n"
    ));
    // The vendored CRD schema accepts template expressions for its scalars.
    let schema: serde_json::Value = serde_json::from_slice(
        &fs::read(
            directory
                .path()
                .join("vendor/schemas/editor/crds/example.com/Widget_v1.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        schema["properties"]["spec"]["properties"]["count"]["anyOf"][0]["type"],
        "integer"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("charts/web/templates/configmap.yaml")).unwrap(),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\n"
    );

    annotate(directory.path(), true).success();
    // A stale vendored schema is drift too.
    fs::write(
        directory
            .path()
            .join("vendor/schemas/editor/crds/example.com/Widget_v1.json"),
        "{}",
    )
    .unwrap();
    annotate(directory.path(), true)
        .failure()
        .stderr(predicate::str::contains("Widget_v1.json"));
}

/// The pinned kubeconform URL of a core/v1 kind for the fixture's Kubernetes version.
fn pinned_url(kind: &str, strict: bool) -> String {
    let settings = nyl::validation::KubeconformSettings {
        strict,
        ..Default::default()
    };
    nyl::validation::builtin_url(&settings, &format!("v1/{kind}"), "1.31.4").unwrap()
}

/// Make built-in schemas available without the network: ConfigMap as a vendored
/// non-strict copy, as validation with `strict = false` stores it, and Secret
/// in the download cache. Their schemas carry a marker property naming the source.
fn provide_builtin_schemas(directory: &Path) {
    let vendor = directory.join("vendor");
    let vendored = json!({"type": "object", "properties": {"vendored": {"type": "boolean"}}});
    let digest = store::write_blob(&vendor, &store::json_bytes(&vendored).unwrap()).unwrap();
    store::atomic_write(
        &vendor.join("schemas/builtins.json"),
        &store::json_bytes(&json!({"version": 1, "schemas": {pinned_url("ConfigMap", false): digest}})).unwrap(),
    )
    .unwrap();
    let downloaded = json!({"type": "object", "properties": {"downloaded": {"type": "boolean"}}});
    let cache = directory.join(".nyl/cache/validation-schemas").join(format!(
        "{}.json",
        nyl_core::digest::sha256_hex(pinned_url("Secret", true).as_bytes())
    ));
    store::atomic_write(
        &cache,
        &store::json_bytes(&json!({
            "digest": nyl_core::digest::sha256_hex(&store::json_bytes(&downloaded).unwrap()),
            "value": downloaded,
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        directory.join("apps/web/configmap.yaml"),
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\n",
    )
    .unwrap();
    fs::write(
        directory.join("apps/web/secret.yaml"),
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: web\n",
    )
    .unwrap();
}

/// Assert both built-in documents point at their local schema copies.
fn assert_builtin_comments(directory: &Path, schema_root: &str) {
    for (kind, file, property) in [
        ("configmap", "configmap-v1.json", "vendored"),
        ("secret", "secret-v1.json", "downloaded"),
    ] {
        let document = fs::read_to_string(directory.join(format!("apps/web/{kind}.yaml"))).unwrap();
        assert!(document.starts_with(&format!(
            "# yaml-language-server: $schema=../../{schema_root}/builtins/v1.31.4/{file}\n"
        )));
        let schema: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join(schema_root).join("builtins/v1.31.4").join(file)).unwrap())
                .unwrap();
        assert_eq!(schema["properties"][property]["anyOf"][0]["type"], "boolean");
    }
}

#[test]
fn test_schema_annotate_vendored_mode_commits_builtin_schemas_instead_of_urls() {
    let directory = project();
    provide_builtin_schemas(directory.path());

    annotate(directory.path(), false).success();
    assert_builtin_comments(directory.path(), "vendor/schemas/editor");

    annotate(directory.path(), true).success();
    // A committed built-in schema that went missing is drift, even though checking cannot download it.
    fs::remove_file(
        directory
            .path()
            .join("vendor/schemas/editor/builtins/v1.31.4/secret-v1.json"),
    )
    .unwrap();
    annotate(directory.path(), true)
        .failure()
        .stderr(predicate::str::contains("secret-v1.json"));
}

#[test]
fn test_schema_annotate_local_mode_keeps_builtin_comments_local_and_offline() {
    let directory = project();
    fs::write(directory.path().join("nyl.toml"), "").unwrap();
    provide_builtin_schemas(directory.path());

    annotate(directory.path(), false).success();
    assert_builtin_comments(directory.path(), ".nyl/schemas");

    // Checking compares comments only, so a fresh checkout without schemas passes.
    fs::remove_dir_all(directory.path().join(".nyl/schemas")).unwrap();
    annotate(directory.path(), true).success();
}
