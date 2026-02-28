/// RemoteManifest resource definition
///
/// A RemoteManifest fetches YAML/JSON documents from an HTTPS URL and injects
/// them into the normal render pipeline.
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION;
use crate::resources::ObjectMetadata;
use crate::{NylError, Result};

/// RemoteManifest specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteManifestSpec {
    /// HTTPS URL to fetch manifest documents from
    pub url: String,
}

/// RemoteManifest resource
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteManifest {
    /// API version (must be core Nyl API version)
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    /// Resource kind (always "RemoteManifest")
    pub kind: String,
    /// Kubernetes metadata
    pub metadata: ObjectMetadata,
    /// Remote manifest source configuration
    pub spec: RemoteManifestSpec,
}

impl RemoteManifest {
    /// Check if a manifest is a RemoteManifest resource
    pub fn is_remote_manifest(manifest: &serde_json::Value) -> bool {
        manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION)
            && manifest.get("kind").and_then(|v| v.as_str()) == Some("RemoteManifest")
    }

    /// Parse a RemoteManifest from a JSON value
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| NylError::Config(format!("Invalid RemoteManifest resource: {}", e)))
    }

    /// Validate the RemoteManifest configuration
    pub fn validate(&self) -> Result<()> {
        let url = self.spec.url.trim();
        if url.is_empty() {
            return Err(NylError::Config(
                "RemoteManifest spec.url must not be empty".to_string(),
            ));
        }
        if !url.starts_with("https://") {
            return Err(NylError::Config(format!(
                "RemoteManifest spec.url must use https:// scheme, got '{}'",
                self.spec.url
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_remote_manifest_true() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "https://example.com/manifests.yaml"}
        });
        assert!(RemoteManifest::is_remote_manifest(&manifest));
    }

    #[test]
    fn test_is_remote_manifest_false() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "cm"}
        });
        assert!(!RemoteManifest::is_remote_manifest(&manifest));
    }

    #[test]
    fn test_remote_manifest_validate_rejects_empty_url() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "   "}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn test_remote_manifest_validate_rejects_non_https_url() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "http://example.com/manifests.yaml"}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn test_remote_manifest_from_value_rejects_unknown_fields() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "https://example.com/manifests.yaml", "unknownField": true}
        });
        let err = RemoteManifest::from_value(&manifest).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }
}
