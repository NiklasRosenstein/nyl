use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::{
    kubernetes::{GroupVersionKind, KubeClient, ResourceKey},
    NylError, Result,
};

/// The scopes of the kinds that CustomResourceDefinitions among the manifests define.
///
/// A custom resource whose CRD is new in the same manifest set has no kind the
/// cluster can name yet, and a dry run never creates the CRD. The CRD declares the
/// scope, so reading it there resolves the resource's namespace before anything is
/// applied: `apply`, `diff`, and the keys a release records agree, and the result
/// depends only on the declaration. Like the cluster's discovery, only served
/// versions are requestable.
#[derive(Debug, Default, Clone)]
pub struct DeclaredScopes {
    namespaced: HashMap<GroupVersionKind, bool>,
}

impl DeclaredScopes {
    /// The scopes declared by the CustomResourceDefinitions among `manifests`.
    pub fn of(manifests: &[Value]) -> Self {
        let namespaced = manifests
            .iter()
            .filter(|manifest| {
                manifest["kind"].as_str() == Some("CustomResourceDefinition")
                    && manifest["apiVersion"]
                        .as_str()
                        .is_some_and(|api_version| api_version.split('/').next() == Some("apiextensions.k8s.io"))
            })
            .flat_map(|crd| crd_scope_entries(&crd["spec"]))
            .collect();
        Self { namespaced }
    }

    /// Whether the kind is namespaced, when a manifest declares it.
    pub fn is_namespaced(&self, gvk: &GroupVersionKind) -> Option<bool> {
        self.namespaced.get(gvk).copied()
    }

    /// Every declared kind with its scope, by group and kind.
    pub(crate) fn kinds(&self) -> impl Iterator<Item = ((&str, &str), bool)> {
        self.namespaced
            .iter()
            .map(|(gvk, namespaced)| ((gvk.group.as_str(), gvk.kind.as_str()), *namespaced))
    }

    /// Whether the kind is namespaced: as a manifest declares it, otherwise as the
    /// cluster reports it.
    async fn resolve(&self, client: &dyn KubeClient, gvk: &GroupVersionKind) -> Result<bool> {
        match self.is_namespaced(gvk) {
            Some(namespaced) => Ok(namespaced),
            None => client.is_namespaced(gvk).await,
        }
    }
}

/// The served kinds and scope a CustomResourceDefinition's `spec` defines.
pub(crate) fn crd_scope_entries(spec: &Value) -> Vec<(GroupVersionKind, bool)> {
    let (Some(group), Some(kind), Some(versions)) = (
        spec.get("group").and_then(Value::as_str),
        spec.get("names")
            .and_then(|names| names.get("kind"))
            .and_then(Value::as_str),
        spec.get("versions").and_then(Value::as_array),
    ) else {
        return Vec::new();
    };
    let namespaced = match spec.get("scope").and_then(Value::as_str) {
        Some("Namespaced") => true,
        Some("Cluster") => false,
        _ => return Vec::new(),
    };
    // Mirror Kubernetes behavior: only served versions are requestable.
    versions
        .iter()
        .filter(|version| version.get("served").and_then(Value::as_bool) == Some(true))
        .filter_map(|version| version.get("name").and_then(Value::as_str))
        .map(|version| {
            let gvk = GroupVersionKind {
                group: group.to_string(),
                version: version.to_string(),
                kind: kind.to_string(),
            };
            (gvk, namespaced)
        })
        .collect()
}

