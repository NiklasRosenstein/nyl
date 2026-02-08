/// NylRelease resource for specifying release metadata
///
/// This is an optional resource that can be included in YAML files to specify
/// release metadata. When present, it provides the release name and namespace.
/// When absent, these values must be provided via CLI flags.
use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION;
use crate::{NylError, Result};

/// NylRelease resource for specifying release metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NylRelease {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: NylReleaseMetadata,
    #[serde(default)]
    pub spec: NylReleaseSpec,
}

/// Metadata for NylRelease
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NylReleaseMetadata {
    /// Release name
    pub name: String,
    /// Target namespace for the release
    pub namespace: String,
}

/// Spec for NylRelease (currently empty, reserved for future use)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NylReleaseSpec {
    // Future: additional metadata like labels, annotations
}

impl NylRelease {
    /// Check if a manifest is a NylRelease resource
    pub fn is_nyl_release(manifest: &serde_json::Value) -> bool {
        manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION)
            && manifest.get("kind").and_then(|v| v.as_str()) == Some("NylRelease")
    }

    /// Parse NylRelease from JSON value
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(value.clone())
            .map_err(|e| NylError::Config(format!("Invalid NylRelease resource: {}", e)))
    }
}

/// Extract NylRelease metadata and filter it from manifests
///
/// Returns a tuple of (optional NylRelease, filtered manifests without NylRelease)
pub fn extract_nyl_release(manifests: &[serde_json::Value]) -> Result<(Option<NylRelease>, Vec<serde_json::Value>)> {
    let mut nyl_release = None;
    let mut filtered = Vec::new();

    for manifest in manifests {
        if NylRelease::is_nyl_release(manifest) {
            if nyl_release.is_some() {
                return Err(NylError::Config(
                    "Multiple NylRelease resources found in file".to_string(),
                ));
            }
            nyl_release = Some(NylRelease::from_value(manifest)?);
        } else {
            filtered.push(manifest.clone());
        }
    }

    Ok((nyl_release, filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_nyl_release_true() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "NylRelease",
            "metadata": {
                "name": "test",
                "namespace": "default"
            }
        });

        assert!(NylRelease::is_nyl_release(&manifest));
    }

    #[test]
    fn test_is_nyl_release_false_wrong_kind() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            }
        });

        assert!(!NylRelease::is_nyl_release(&manifest));
    }

    #[test]
    fn test_is_nyl_release_false_wrong_api_version() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "NylRelease",
            "metadata": {
                "name": "test"
            }
        });

        assert!(!NylRelease::is_nyl_release(&manifest));
    }

    #[test]
    fn test_from_value_valid() {
        let value = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "NylRelease",
            "metadata": {
                "name": "myapp",
                "namespace": "production"
            }
        });

        let release = NylRelease::from_value(&value).unwrap();
        assert_eq!(release.api_version, "nyl.niklasrosenstein.github.com/v1");
        assert_eq!(release.kind, "NylRelease");
        assert_eq!(release.metadata.name, "myapp");
        assert_eq!(release.metadata.namespace, "production");
    }

    #[test]
    fn test_from_value_with_spec() {
        let value = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "NylRelease",
            "metadata": {
                "name": "myapp",
                "namespace": "production"
            },
            "spec": {}
        });

        let release = NylRelease::from_value(&value).unwrap();
        assert_eq!(release.metadata.name, "myapp");
    }

    #[test]
    fn test_from_value_invalid_missing_metadata() {
        let value = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "NylRelease"
        });

        assert!(NylRelease::from_value(&value).is_err());
    }

    #[test]
    fn test_extract_nyl_release_with_release() {
        let manifests = vec![
            json!({
                "apiVersion": "nyl.niklasrosenstein.github.com/v1",
                "kind": "NylRelease",
                "metadata": {
                    "name": "myapp",
                    "namespace": "default"
                }
            }),
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": "test"
                }
            }),
        ];

        let (release, filtered) = extract_nyl_release(&manifests).unwrap();
        assert!(release.is_some());
        assert_eq!(release.unwrap().metadata.name, "myapp");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_extract_nyl_release_without_release() {
        let manifests = vec![
            json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": "test1"
                }
            }),
            json!({
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {
                    "name": "test2"
                }
            }),
        ];

        let (release, filtered) = extract_nyl_release(&manifests).unwrap();
        assert!(release.is_none());
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_extract_nyl_release_multiple_error() {
        let manifests = vec![
            json!({
                "apiVersion": "nyl.niklasrosenstein.github.com/v1",
                "kind": "NylRelease",
                "metadata": {
                    "name": "app1",
                    "namespace": "default"
                }
            }),
            json!({
                "apiVersion": "nyl.niklasrosenstein.github.com/v1",
                "kind": "NylRelease",
                "metadata": {
                    "name": "app2",
                    "namespace": "default"
                }
            }),
        ];

        let result = extract_nyl_release(&manifests);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Multiple NylRelease"));
    }

    #[test]
    fn test_nyl_release_rejects_unknown_fields() {
        let yaml = r"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: test
  namespace: default
unknownField: should-fail
";
        let result: std::result::Result<NylRelease, _> = serde_norway::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown field"));
    }
}
