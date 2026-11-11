//! Helm `lookup` against the local read-only API, with the real `helm` binary.
//!
//! These tests need the Helm version Nyl pins (`mise install helm`), like the
//! other tests that run Helm.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use nyl_render::helm::lookup::{
    EmptyStore, KindInfo, LookupBackend, LookupConfig, LookupHandle, ObjectSet, ObjectStore, ReleaseIdentity,
    ReleaseScope, ScopeSource, StoreError,
};
use nyl_render::helm::{HelmTemplateExecutor, ResolvedChart};
use nyl_render::resources::ChartRef;
use serde_json::{json, Value};
use tempfile::TempDir;

const CA_TEMPLATE: &str = r#"{{- $ca := lookup "v1" "Secret" .Release.Namespace "my-ca" }}
apiVersion: v1
kind: Secret
metadata:
  name: my-ca
type: Opaque
data:
{{- if $ca }}
  tls.crt: {{ index $ca.data "tls.crt" }}
  tls.key: {{ index $ca.data "tls.key" }}
{{- else }}
{{- $gen := genCA "my-ca" 365 }}
  tls.crt: {{ $gen.Cert | b64enc }}
  tls.key: {{ $gen.Key | b64enc }}
{{- end }}
"#;

const RESULTS_TEMPLATE: &str = r#"{{- $cms := lookup "v1" "ConfigMap" .Release.Namespace "" }}
{{- $missing := lookup "v1" "ConfigMap" .Release.Namespace "does-not-exist" }}
{{- $elsewhere := lookup "v1" "ConfigMap" "elsewhere" "" }}
apiVersion: v1
kind: ConfigMap
metadata:
  name: lookup-results
data:
  listNames: {{ $names := list }}{{ range (dig "items" (list) $cms) }}{{ $names = append $names .metadata.name }}{{ end }}{{ $names | join "," | quote }}
  missingIsEmpty: {{ empty $missing | quote }}
  elsewhere: {{ len (dig "items" (list) $elsewhere) | quote }}
  kubeVersion: {{ .Capabilities.KubeVersion.Version | quote }}
  hasFoo: {{ .Capabilities.APIVersions.Has "foo.example.com/v1" | quote }}
  hasPolicyV1: {{ .Capabilities.APIVersions.Has "policy/v1" | quote }}
  hasShipped: {{ .Capabilities.APIVersions.Has "shipped.example.com/v1" | quote }}
"#;

fn chart(directory: &Path, extra_templates: &[(&str, &str)]) -> ResolvedChart {
    std::fs::create_dir_all(directory.join("templates")).unwrap();
    std::fs::create_dir_all(directory.join("crds")).unwrap();
    std::fs::write(
        directory.join("Chart.yaml"),
        "apiVersion: v2\nname: lookup-chart\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(directory.join("templates/ca.yaml"), CA_TEMPLATE).unwrap();
    std::fs::write(directory.join("templates/results.yaml"), RESULTS_TEMPLATE).unwrap();
    std::fs::write(
        directory.join("crds/shipped.yaml"),
        "apiVersion: apiextensions.k8s.io/v1\nkind: CustomResourceDefinition\nmetadata:\n  name: things.shipped.example.com\nspec:\n  group: shipped.example.com\n  names:\n    kind: Thing\n    plural: things\n  scope: Namespaced\n  versions:\n    - name: v1\n      served: true\n      storage: true\n",
    )
    .unwrap();
    for (name, content) in extra_templates {
        std::fs::write(directory.join("templates").join(name), content).unwrap();
    }
    ResolvedChart {
        path: directory.to_path_buf(),
        chart_ref: ChartRef::default(),
    }
}

struct Snapshot(Result<ObjectSet, StoreError>);

struct Held(Result<ObjectSet, StoreError>);

impl Held {
    fn set(&self) -> Result<&ObjectSet, StoreError> {
        self.0.as_ref().map_err(Clone::clone)
    }
}

impl ObjectStore for Held {
    fn kinds(&self) -> Result<Vec<KindInfo>, StoreError> {
        self.set()?.kinds()
    }

    fn get(&self, kind: &KindInfo, namespace: Option<&str>, name: &str) -> Result<Option<Value>, StoreError> {
        self.set()?.get(kind, namespace, name)
    }

    fn list(&self, kind: &KindInfo, namespace: Option<&str>) -> Result<Vec<Value>, StoreError> {
        self.set()?.list(kind, namespace)
    }
}

impl LookupBackend for Snapshot {
    fn describe(&self) -> String {
        "a test snapshot".to_owned()
    }

    fn store(&self, scope: &ReleaseScope) -> Arc<dyn ObjectStore> {
        if scope.is_none() {
            Arc::new(EmptyStore)
        } else {
            Arc::new(Held(self.0.clone()))
        }
    }
}

fn objects(documents: Vec<Value>) -> ObjectSet {
    ObjectSet::from_documents(documents, &|_, _| Some(true), "demo").unwrap()
}

fn snapshot_with_ca() -> ObjectSet {
    objects(vec![
        json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "my-ca", "namespace": "demo", "resourceVersion": "7"},
               "type": "Opaque", "data": {"tls.crt": "U05BUFNIT1QtQ0VSVA==", "tls.key": "U05BUFNIT1QtS0VZ"}}),
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-a", "namespace": "demo"}}),
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-b", "namespace": "demo"}}),
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-other", "namespace": "elsewhere"}}),
    ])
}