/// Resolve missing metadata.namespace values for namespaced resources.
///
/// A resource keeps its own namespace; otherwise it takes the destination
/// namespace, as Argo CD's Application destination gives it. The kube
/// context's default namespace is never used, so the result depends only on
/// the declaration. The scope of a kind that a CustomResourceDefinition among
/// the manifests defines is read from it ([`DeclaredScopes`]); any other kind
/// is looked up in the cluster, and one the cluster does not know is left alone.
pub async fn resolve_manifest_namespaces(
    client: &dyn KubeClient,
    manifests: &mut [Value],
    destination_namespace: Option<&str>,
) -> Result<usize> {
    let declared = DeclaredScopes::of(manifests);
    let mut resolved_count = 0;
    let mut scope_cache: HashMap<GroupVersionKind, bool> = HashMap::new();

    for manifest in manifests {
        let key = ResourceKey::from_json_value(manifest)?;
        if key.namespace.as_deref().is_some_and(|ns| !ns.trim().is_empty()) {
            continue;
        }

        let is_namespaced = if let Some(cached) = scope_cache.get(&key.gvk) {
            *cached
        } else {
            let discovered = match declared.resolve(client, &key.gvk).await {
                Ok(v) => v,
                Err(err) if err.is_api_resource_not_found_error() => {
                    tracing::warn!(
                        api_version = %format_api_version(&key.gvk),
                        kind = %key.gvk.kind,
                        "Skipping namespace resolution for resource with unknown API discovery"
                    );
                    scope_cache.insert(key.gvk.clone(), false);
                    continue;
                }
                Err(err) => return Err(err),
            };
            scope_cache.insert(key.gvk.clone(), discovered);
            discovered
        };
        if !is_namespaced {
            continue;
        }

        let namespace = destination_namespace
            .filter(|ns| !ns.trim().is_empty())
            .ok_or_else(|| missing_namespace(&key))?;

        set_manifest_namespace(manifest, namespace)?;
        resolved_count += 1;

        tracing::debug!(
            kind = %key.gvk.kind,
            name = %key.name,
            namespace = %namespace,
            "Resolved missing resource namespace"
        );
    }

    if resolved_count > 0 {
        tracing::info!(
            "Resolved missing namespaces for {} rendered resource(s)",
            resolved_count
        );
    }

    Ok(resolved_count)
}

/// Re-key duplicate map after namespace resolution mutates manifest keys.
pub async fn adjust_duplicate_keys_for_namespace_resolution(
    client: &dyn KubeClient,
    declared: &DeclaredScopes,
    duplicates: &HashMap<ResourceKey, usize>,
    destination_namespace: Option<&str>,
) -> Result<HashMap<ResourceKey, usize>> {
    let mut adjusted = HashMap::with_capacity(duplicates.len());
    let mut scope_cache: HashMap<GroupVersionKind, bool> = HashMap::new();

    for (key, count) in duplicates {
        if key.namespace.as_deref().is_some_and(|ns| !ns.trim().is_empty()) {
            adjusted.insert(key.clone(), *count);
            continue;
        }

        let is_namespaced = if let Some(cached) = scope_cache.get(&key.gvk) {
            *cached
        } else {
            let discovered = match declared.resolve(client, &key.gvk).await {
                Ok(v) => v,
                Err(err) if err.is_api_resource_not_found_error() => {
                    tracing::warn!(
                        api_version = %format_api_version(&key.gvk),
                        kind = %key.gvk.kind,
                        "Skipping duplicate-key namespace adjustment for resource with unknown API discovery"
                    );
                    scope_cache.insert(key.gvk.clone(), false);
                    adjusted.insert(key.clone(), *count);
                    continue;
                }
                Err(err) => return Err(err),
            };
            scope_cache.insert(key.gvk.clone(), discovered);
            discovered
        };

        if !is_namespaced {
            adjusted.insert(key.clone(), *count);
            continue;
        }

        let namespace = destination_namespace
            .filter(|ns| !ns.trim().is_empty())
            .ok_or_else(|| missing_namespace(key))?;

        let mut resolved_key = key.clone();
        resolved_key.namespace = Some(namespace.to_string());
        adjusted.insert(resolved_key, *count);
    }

    Ok(adjusted)
}

fn missing_namespace(key: &ResourceKey) -> NylError {
    NylError::config(format!(
        "{} {} sets no namespace and the file declares no Release to give it one; set metadata.namespace, declare a Release, or pass --namespace",
        key.gvk.kind, key.name
    ))
}

fn format_api_version(gvk: &GroupVersionKind) -> String {
    if gvk.group.is_empty() {
        gvk.version.clone()
    } else {
        format!("{}/{}", gvk.group, gvk.version)
    }
}

