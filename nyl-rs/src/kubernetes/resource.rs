use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

use crate::{NylError, Result};

/// Group, Version, Kind tuple for Kubernetes resources
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupVersionKind {
    pub group: String,
    pub version: String,
    pub kind: String,
}

impl GroupVersionKind {
    /// Parse GVK from apiVersion and kind fields
    /// apiVersion can be "v1" or "apps/v1"
    pub fn from_api_version_kind(api_version: &str, kind: &str) -> Result<Self> {
        let (group, version) = if api_version.contains('/') {
            let parts: Vec<&str> = api_version.splitn(2, '/').collect();
            (parts[0].to_string(), parts[1].to_string())
        } else {
            ("".to_string(), api_version.to_string())
        };

        Ok(Self {
            group,
            version,
            kind: kind.to_string(),
        })
    }

    /// Convert to apiVersion string (e.g., "v1" or "apps/v1")
    pub fn to_api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.clone()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    /// Convert to full string representation (e.g., "v1/ConfigMap" or "apps/v1/Deployment")
    pub fn to_string(&self) -> String {
        format!("{}/{}", self.to_api_version(), self.kind)
    }
}

impl Hash for GroupVersionKind {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.group.hash(state);
        self.version.hash(state);
        self.kind.hash(state);
    }
}

/// Unique identifier for a Kubernetes resource
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceKey {
    pub gvk: GroupVersionKind,
    pub namespace: Option<String>,
    pub name: String,
}

impl ResourceKey {
    /// Extract ResourceKey from a JSON manifest
    pub fn from_json_value(value: &serde_json::Value) -> Result<Self> {
        let api_version = extract_api_version(value)?;
        let kind = extract_kind(value)?;
        let gvk = GroupVersionKind::from_api_version_kind(&api_version, &kind)?;

        let name = extract_name(value)?;
        let namespace = extract_namespace(value);

        Ok(Self {
            gvk,
            namespace,
            name,
        })
    }

    /// Convert to string representation (e.g., "ConfigMap default/myconfig")
    pub fn to_string(&self) -> String {
        let ns_name = if let Some(ns) = &self.namespace {
            format!("{}/{}", ns, self.name)
        } else {
            self.name.clone()
        };
        format!("{} {}", self.gvk.kind, ns_name)
    }
}

/// Outcome of applying a resource
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Resource was created
    Created {
        name: String,
        namespace: Option<String>,
    },
    /// Resource was updated
    Updated {
        name: String,
        namespace: Option<String>,
    },
    /// Resource was unchanged
    Unchanged {
        name: String,
        namespace: Option<String>,
    },
    /// Dry run mode - shows what would happen
    DryRun {
        would_be: Box<ApplyOutcome>,
    },
}

impl ApplyOutcome {
    /// Get the resource name
    pub fn name(&self) -> &str {
        match self {
            ApplyOutcome::Created { name, .. } => name,
            ApplyOutcome::Updated { name, .. } => name,
            ApplyOutcome::Unchanged { name, .. } => name,
            ApplyOutcome::DryRun { would_be } => would_be.name(),
        }
    }

    /// Get the resource namespace
    pub fn namespace(&self) -> Option<&str> {
        match self {
            ApplyOutcome::Created { namespace, .. } => namespace.as_deref(),
            ApplyOutcome::Updated { namespace, .. } => namespace.as_deref(),
            ApplyOutcome::Unchanged { namespace, .. } => namespace.as_deref(),
            ApplyOutcome::DryRun { would_be } => would_be.namespace(),
        }
    }

    /// Check if this is a dry run outcome
    pub fn is_dry_run(&self) -> bool {
        matches!(self, ApplyOutcome::DryRun { .. })
    }
}

// Helper functions for extracting fields from manifests

/// Extract apiVersion from a manifest
pub fn extract_api_version(value: &serde_json::Value) -> Result<String> {
    value
        .get("apiVersion")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| NylError::Config("Manifest missing apiVersion field".to_string()))
}

/// Extract kind from a manifest
pub fn extract_kind(value: &serde_json::Value) -> Result<String> {
    value
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| NylError::Config("Manifest missing kind field".to_string()))
}

/// Extract name from a manifest
pub fn extract_name(value: &serde_json::Value) -> Result<String> {
    value
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| NylError::Config("Manifest missing metadata.name field".to_string()))
}

