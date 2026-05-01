use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::{
    kubernetes::{GroupVersionKind, KubeClient, ResourceKey},
    NylError, Result,
};

/// Resolve missing metadata.namespace values for namespaced resources.
///
/// Fallback order:
/// 1. Existing manifest metadata.namespace
/// 2. release_namespace argument
/// 3. kube client default namespace
pub async fn resolve_manifest_namespaces(
    client: &dyn KubeClient,
    manifests: &mut [Value],
    release_namespace: Option<&str>,
) -> Result<usize> {
    let mut resolved_count = 0;
    let default_namespace = client.default_namespace();
    let mut scope_cache: HashMap<GroupVersionKind, bool> = HashMap::new();

    for manifest in manifests {
        let key = ResourceKey::from_json_value(manifest)?;
        if key.namespace.as_deref().is_some_and(|ns| !ns.trim().is_empty()) {
            continue;
        }

        let is_namespaced = if let Some(cached) = scope_cache.get(&key.gvk) {
            *cached
        } else {
            let discovered = match client.is_namespaced(&key.gvk).await {
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

        let namespace = release_namespace
            .filter(|ns| !ns.is_empty())
            .or_else(|| (!default_namespace.is_empty()).then_some(default_namespace))
            .ok_or_else(|| {
                NylError::Config(format!(
                    "Namespace required for namespaced resource {} {}",
                    key.gvk.kind, key.name
                ))
            })?;

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
    duplicates: &HashMap<ResourceKey, usize>,
    release_namespace: Option<&str>,
) -> Result<HashMap<ResourceKey, usize>> {
    let mut adjusted = HashMap::with_capacity(duplicates.len());
    let mut scope_cache: HashMap<GroupVersionKind, bool> = HashMap::new();
    let default_namespace = client.default_namespace();

    for (key, count) in duplicates {
        if key.namespace.as_deref().is_some_and(|ns| !ns.trim().is_empty()) {
            adjusted.insert(key.clone(), *count);
            continue;
        }

        let is_namespaced = if let Some(cached) = scope_cache.get(&key.gvk) {
            *cached
        } else {
            let discovered = match client.is_namespaced(&key.gvk).await {
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

        let namespace = release_namespace
            .filter(|ns| !ns.trim().is_empty())
            .or_else(|| {
                let trimmed = default_namespace.trim();
                (!trimmed.is_empty()).then_some(trimmed)
            })
            .ok_or_else(|| {
                NylError::Config(format!(
                    "Namespace required for namespaced resource {} {}",
                    key.gvk.kind, key.name
                ))
            })?;

        let mut resolved_key = key.clone();
        resolved_key.namespace = Some(namespace.to_string());
        adjusted.insert(resolved_key, *count);
    }

    Ok(adjusted)
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

        async fn delete_resource(&self, gvk: &GroupVersionKind, namespace: Option<&str>, name: &str) -> Result<()> {
            self.inner.delete_resource(gvk, namespace, name).await
        }

        async fn get_normalized_resource(
            &self,
            resource: &DynamicObject,
            field_manager: &str,
        ) -> Result<DynamicObject> {
            self.inner.get_normalized_resource(resource, field_manager).await
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

        async fn delete_resource(&self, _gvk: &GroupVersionKind, _namespace: Option<&str>, _name: &str) -> Result<()> {
            Ok(())
        }

        async fn get_normalized_resource(
            &self,
            resource: &DynamicObject,
            _field_manager: &str,
        ) -> Result<DynamicObject> {
            Ok(resource.clone())
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
    async fn test_resolve_manifest_namespaces_uses_client_default_namespace() {
        let client = MockKubeClient::with_default_namespace("ctx-default");
        let mut manifests = vec![json!({
            "apiVersion": "v1",
            "kind": "ServiceAccount",
            "metadata": {"name": "sa"}
        })];

        let resolved = resolve_manifest_namespaces(&client, &mut manifests, None)
            .await
            .unwrap();

        assert_eq!(resolved, 1);
        assert_eq!(manifests[0]["metadata"]["namespace"].as_str(), Some("ctx-default"));
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
    async fn test_resolve_manifest_namespaces_errors_when_no_fallback_exists() {
        let client = MockKubeClient::with_default_namespace("");
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
            .contains("Namespace required for namespaced resource ServiceAccount sa"));
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

        let adjusted = adjust_duplicate_keys_for_namespace_resolution(&client, &duplicates, Some("release"))
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

        adjust_duplicate_keys_for_namespace_resolution(&client, &duplicates, Some("release"))
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

        let adjusted = adjust_duplicate_keys_for_namespace_resolution(&client, &duplicates, Some("release"))
            .await
            .unwrap();

        assert_eq!(adjusted.len(), 2);
        assert_eq!(client.call_count(&gvk), 1);
    }
}
