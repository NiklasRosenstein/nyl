//! kubectl-style YAML diff engine for Kubernetes resources
//!
//! This module provides functionality to compare Kubernetes resources by:
//! 1. Normalizing resources (removing auto-generated fields)
//! 2. Converting to YAML with consistent formatting
//! 3. Generating unified diffs (like kubectl diff)

use crate::Result;
use kube::api::DynamicObject;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

/// Diff engine for comparing Kubernetes resources
pub struct DiffEngine;

impl DiffEngine {
    /// Normalize a resource for comparison by removing auto-generated fields
    ///
    /// This removes fields that Kubernetes generates automatically and should
    /// not be considered when comparing desired vs live state:
    /// - metadata.resourceVersion
    /// - metadata.uid
    /// - metadata.generation
    /// - metadata.creationTimestamp
    /// - metadata.managedFields
    /// - metadata.selfLink
    /// - status (entire section)
    pub fn normalize(resource: &mut Value) {
        // Remove auto-generated metadata fields
        if let Some(metadata) = resource.get_mut("metadata").and_then(|m| m.as_object_mut()) {
            metadata.remove("resourceVersion");
            metadata.remove("uid");
            metadata.remove("generation");
            metadata.remove("creationTimestamp");
            metadata.remove("managedFields");
            metadata.remove("selfLink");
        }

        // Remove entire status section
        if let Some(obj) = resource.as_object_mut() {
            obj.remove("status");
        }
    }

    /// Convert a resource to normalized YAML string for comparison
    ///
    /// This normalizes the resource and converts it to YAML with consistent
    /// formatting for reliable comparison.
    pub fn to_comparable_yaml(resource: &Value) -> Result<String> {
        let mut normalized = resource.clone();
        Self::normalize(&mut normalized);

        // Convert to YAML with consistent formatting
        serde_norway::to_string(&normalized)
            .map_err(|e| crate::NylError::Config(format!("Failed to serialize resource to YAML: {}", e)))
    }

    /// Compare two resources and return a unified diff (kubectl-style)
    ///
    /// This generates a unified diff similar to `kubectl diff` by:
    /// 1. Normalizing both resources
    /// 2. Converting to YAML
    /// 3. Running a line-by-line unified diff algorithm
    ///
    /// Returns empty string if resources are equivalent.
    pub fn diff_yaml(desired: &Value, live: &Value) -> Result<String> {
        let desired_yaml = Self::to_comparable_yaml(desired)?;
        let live_yaml = Self::to_comparable_yaml(live)?;

        // Generate unified diff using similar crate
        let diff = TextDiff::from_lines(&live_yaml, &desired_yaml);

        // Build unified diff output
        let mut output = String::new();

        for hunk in diff.unified_diff().iter_hunks() {
            // Add hunk header (e.g., @@ -1,4 +1,4 @@)
            use std::fmt::Write;
            write!(&mut output, "{}", hunk.header()).unwrap();

            // Add diff lines
            for change in hunk.iter_changes() {
                match change.tag() {
                    ChangeTag::Delete => {
                        output.push('-');
                        output.push_str(change.value());
                    }
                    ChangeTag::Insert => {
                        output.push('+');
                        output.push_str(change.value());
                    }
                    ChangeTag::Equal => {
                        output.push(' ');
                        output.push_str(change.value());
                    }
                }
            }
        }

        Ok(output)
    }

    /// Check if two resources are equivalent after normalization
    ///
    /// This is a simple equality check after normalization. Use this for
    /// determining if resources differ, then use diff_yaml() to show what changed.
    pub fn are_equivalent(desired: &Value, live: &Value) -> Result<bool> {
        let desired_yaml = Self::to_comparable_yaml(desired)?;
        let live_yaml = Self::to_comparable_yaml(live)?;
        Ok(desired_yaml == live_yaml)
    }

    /// Normalize a resource with optional server-side normalization
    ///
    /// If a client is provided, sends the resource to the server with dry-run
    /// to apply server-side defaults. Otherwise, just clones the resource.
    /// In both cases, applies client-side normalization to remove metadata fields.
    pub async fn normalize_with_server(
        resource: &Value,
        client: Option<&dyn crate::kubernetes::KubeClient>,
    ) -> Result<Value> {
        let normalized = if let Some(client) = client {
            // Convert to DynamicObject for server-side normalization
            let dynamic: DynamicObject = serde_json::from_value(resource.clone())?;

            // Get server-normalized version using dry-run apply
            let server_normalized = client.get_normalized_resource(&dynamic, "nyl-diff").await?;

            // Convert back to Value
            serde_json::to_value(&server_normalized)?
        } else {
            // No client provided, just clone
            resource.clone()
        };

        // Apply client-side normalization to remove metadata fields
        let mut result = normalized;
        Self::normalize(&mut result);
        Ok(result)
    }

