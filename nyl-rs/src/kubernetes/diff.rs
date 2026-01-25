//! kubectl-style YAML diff engine for Kubernetes resources
//!
//! This module provides functionality to compare Kubernetes resources by:
//! 1. Normalizing resources (removing auto-generated fields)
//! 2. Converting to YAML with consistent formatting
//! 3. Generating unified diffs (like kubectl diff)

use crate::Result;
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
        serde_norway::to_string(&normalized).map_err(|e| {
            crate::NylError::Config(format!("Failed to serialize resource to YAML: {}", e))
        })
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
            output.push_str(&format!("{}", hunk.header()));

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
}