/// Extract namespace from a manifest (returns None for cluster-scoped resources)
pub fn extract_namespace(value: &serde_json::Value) -> Option<String> {
    value
        .get("metadata")
        .and_then(|m| m.get("namespace"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract GVK from a manifest
pub fn extract_gvk(value: &serde_json::Value) -> Result<GroupVersionKind> {
    let api_version = extract_api_version(value)?;
    let kind = extract_kind(value)?;
    GroupVersionKind::from_api_version_kind(&api_version, &kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_gvk_from_api_version_core() {
        let gvk = GroupVersionKind::from_api_version_kind("v1", "ConfigMap").unwrap();
        assert_eq!(gvk.group, "");
        assert_eq!(gvk.version, "v1");
        assert_eq!(gvk.kind, "ConfigMap");
    }

    #[test]
    fn test_gvk_from_api_version_with_group() {
        let gvk = GroupVersionKind::from_api_version_kind("apps/v1", "Deployment").unwrap();
        assert_eq!(gvk.group, "apps");
        assert_eq!(gvk.version, "v1");
        assert_eq!(gvk.kind, "Deployment");
    }

    #[test]
    fn test_gvk_to_api_version_core() {
        let gvk = GroupVersionKind {
            group: "".to_string(),
            version: "v1".to_string(),
            kind: "ConfigMap".to_string(),
        };
        assert_eq!(gvk.to_api_version(), "v1");
    }

    #[test]
    fn test_gvk_to_api_version_with_group() {
        let gvk = GroupVersionKind {
            group: "apps".to_string(),
            version: "v1".to_string(),
            kind: "Deployment".to_string(),
        };
        assert_eq!(gvk.to_api_version(), "apps/v1");
    }

    #[test]
    fn test_gvk_to_string() {
        let gvk = GroupVersionKind {
            group: "apps".to_string(),
            version: "v1".to_string(),
            kind: "Deployment".to_string(),
        };
        assert_eq!(gvk.to_string(), "apps/v1/Deployment");
    }

    #[test]
    fn test_resource_key_from_json_namespaced() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "myconfig",
                "namespace": "default"
            }
        });

        let key = ResourceKey::from_json_value(&manifest).unwrap();
        assert_eq!(key.gvk.kind, "ConfigMap");
        assert_eq!(key.name, "myconfig");
        assert_eq!(key.namespace, Some("default".to_string()));
    }

    #[test]
    fn test_resource_key_from_json_cluster_scoped() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": {
                "name": "mynamespace"
            }
        });

        let key = ResourceKey::from_json_value(&manifest).unwrap();
        assert_eq!(key.gvk.kind, "Namespace");
        assert_eq!(key.name, "mynamespace");
        assert_eq!(key.namespace, None);
    }

    #[test]
    fn test_extract_api_version() {
        let manifest = json!({"apiVersion": "apps/v1"});
        assert_eq!(extract_api_version(&manifest).unwrap(), "apps/v1");
    }

    #[test]
    fn test_extract_kind() {
        let manifest = json!({"kind": "Deployment"});
        assert_eq!(extract_kind(&manifest).unwrap(), "Deployment");
    }

    #[test]
    fn test_extract_name() {
        let manifest = json!({"metadata": {"name": "test"}});
        assert_eq!(extract_name(&manifest).unwrap(), "test");
    }

    #[test]
    fn test_extract_namespace_present() {
        let manifest = json!({"metadata": {"namespace": "default"}});
        assert_eq!(extract_namespace(&manifest), Some("default".to_string()));
    }

    #[test]
    fn test_extract_namespace_missing() {
        let manifest = json!({"metadata": {"name": "test"}});
        assert_eq!(extract_namespace(&manifest), None);
    }

    #[test]
    fn test_extract_gvk() {
        let manifest = json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment"
        });

        let gvk = extract_gvk(&manifest).unwrap();
        assert_eq!(gvk.group, "apps");
        assert_eq!(gvk.version, "v1");
        assert_eq!(gvk.kind, "Deployment");
    }

    #[test]
    fn test_resource_key_to_string_namespaced() {
        let key = ResourceKey {
            gvk: GroupVersionKind {
                group: "".to_string(),
                version: "v1".to_string(),
                kind: "ConfigMap".to_string(),
            },
            namespace: Some("default".to_string()),
            name: "myconfig".to_string(),
        };
        assert_eq!(key.to_string(), "ConfigMap default/myconfig");
    }

    #[test]
    fn test_resource_key_to_string_cluster_scoped() {
        let key = ResourceKey {
            gvk: GroupVersionKind {
                group: "".to_string(),
                version: "v1".to_string(),
                kind: "Namespace".to_string(),
            },
            namespace: None,
            name: "mynamespace".to_string(),
        };
        assert_eq!(key.to_string(), "Namespace mynamespace");
    }

    #[test]
    fn test_apply_outcome_name() {
        let outcome = ApplyOutcome::Created {
            name: "test".to_string(),
            namespace: Some("default".to_string()),
        };
        assert_eq!(outcome.name(), "test");
    }

    #[test]
    fn test_apply_outcome_namespace() {
        let outcome = ApplyOutcome::Updated {
            name: "test".to_string(),
            namespace: Some("default".to_string()),
        };
        assert_eq!(outcome.namespace(), Some("default"));
    }

    #[test]
    fn test_apply_outcome_is_dry_run() {
        let outcome = ApplyOutcome::DryRun {
            would_be: Box::new(ApplyOutcome::Created {
                name: "test".to_string(),
                namespace: None,
            }),
        };
        assert!(outcome.is_dry_run());

        let outcome2 = ApplyOutcome::Created {
            name: "test".to_string(),
            namespace: None,
        };
        assert!(!outcome2.is_dry_run());
    }
}