    /// Generate a diff with server-side normalization
    ///
    /// Uses server-side apply dry-run to normalize the desired resource,
    /// then compares against the live resource (which already has server defaults).
    pub async fn diff_yaml_with_server(
        desired: &Value,
        live: &Value,
        client: &dyn crate::kubernetes::KubeClient,
    ) -> Result<String> {
        // Normalize desired with server-side defaults
        let desired_normalized = Self::normalize_with_server(desired, Some(client)).await?;

        // Normalize live without server call (it already has server defaults)
        let live_normalized = Self::normalize_with_server(live, None).await?;

        // Convert to YAML and generate unified diff
        let desired_yaml = serde_norway::to_string(&desired_normalized)
            .map_err(|e| crate::NylError::Config(format!("Failed to serialize resource to YAML: {}", e)))?;
        let live_yaml = serde_norway::to_string(&live_normalized)
            .map_err(|e| crate::NylError::Config(format!("Failed to serialize resource to YAML: {}", e)))?;

        // Generate unified diff
        let diff = TextDiff::from_lines(&live_yaml, &desired_yaml);
        let mut output = String::new();

        for hunk in diff.unified_diff().iter_hunks() {
            use std::fmt::Write;
            write!(&mut output, "{}", hunk.header()).unwrap();

            for change in hunk.iter_changes() {
                match change.tag() {
                    ChangeTag::Delete => {
                        output.push('-');
                        output.push_str(change.value());
                    }
                    ChangeTag::Insert => {
                        output.push('+');
                        output.push_str(change.value());
                    }
                    ChangeTag::Equal => {
                        output.push(' ');
                        output.push_str(change.value());
                    }
                }
            }
        }

        Ok(output)
    }

