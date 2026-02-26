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
        if key
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .is_some()
        {
            continue;
        }

        let is_namespaced = if let Some(cached) = scope_cache.get(&key.gvk) {
            *cached
        } else {
            let discovered = client.is_namespaced(&key.gvk).await?;
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
        if key
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|ns| !ns.is_empty())
            .is_some()
        {
            adjusted.insert(key.clone(), *count);
            continue;
        }

        let is_namespaced = if let Some(cached) = scope_cache.get(&key.gvk) {
            *cached
        } else {
            let discovered = client.is_namespaced(&key.gvk).await?;
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
    use crate::kubernetes::MockKubeClient;
    use serde_json::json;

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
}
