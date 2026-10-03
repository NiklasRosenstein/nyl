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

#[test]
fn test_schema_annotate_prefers_vendored_builtin_schemas_over_the_pinned_url() {
    let directory = project();
    let configmap = directory.path().join("apps/web/configmap.yaml");
    fs::write(&configmap, "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\n").unwrap();

    // Without a vendored copy, the comment uses kubeconform's pinned URL for the newest Cluster version.
    annotate(directory.path(), false).success();
    let remote = fs::read_to_string(&configmap).unwrap();
    let url = remote
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("# yaml-language-server: $schema="))
        .unwrap()
        .to_owned();
    assert!(url.starts_with("https://raw.githubusercontent.com/yannh/kubernetes-json-schema/"));
    assert!(url.ends_with("/v1.31.4-standalone-strict/configmap-v1.json"));

    // A vendored built-in becomes a local, template-friendly copy.
    let vendor = directory.path().join("vendor");
    let digest = store::write_blob(
        &vendor,
        &store::json_bytes(&json!({"type": "object", "properties": {"immutable": {"type": "boolean"}}})).unwrap(),
    )
    .unwrap();
    store::atomic_write(
        &vendor.join("schemas/builtins.json"),
        &store::json_bytes(&json!({"version": 1, "schemas": {url: digest}})).unwrap(),
    )
    .unwrap();
    annotate(directory.path(), false).success();
    assert!(fs::read_to_string(&configmap).unwrap().starts_with(
        "# yaml-language-server: $schema=../../vendor/schemas/editor/builtins/v1.31.4/configmap-v1.json\n"
    ));
    let schema: serde_json::Value = serde_json::from_slice(
        &fs::read(
            directory
                .path()
                .join("vendor/schemas/editor/builtins/v1.31.4/configmap-v1.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(schema["properties"]["immutable"]["anyOf"][0]["type"], "boolean");
}

#[test]
fn test_schema_annotate_local_mode_keeps_builtin_comments_local_and_offline() {
    let directory = project();
    let configmap = directory.path().join("apps/web/configmap.yaml");
    fs::write(&configmap, "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\n").unwrap();
    let secret = directory.path().join("apps/web/secret.yaml");
    fs::write(&secret, "apiVersion: v1\nkind: Secret\nmetadata:\n  name: web\n").unwrap();
    // Vendored mode references the pinned strict URLs of schemas that are not vendored.
    annotate(directory.path(), false).success();
    let pinned_url = |path: &Path| {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("# yaml-language-server: $schema="))
            .unwrap()
            .to_owned()
    };
    let (configmap_url, secret_url) = (pinned_url(&configmap), pinned_url(&secret));
    fs::write(directory.path().join("nyl.toml"), "").unwrap();

    // Validation configured as non-strict vendors only the non-strict copy; editors still use it.
    let vendor = directory.path().join("vendor");
    let digest = store::write_blob(
        &vendor,
        &store::json_bytes(&json!({"type": "object", "properties": {"immutable": {"type": "boolean"}}})).unwrap(),
    )
    .unwrap();
    store::atomic_write(
        &vendor.join("schemas/builtins.json"),
        &store::json_bytes(&json!({"version": 1, "schemas": {configmap_url.replace("-strict/", "/"): digest}}))
            .unwrap(),
    )
    .unwrap();
    // A schema that is not vendored comes from the download cache, so this needs no network.
    let downloaded = json!({"type": "object", "properties": {"cached": {"type": "boolean"}}});
    let cache = directory
        .path()
        .join(".nyl/cache/validation-schemas")
        .join(format!("{}.json", nyl_core::digest::sha256_hex(secret_url.as_bytes())));
    store::atomic_write(
        &cache,
        &store::json_bytes(&json!({
            "digest": nyl_core::digest::sha256_hex(&store::json_bytes(&downloaded).unwrap()),
            "value": downloaded,
        }))
        .unwrap(),
    )
    .unwrap();

    annotate(directory.path(), false).success();
    // Both comments are local paths, whichever source filled them.
    for (path, file, property) in [
        (&configmap, "configmap-v1.json", "immutable"),
        (&secret, "secret-v1.json", "cached"),
    ] {
        assert!(fs::read_to_string(path).unwrap().starts_with(&format!(
            "# yaml-language-server: $schema=../../.nyl/schemas/builtins/v1.31.4/{file}\n"
        )));
        let schema: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.path().join(".nyl/schemas/builtins/v1.31.4").join(file)).unwrap(),
        )
        .unwrap();
        assert_eq!(schema["properties"][property]["anyOf"][0]["type"], "boolean");
    }

    // Checking compares comments only, so a fresh checkout without schemas passes.
    fs::remove_dir_all(directory.path().join(".nyl/schemas")).unwrap();
    annotate(directory.path(), true).success();
}