    /// Check if two resources are equivalent with server-side normalization
    ///
    /// Uses server-side apply dry-run to normalize the desired resource,
    /// then compares against the live resource.
    pub async fn are_equivalent_with_server(
        desired: &Value,
        live: &Value,
        client: &dyn crate::kubernetes::KubeClient,
    ) -> Result<bool> {
        // Normalize desired with server-side defaults
        let desired_normalized = Self::normalize_with_server(desired, Some(client)).await?;

        // Normalize live without server call
        let live_normalized = Self::normalize_with_server(live, None).await?;

        // Convert to YAML for comparison
        let desired_yaml = serde_norway::to_string(&desired_normalized)
            .map_err(|e| crate::NylError::Config(format!("Failed to serialize resource to YAML: {}", e)))?;
        let live_yaml = serde_norway::to_string(&live_normalized)
            .map_err(|e| crate::NylError::Config(format!("Failed to serialize resource to YAML: {}", e)))?;

        Ok(desired_yaml == live_yaml)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_removes_metadata_fields() {
        let mut resource = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test-config",
                "namespace": "default",
                "resourceVersion": "12345",
                "uid": "abc-123",
                "generation": 1,
                "creationTimestamp": "2024-01-01T00:00:00Z",
                "managedFields": [],
                "selfLink": "/api/v1/namespaces/default/configmaps/test-config"
            },
            "data": {
                "key": "value"
            }
        });

        DiffEngine::normalize(&mut resource);

        let metadata = resource["metadata"].as_object().unwrap();
        assert!(!metadata.contains_key("resourceVersion"));
        assert!(!metadata.contains_key("uid"));
        assert!(!metadata.contains_key("generation"));
        assert!(!metadata.contains_key("creationTimestamp"));
        assert!(!metadata.contains_key("managedFields"));
        assert!(!metadata.contains_key("selfLink"));

        // Should preserve name and namespace
        assert_eq!(metadata.get("name").unwrap(), "test-config");
        assert_eq!(metadata.get("namespace").unwrap(), "default");
    }

    #[test]
    fn test_normalize_removes_status() {
        let mut resource = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {
                "name": "test-deploy"
            },
            "spec": {
                "replicas": 3
            },
            "status": {
                "availableReplicas": 3,
                "conditions": []
            }
        });

        DiffEngine::normalize(&mut resource);

        assert!(!resource.as_object().unwrap().contains_key("status"));
        // Should preserve spec
        assert!(resource.as_object().unwrap().contains_key("spec"));
    }

    #[test]
    fn test_to_comparable_yaml() {
        let resource = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "resourceVersion": "12345"
            },
            "data": {
                "key": "value"
            }
        });

        let yaml = DiffEngine::to_comparable_yaml(&resource).unwrap();

        // Should not contain resourceVersion
        assert!(!yaml.contains("resourceVersion"));
        // Should contain name and data
        assert!(yaml.contains("name: test"));
        assert!(yaml.contains("key: value"));
    }

    #[test]
    fn test_are_equivalent_identical_resources() {
        let resource1 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            },
            "data": {
                "key": "value"
            }
        });

        let resource2 = resource1.clone();

        assert!(DiffEngine::are_equivalent(&resource1, &resource2).unwrap());
    }

    #[test]
    fn test_are_equivalent_ignores_auto_generated_fields() {
        let resource1 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "resourceVersion": "12345"
            },
            "data": {
                "key": "value"
            }
        });

        let resource2 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "resourceVersion": "67890"
            },
            "data": {
                "key": "value"
            }
        });

        // Should be equivalent despite different resourceVersion
        assert!(DiffEngine::are_equivalent(&resource1, &resource2).unwrap());
    }

    #[test]
    fn test_are_equivalent_different_data() {
        let resource1 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            },
            "data": {
                "key": "value1"
            }
        });

        let resource2 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            },
            "data": {
                "key": "value2"
            }
        });

        // Should NOT be equivalent
        assert!(!DiffEngine::are_equivalent(&resource1, &resource2).unwrap());
    }

    #[test]
    fn test_diff_yaml_shows_changes() {
        let live = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            },
            "data": {
                "key": "old-value"
            }
        });

        let desired = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            },
            "data": {
                "key": "new-value"
            }
        });

        let diff = DiffEngine::diff_yaml(&desired, &live).unwrap();

        // Should contain diff markers
        assert!(diff.contains('-') || diff.contains('+'));
        // Should show the change
        assert!(diff.contains("old-value") || diff.contains("new-value"));
    }

    #[test]
    fn test_diff_yaml_empty_for_equivalent() {
        let resource1 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "resourceVersion": "12345"
            },
            "data": {
                "key": "value"
            }
        });

        let resource2 = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "resourceVersion": "67890"
            },
            "data": {
                "key": "value"
            }
        });

        let diff = DiffEngine::diff_yaml(&resource1, &resource2).unwrap();

        // Should be empty for equivalent resources
        assert_eq!(diff, "");
    }

    #[tokio::test]
    async fn test_normalize_with_server_no_client() {
        let resource = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "resourceVersion": "12345"
            },
            "data": {
                "key": "value"
            }
        });

        let normalized = DiffEngine::normalize_with_server(&resource, None).await.unwrap();

        // Should not contain resourceVersion (client-side normalization applied)
        assert!(!normalized["metadata"]
            .as_object()
            .unwrap()
            .contains_key("resourceVersion"));
        // Should contain name and data
        assert_eq!(normalized["metadata"]["name"], "test");
        assert_eq!(normalized["data"]["key"], "value");
    }

    #[tokio::test]
    async fn test_normalize_with_server_with_mock_client() {
        use crate::kubernetes::MockKubeClient;

        let client = MockKubeClient::new();

        let resource = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default",
                "resourceVersion": "12345"
            },
            "data": {
                "key": "value"
            }
        });

        let normalized = DiffEngine::normalize_with_server(&resource, Some(&client))
            .await
            .unwrap();

        // Should not contain resourceVersion (client-side normalization applied)
        assert!(!normalized["metadata"]
            .as_object()
            .unwrap()
            .contains_key("resourceVersion"));
        // Should contain name and data
        assert_eq!(normalized["metadata"]["name"], "test");
        assert_eq!(normalized["data"]["key"], "value");
    }

    #[tokio::test]
    async fn test_are_equivalent_with_server() {
        use crate::kubernetes::MockKubeClient;

        let client = MockKubeClient::new();

        let desired = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "value"
            }
        });

        let live = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default",
                "resourceVersion": "12345",
                "uid": "abc-123"
            },
            "data": {
                "key": "value"
            }
        });

        // Should be equivalent despite different metadata
        assert!(DiffEngine::are_equivalent_with_server(&desired, &live, &client)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_are_equivalent_with_server_different_data() {
        use crate::kubernetes::MockKubeClient;

        let client = MockKubeClient::new();

        let desired = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "new-value"
            }
        });

        let live = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "old-value"
            }
        });

        // Should NOT be equivalent
        assert!(!DiffEngine::are_equivalent_with_server(&desired, &live, &client)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_diff_yaml_with_server_shows_changes() {
        use crate::kubernetes::MockKubeClient;

        let client = MockKubeClient::new();

        let desired = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "new-value"
            }
        });

        let live = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default",
                "resourceVersion": "12345"
            },
            "data": {
                "key": "old-value"
            }
        });

        let diff = DiffEngine::diff_yaml_with_server(&desired, &live, &client)
            .await
            .unwrap();

        // Should contain diff markers and the actual change
        assert!(diff.contains("new-value") || diff.contains("old-value"));
        assert!(diff.contains('-') || diff.contains('+'));
    }

    #[tokio::test]
    async fn test_diff_yaml_with_server_ignores_server_defaults() {
        use crate::kubernetes::MockKubeClient;

        let client = MockKubeClient::new();

        let desired = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default"
            },
            "data": {
                "key": "value"
            }
        });

        let live = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test",
                "namespace": "default",
                "resourceVersion": "12345",
                "uid": "abc-123",
                "generation": 1
            },
            "data": {
                "key": "value"
            }
        });

        let diff = DiffEngine::diff_yaml_with_server(&desired, &live, &client)
            .await
            .unwrap();

        // Should be empty (no differences in actual data)
        assert_eq!(diff, "");
    }
}