fn handle(backend: Snapshot) -> LookupHandle {
    handle_with_config(LookupConfig::new(Arc::new(backend), BTreeSet::new()))
}

fn handle_with_config(config: LookupConfig) -> LookupHandle {
    LookupHandle::new(
        config,
        Some(ReleaseIdentity {
            name: "web".to_owned(),
            namespace: "demo".to_owned(),
        }),
        Some(ScopeSource::new(|release| {
            Ok(Some(ReleaseScope {
                directory: format!("platform/{}", release.name),
                namespace: release.namespace.clone(),
            }))
        })),
    )
}

fn render(chart: &ResolvedChart, handle: Option<LookupHandle>) -> nyl_render::Result<Vec<Value>> {
    executor(handle).template(chart, "web", Some("demo"), &Value::Null)
}

fn executor(handle: Option<LookupHandle>) -> HelmTemplateExecutor {
    HelmTemplateExecutor::new()
        .with_kube_version("1.30.0")
        .with_api_versions(vec![
            "v1".to_owned(),
            "apps/v1".to_owned(),
            "foo.example.com/v1".to_owned(),
        ])
        .with_lookup(handle)
}

fn secret_data(manifests: &[Value]) -> Value {
    manifests
        .iter()
        .find(|manifest| manifest["kind"] == "Secret")
        .expect("the chart renders its Secret")["data"]
        .clone()
}

fn results(manifests: &[Value]) -> Value {
    manifests
        .iter()
        .find(|manifest| manifest["metadata"]["name"] == "lookup-results")
        .expect("the chart renders its results")["data"]
        .clone()
}

#[test]
fn test_a_found_object_is_reused_so_two_renders_are_byte_identical() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let first = render(&chart, Some(handle(Snapshot(Ok(snapshot_with_ca()))))).unwrap();
    let second = render(&chart, Some(handle(Snapshot(Ok(snapshot_with_ca()))))).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        secret_data(&first),
        json!({"tls.crt": "U05BUFNIT1QtQ0VSVA==", "tls.key": "U05BUFNIT1QtS0VZ"}),
        "the chart reuses the CA the snapshot holds"
    );
}

#[test]
fn test_an_absent_object_generates_and_the_render_still_succeeds() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let without_ca = objects(vec![
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-a", "namespace": "demo"}}),
    ]);
    let first = render(&chart, Some(handle(Snapshot(Ok(without_ca.clone()))))).unwrap();
    let second = render(&chart, Some(handle(Snapshot(Ok(without_ca))))).unwrap();
    assert_ne!(
        secret_data(&first),
        secret_data(&second),
        "an absent CA is generated afresh, which is what a snapshot prevents once the CA is published"
    );
    assert_ne!(secret_data(&first)["tls.crt"], json!("U05BUFNIT1QtQ0VSVA=="));
}

#[test]
fn test_lookup_sees_only_what_the_snapshot_holds_and_capabilities_match_the_declaration() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let manifests = render(&chart, Some(handle(Snapshot(Ok(snapshot_with_ca()))))).unwrap();
    let data = results(&manifests);
    assert_eq!(data["listNames"], "cm-a,cm-b", "a list covers the namespace it names");
    assert_eq!(data["missingIsEmpty"], "true");
    assert_eq!(data["elsewhere"], "1");
    assert_eq!(data["kubeVersion"], "v1.30.0");
    assert_eq!(data["hasFoo"], "true", "a declared API version is listed");
    assert_eq!(data["hasPolicyV1"], "true", "Helm's own defaults are listed");
    assert_eq!(data["hasShipped"], "true", "the chart's own CRDs are listed");
}