fn set_manifest_namespace(manifest: &mut Value, namespace: &str) -> Result<()> {
    let root = manifest
        .as_object_mut()
        .ok_or_else(|| NylError::Config("Manifest must be a JSON object".to_string()))?;

    let metadata = root.entry("metadata").or_insert_with(|| Value::Object(Map::new()));

    let metadata = metadata
        .as_object_mut()
        .ok_or_else(|| NylError::Config("Manifest metadata must be a JSON object".to_string()))?;

    metadata.insert("namespace".to_string(), Value::String(namespace.to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kubernetes::{ApplyOutcome, GroupVersionKind, MockKubeClient};
    use async_trait::async_trait;
    use kube::api::DynamicObject;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    struct CountingKubeClient {
        inner: MockKubeClient,
        is_namespaced_calls: Arc<Mutex<HashMap<GroupVersionKind, usize>>>,
    }

    impl CountingKubeClient {
        fn new(inner: MockKubeClient) -> Self {
            Self {
                inner,
                is_namespaced_calls: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn is_namespaced_call_count(&self, gvk: &GroupVersionKind) -> usize {
            self.is_namespaced_calls.lock().unwrap().get(gvk).copied().unwrap_or(0)
        }
    }

    struct ErroringKubeClient {
        default_namespace: String,
        call_counts: Arc<Mutex<HashMap<GroupVersionKind, usize>>>,
    }

    impl ErroringKubeClient {
        fn new(default_namespace: &str) -> Self {
            Self {
                default_namespace: default_namespace.to_string(),
                call_counts: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        fn call_count(&self, gvk: &GroupVersionKind) -> usize {
            self.call_counts.lock().unwrap().get(gvk).copied().unwrap_or(0)
        }
    }

    #[async_trait]
    impl KubeClient for CountingKubeClient {
        async fn get_resource(
            &self,
            gvk: &GroupVersionKind,
            namespace: Option<&str>,
            name: &str,
        ) -> Result<Option<DynamicObject>> {
            self.inner.get_resource(gvk, namespace, name).await
        }

        async fn apply_resource(
            &self,
            resource: &DynamicObject,
            field_manager: &str,
            dry_run: bool,
        ) -> Result<ApplyOutcome> {
            self.inner.apply_resource(resource, field_manager, dry_run).await
        }

        async fn get_server_version(&self) -> Result<String> {
            self.inner.get_server_version().await
        }

        async fn get_api_versions(&self) -> Result<Vec<String>> {
            self.inner.get_api_versions().await
        }

        async fn is_namespaced(&self, gvk: &GroupVersionKind) -> Result<bool> {
            {
                let mut counts = self.is_namespaced_calls.lock().unwrap();
                *counts.entry(gvk.clone()).or_insert(0) += 1;
            }
            self.inner.is_namespaced(gvk).await
        }

        fn default_namespace(&self) -> &str {
            self.inner.default_namespace()
        }

        async fn delete_resource_with(
            &self,
            gvk: &GroupVersionKind,
            namespace: Option<&str>,
            name: &str,
            options: &crate::kubernetes::DeleteOptions,
        ) -> Result<()> {
            self.inner.delete_resource_with(gvk, namespace, name, options).await
        }

        async fn get_normalized_resource(
            &self,
            resource: &DynamicObject,
            field_manager: &str,
        ) -> Result<DynamicObject> {
            self.inner.get_normalized_resource(resource, field_manager).await
        }

        async fn release_field_manager(&self, resource: &DynamicObject, field_manager: &str) -> Result<bool> {
            self.inner.release_field_manager(resource, field_manager).await
        }
    }

    #[async_trait]
    impl KubeClient for ErroringKubeClient {
        async fn get_resource(
            &self,
            _gvk: &GroupVersionKind,
            _namespace: Option<&str>,
            _name: &str,
        ) -> Result<Option<DynamicObject>> {
            Ok(None)
        }

        async fn apply_resource(
            &self,
            _resource: &DynamicObject,
            _field_manager: &str,
            _dry_run: bool,
        ) -> Result<ApplyOutcome> {
            Err(NylError::Other("not used in this test".to_string()))
        }

        async fn get_server_version(&self) -> Result<String> {
            Ok("1.30.0".to_string())
        }

        async fn get_api_versions(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }

        async fn is_namespaced(&self, gvk: &GroupVersionKind) -> Result<bool> {
            {
                let mut counts = self.call_counts.lock().unwrap();
                *counts.entry(gvk.clone()).or_insert(0) += 1;
            }
            Err(NylError::ApiResourceNotFound(format!(
                "{}/{}",
                if gvk.group.is_empty() {
                    gvk.version.clone()
                } else {
                    format!("{}/{}", gvk.group, gvk.version)
                },
                gvk.kind
            )))
        }

        fn default_namespace(&self) -> &str {
            &self.default_namespace
        }

        async fn delete_resource_with(
            &self,
            _gvk: &GroupVersionKind,
            _namespace: Option<&str>,
            _name: &str,
            _options: &crate::kubernetes::DeleteOptions,
        ) -> Result<()> {
            Ok(())
        }

        async fn get_normalized_resource(
            &self,
            resource: &DynamicObject,
            _field_manager: &str,
        ) -> Result<DynamicObject> {
            Ok(resource.clone())
        }

        async fn release_field_manager(&self, _resource: &DynamicObject, _field_manager: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_keeps_explicit_namespace() {
        let client = MockKubeClient::new();
        let mut manifests = vec![json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "cm", "namespace": "explicit"}
        })];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, Some("release"))
            .await
            .unwrap();

        assert_eq!(resolved, 0);
        assert_eq!(manifests[0]["metadata"]["namespace"].as_str(), Some("explicit"));
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_uses_release_namespace() {
        let client = MockKubeClient::with_default_namespace("ctx-default");
        let mut manifests = vec![json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": {"name": "sa"}
        })];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, Some("release-ns"))
            .await
            .unwrap();

        assert_eq!(resolved, 1);
        assert_eq!(manifests[0]["metadata"]["namespace"].as_str(), Some("release-ns"));
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_skips_cluster_scoped_resources() {
        let client = MockKubeClient::new();
        let mut manifests = vec![json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {"name": "ns"}
        })];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, Some("release-ns"))
            .await
            .unwrap();

        assert_eq!(resolved, 0);
        assert!(manifests[0]["metadata"]["namespace"].is_null());
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_never_uses_the_kube_context_namespace() {
        let client = MockKubeClient::with_default_namespace("ctx-default");
        let mut manifests = vec![json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": {"name": "sa"}
        })];

        let error = resolve_manifest_namespaces(&client, &mut manifests, None)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("ServiceAccount sa sets no namespace and the file declares no Release"));
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_treats_empty_namespace_as_missing() {
        let client = MockKubeClient::with_default_namespace("ctx-default");
        let mut manifests = vec![json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": {"name": "sa", "namespace": "   "}
        })];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, Some("release-ns"))
            .await
            .unwrap();
        assert_eq!(resolved, 1);
        assert_eq!(manifests[0]["metadata"]["namespace"].as_str(), Some("release-ns"));
    }

    #[tokio::test]
    async fn test_adjust_duplicate_keys_for_namespace_resolution() {
        let client = MockKubeClient::with_default_namespace("ctx-default");
        let mut duplicates = HashMap::new();
        duplicates.insert(
            ResourceKey {
                gvk: crate::kubernetes::GroupVersionKind {
                    group: String::new(),
                    version: "v1".to_string(),
                    kind: "ServiceAccount".to_string(),
                },
                namespace: None,
                name: "sa".to_string(),
            },
            2,
        );

        let adjusted = adjust_duplicate_keys_for_namespace_resolution(
            &client,
            &DeclaredScopes::default(),
            &duplicates,
            Some("release"),
        )
        .await
        .unwrap();
        assert!(adjusted.contains_key(&ResourceKey {
            gvk: crate::kubernetes::GroupVersionKind {
                group: String::new(),
                version: "v1".to_string(),
                kind: "ServiceAccount".to_string(),
            },
            namespace: Some("release".to_string()),
            name: "sa".to_string(),
        }));
        assert_eq!(adjusted.values().next().copied(), Some(2));
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_caches_scope_lookup_per_gvk() {
        let inner = MockKubeClient::with_default_namespace("ctx-default");
        let client = CountingKubeClient::new(inner);
        let mut manifests = vec![
            json!({
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {"name": "sa-one"}
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {"name": "sa-two"}
            }),
        ];

        resolve_manifest_namespaces(&client, &mut manifests, Some("release"))
            .await
            .unwrap();

        let gvk = GroupVersionKind {
            group: String::new(),
            version: "v1".to_string(),
            kind: "ServiceAccount".to_string(),
        };
        assert_eq!(client.is_namespaced_call_count(&gvk), 1);
    }

    #[tokio::test]
    async fn test_adjust_duplicate_keys_caches_scope_lookup_per_gvk() {
        let inner = MockKubeClient::with_default_namespace("ctx-default");
        let client = CountingKubeClient::new(inner);
        let mut duplicates = HashMap::new();
        let key = ResourceKey {
            gvk: GroupVersionKind {
                group: String::new(),
                version: "v1".to_string(),
                kind: "ServiceAccount".to_string(),
            },
            namespace: None,
            name: "sa".to_string(),
        };
        duplicates.insert(key.clone(), 2);
        duplicates.insert(
            ResourceKey {
                gvk: key.gvk.clone(),
                namespace: None,
                name: "sa-2".to_string(),
            },
            2,
        );

        adjust_duplicate_keys_for_namespace_resolution(
            &client,
            &DeclaredScopes::default(),
            &duplicates,
            Some("release"),
        )
        .await
        .unwrap();

        assert_eq!(client.is_namespaced_call_count(&key.gvk), 1);
    }

    #[tokio::test]
    async fn test_resolve_manifest_namespaces_skips_api_not_found_and_caches() {
        let client = ErroringKubeClient::new("ctx-default");
        let gvk = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v1".to_string(),
            kind: "ClusterPolicy".to_string(),
        };
        let mut manifests = vec![
            json!({
                "apiVersion": "kyverno.io/v1",
                "kind": "ClusterPolicy",
                "metadata": {"name": "policy-1"}
            }),
            json!({
                "apiVersion": "kyverno.io/v1",
                "kind": "ClusterPolicy",
                "metadata": {"name": "policy-2"}
            }),
        ];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, Some("release"))
            .await
            .unwrap();

        assert_eq!(resolved, 0);
        assert_eq!(client.call_count(&gvk), 1);
    }

    #[tokio::test]
    async fn test_adjust_duplicate_keys_skips_api_not_found_and_caches() {
        let client = ErroringKubeClient::new("ctx-default");
        let gvk = GroupVersionKind {
            group: "kyverno.io".to_string(),
            version: "v1".to_string(),
            kind: "ClusterPolicy".to_string(),
        };

        let mut duplicates = HashMap::new();
        duplicates.insert(
            ResourceKey {
                gvk: gvk.clone(),
                namespace: None,
                name: "policy-1".to_string(),
            },
            2,
        );
        duplicates.insert(
            ResourceKey {
                gvk: gvk.clone(),
                namespace: None,
                name: "policy-2".to_string(),
            },
            1,
        );

        let adjusted = adjust_duplicate_keys_for_namespace_resolution(
            &client,
            &DeclaredScopes::default(),
            &duplicates,
            Some("release"),
        )
        .await
        .unwrap();

        assert_eq!(adjusted.len(), 2);
        assert_eq!(client.call_count(&gvk), 1);
    }

    fn crd(group: &str, kind: &str, scope: &str, versions: Value) -> Value {
        json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": format!("{}s.{group}", kind.to_lowercase())},
            "spec": {"group": group, "scope": scope, "names": {"kind": kind}, "versions": versions}
        })
    }

    /// A CRD in the manifests decides the scope of its custom resources, so one that
    /// no cluster can name yet still takes the destination namespace (or none, for a
    /// cluster-scoped kind), and only the served versions count.
    #[tokio::test]
    async fn test_resolve_manifest_namespaces_reads_scope_from_crds_in_the_manifests() {
        let client = ErroringKubeClient::new("ctx-default");
        let versions = json!([
            {"name": "v1", "served": true, "storage": true},
            {"name": "v1alpha1", "served": false, "storage": false}
        ]);
        let mut manifests = vec![
            crd("example.com", "Widget", "Namespaced", versions.clone()),
            crd("example.com", "Cluster", "Cluster", versions),
            json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "w"}}),
            json!({"apiVersion": "example.com/v1", "kind": "Cluster", "metadata": {"name": "c"}}),
            json!({"apiVersion": "example.com/v1alpha1", "kind": "Widget", "metadata": {"name": "unserved"}}),
        ];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, Some("release"))
            .await
            .unwrap();

        assert_eq!(resolved, 1);
        assert_eq!(manifests[2]["metadata"]["namespace"], "release");
        assert!(manifests[3]["metadata"].get("namespace").is_none());
        // An unserved version is not requestable, so the cluster is asked, and cannot say.
        assert!(manifests[4]["metadata"].get("namespace").is_none());
    }

    /// The duplicate counts follow the namespace the resources resolve to.
    #[tokio::test]
    async fn test_adjust_duplicate_keys_reads_scope_from_crds_in_the_manifests() {
        let client = ErroringKubeClient::new("ctx-default");
        let manifests = vec![crd(
            "example.com",
            "Widget",
            "Namespaced",
            json!([{"name": "v1", "served": true}]),
        )];
        let key = ResourceKey {
            gvk: GroupVersionKind {
                group: "example.com".to_string(),
                version: "v1".to_string(),
                kind: "Widget".to_string(),
            },
            namespace: None,
            name: "w".to_string(),
        };
        let duplicates = HashMap::from([(key.clone(), 2)]);

        let adjusted = adjust_duplicate_keys_for_namespace_resolution(
            &client,
            &DeclaredScopes::of(&manifests),
            &duplicates,
            Some("release"),
        )
        .await
        .unwrap();

        let resolved = ResourceKey {
            namespace: Some("release".to_string()),
            ..key
        };
        assert_eq!(adjusted, HashMap::from([(resolved, 2)]));
    }
}