#[test]
fn test_an_unreadable_snapshot_fails_the_render_instead_of_reading_as_absent() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let broken = StoreError::new("NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE", "the publication cannot be read");
    let error = render(&chart, Some(handle(Snapshot(Err(broken)))))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("NYL-HELM-LOOKUP-SNAPSHOT-UNAVAILABLE:"), "{error}");
    assert!(error.contains("the publication cannot be read"), "{error}");
}

#[test]
fn test_refreshing_an_object_treats_it_as_absent_for_the_render() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let config = LookupConfig::new(
        Arc::new(Snapshot(Ok(snapshot_with_ca()))),
        BTreeSet::from(["Secret/demo/my-ca".parse().unwrap()]),
    );
    let manifests = render(&chart, Some(handle_with_config(config))).unwrap();
    assert_ne!(
        secret_data(&manifests)["tls.crt"],
        json!("U05BUFNIT1QtQ0VSVA=="),
        "the refreshed CA is generated again"
    );
    assert_eq!(
        results(&manifests)["listNames"],
        "cm-a,cm-b",
        "other objects are unaffected"
    );
}

#[test]
fn test_a_chart_that_does_not_opt_in_renders_offline_and_finds_nothing() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let manifests = render(&chart, None).unwrap();
    let data = results(&manifests);
    assert_eq!(data["listNames"], "");
    assert_eq!(data["elsewhere"], "0");
    assert_eq!(
        data["hasPolicyV1"], "true",
        "Helm's default API versions exist offline too, which is why discovery lists them"
    );
    assert_eq!(data["hasFoo"], "true");
    assert_ne!(secret_data(&manifests)["tls.crt"], json!("U05BUFNIT1QtQ0VSVA=="));
}

#[test]
fn test_a_rendered_kind_of_an_undeclared_api_version_fails_with_the_fix() {
    let directory = TempDir::new().unwrap();
    let chart = chart(
        directory.path(),
        &[(
            "widget.yaml",
            "apiVersion: undeclared.example.com/v1\nkind: Widget\nmetadata:\n  name: w\nspec: {}\n",
        )],
    );
    let error = render(&chart, Some(handle(Snapshot(Ok(snapshot_with_ca())))))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("NYL-HELM-LOOKUP-API-VERSION-UNDECLARED:"), "{error}");
    assert!(
        error.contains("undeclared.example.com/v1") && error.contains("spec.kubernetes.apiVersions"),
        "{error}"
    );
}

#[test]
fn test_objects_of_other_releases_are_invisible() {
    // The backend decides which objects belong to the Release; a render outside
    // any published Release finds nothing even when another Release holds a CA.
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let unpublished = LookupHandle::new(
        LookupConfig::new(Arc::new(Snapshot(Ok(snapshot_with_ca()))), BTreeSet::new()),
        None,
        None,
    );
    let manifests = render(&chart, Some(unpublished)).unwrap();
    assert_ne!(secret_data(&manifests)["tls.crt"], json!("U05BUFNIT1QtQ0VSVA=="));
    assert_eq!(results(&manifests)["listNames"], "");
}

/// Catches the Helm cache assuming a lookup result: a cached render is reused
/// while what the chart looked up is unchanged (a regenerated CA would differ),
/// and not once the snapshot holds the object the render found absent.
#[test]
fn test_a_cached_render_is_reused_only_while_its_lookups_return_the_same_results() {
    let directory = TempDir::new().unwrap();
    let chart = chart(directory.path(), &[]);
    let cache_root = TempDir::new().unwrap();
    let cache = nyl_render::render::cache::RenderCache::with_root(
        cache_root.path(),
        nyl_render::render::cache::CacheMode::Default,
    )
    .unwrap();
    let cached = |snapshot: ObjectSet| {
        executor(Some(handle(Snapshot(Ok(snapshot)))))
            .with_gitops_cache(Some(cache.clone()))
            .template(&chart, "web", Some("demo"), &Value::Null)
            .unwrap()
    };
    let without_ca = objects(vec![
        json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cm-a", "namespace": "demo"}}),
    ]);

    let generated = cached(without_ca.clone());
    assert_eq!(
        cached(without_ca),
        generated,
        "the unchanged snapshot reuses the render, which a regenerated CA would not match"
    );
    let reused = cached(snapshot_with_ca());
    assert_eq!(secret_data(&reused)["tls.crt"], json!("U05BUFNIT1QtQ0VSVA=="));
    assert_ne!(
        reused, generated,
        "a snapshot that now holds the CA invalidates the render"
    );
    assert_eq!(cached(snapshot_with_ca()), reused);
}
