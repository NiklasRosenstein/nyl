use std::collections::BTreeSet;
use std::path::{Component, Path};

use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Deserializer, Serialize};

use crate::config::StripEmptyMetadataLabelsMode;
use crate::constants::API_VERSION_GITOPS;
use crate::{NylError, Result};

pub const KIND_RELEASE: &str = "Release";
pub const RELEASE_SCHEMA_FILENAME: &str = "release.schema.json";

/// Release resource for specifying release metadata
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Release {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: ReleaseMetadata,
    #[serde(default)]
    pub spec: ReleaseSpec,
}

/// Metadata for Release
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMetadata {
    /// Release name
    pub name: String,
    /// Target namespace for the release
    pub namespace: String,
}

/// Rendering and GitOps options for a Release.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSpec {
    /// Control when empty `metadata.labels` maps are stripped from emitted manifests.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "stripEmptyMetadataLabels"
    )]
    pub strip_empty_metadata_labels: Option<StripEmptyMetadataLabelsMode>,

    /// Additional namespaces that rendered resources may explicitly target.
    #[serde(default, rename = "additionalNamespaces", skip_serializing_if = "Vec::is_empty")]
    pub additional_namespaces: Vec<String>,

    /// Additional manifest files, resolved relative to the Release file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

    /// ArgoCD-specific options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argocd: Option<ReleaseArgoCdSpec>,
}

/// ArgoCD-specific options for Release.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArgoCdSpec {
    /// Optional partial ArgoCD Application override.
    ///
    /// Must be an object if provided.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "applicationOverride",
        deserialize_with = "deserialize_optional_object"
    )]
    pub application_override: Option<serde_json::Map<String, serde_json::Value>>,
}

fn deserialize_optional_object<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Map<String, serde_json::Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(serde::de::Error::custom(
            "applicationOverride must be a YAML/JSON object",
        )),
    }
}

impl Release {
    /// Check if a manifest is a Release resource
    pub fn is_release(manifest: &serde_json::Value) -> bool {
        manifest.get("apiVersion").and_then(|v| v.as_str()) == Some(API_VERSION_GITOPS)
            && manifest.get("kind").and_then(|v| v.as_str()) == Some(KIND_RELEASE)
    }

    /// Parse Release from JSON value
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let release: Self = serde_json::from_value(value.clone())
            .map_err(|e| NylError::Config(format!("Invalid Release resource: {e}")))?;
        release.validate()?;
        Ok(release)
    }

    pub fn validate(&self) -> Result<()> {
        if self.api_version != API_VERSION_GITOPS || self.kind != KIND_RELEASE {
            return Err(NylError::config("Invalid Release resource envelope"));
        }
        validate_namespace_name("metadata.namespace", &self.metadata.namespace)?;
        let mut namespaces = BTreeSet::new();
        for namespace in &self.spec.additional_namespaces {
            validate_namespace_name("spec.additionalNamespaces", namespace)?;
            if !namespaces.insert(namespace) {
                return Err(NylError::config(format!(
                    "spec.additionalNamespaces contains duplicate namespace {namespace:?}"
                )));
            }
        }
        for pattern in &self.spec.include {
            validate_include_pattern(pattern)?;
        }
        Ok(())
    }
}

pub fn generate_release_schema() -> serde_json::Value {
    let mut schema = serde_json::to_value(schema_for!(Release)).expect("schema serialization should never fail");
    let properties = schema
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .expect("Release schema should have object properties");
    properties.insert(
        "apiVersion".to_owned(),
        serde_json::json!({"const": API_VERSION_GITOPS, "type": "string"}),
    );
    properties.insert(
        "kind".to_owned(),
        serde_json::json!({"const": KIND_RELEASE, "type": "string"}),
    );
    schema
}

pub(crate) fn validate_namespace_name(field: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(NylError::config(format!("{field} must be a Kubernetes namespace name")))
    }
}

fn validate_include_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty()
        || pattern.contains('\\')
        || Path::new(pattern).is_absolute()
        || Path::new(pattern).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(NylError::config(format!(
            "Release spec.include pattern {pattern:?} must stay within the Release directory"
        )));
    }
    glob::Pattern::new(pattern)
        .map(|_| ())
        .map_err(|error| NylError::config(format!("Invalid Release spec.include pattern {pattern:?}: {error}")))
}

/// Extract Release metadata and filter it from manifests
///
/// Returns a tuple of (optional Release, filtered manifests without Release)
pub fn extract_release(manifests: &[serde_json::Value]) -> Result<(Option<Release>, Vec<serde_json::Value>)> {
    let mut release = None;
    let mut filtered = Vec::new();

    for manifest in manifests {
        if Release::is_release(manifest) {
            if release.is_some() {
                return Err(NylError::Config("Multiple Release resources found in file".to_string()));
            }
            release = Some(Release::from_value(manifest)?);
        } else {
            filtered.push(manifest.clone());
        }
    }

    Ok((release, filtered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_is_release_true() {
        let manifest = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {
                "name": "test",
                "namespace": "default"
            }
        });

        assert!(Release::is_release(&manifest));
    }

    #[test]
    fn test_is_release_false_wrong_kind() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "test"
            }
        });

        assert!(!Release::is_release(&manifest));
    }

    #[test]
    fn test_is_release_false_wrong_api_version() {
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "Release",
            "metadata": {
                "name": "test"
            }
        });

        assert!(!Release::is_release(&manifest));
    }

    #[test]
    fn test_from_value_valid() {
        let value = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {
                "name": "myapp",
                "namespace": "production"
            }
        });

        let release = Release::from_value(&value).unwrap();
        assert_eq!(release.api_version, "gitops.nyl/v1");
        assert_eq!(release.kind, "Release");
        assert_eq!(release.metadata.name, "myapp");
        assert_eq!(release.metadata.namespace, "production");
    }

    #[test]
    fn test_from_value_with_spec() {
        let value = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {
                "name": "myapp",
                "namespace": "production"
            },
            "spec": {}
        });

        let release = Release::from_value(&value).unwrap();
        assert_eq!(release.metadata.name, "myapp");
    }

    #[test]
    fn release_parses_bundle_and_namespace_scope() {
        let release = Release::from_value(&json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {"name": "myapp", "namespace": "production"},
            "spec": {
                "additionalNamespaces": ["monitoring"],
                "include": ["manifests/**/*.yaml"]
            }
        }))
        .unwrap();
        assert_eq!(release.spec.additional_namespaces, ["monitoring"]);
        assert_eq!(release.spec.include, ["manifests/**/*.yaml"]);
    }

    #[test]
    fn release_rejects_duplicate_namespaces_and_include_traversal() {
        let duplicate = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {"name": "myapp", "namespace": "production"},
            "spec": {"additionalNamespaces": ["monitoring", "monitoring"]}
        });
        assert!(Release::from_value(&duplicate)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        let traversal = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {"name": "myapp", "namespace": "production"},
            "spec": {"include": ["../shared.yaml"]}
        });
        assert!(Release::from_value(&traversal)
            .unwrap_err()
            .to_string()
            .contains("Release directory"));
    }

    #[test]
    fn test_from_value_with_application_override() {
        let value = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release",
            "metadata": {
                "name": "myapp",
                "namespace": "production"
            },
            "spec": {
                "stripEmptyMetadataLabels": "never",
                "argocd": {
                    "applicationOverride": {
                        "spec": {
                            "syncPolicy": {
                                "automated": {
                                    "prune": true
                                }
                            }
                        }
                    }
                }
            }
        });

        let release = Release::from_value(&value).unwrap();
        let application_override = release
            .spec
            .argocd
            .as_ref()
            .and_then(|a| a.application_override.clone())
            .expect("applicationOverride should be parsed");
        assert!(application_override.contains_key("spec"));
        assert_eq!(
            release.spec.strip_empty_metadata_labels,
            Some(StripEmptyMetadataLabelsMode::Never)
        );
    }

    #[test]
    fn test_from_value_invalid_missing_metadata() {
        let value = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "Release"
        });

        assert!(Release::from_value(&value).is_err());
    }

    #[test]
    fn test_extract_release_with_release() {
        let manifests = vec![
            json!({
                "apiVersion": "gitops.nyl/v1",
                "kind": "Release",
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

        let (release, filtered) = extract_release(&manifests).unwrap();
        assert!(release.is_some());
        assert_eq!(release.unwrap().metadata.name, "myapp");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["kind"], "ConfigMap");
    }

    #[test]
    fn test_extract_release_without_release() {
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

        let (release, filtered) = extract_release(&manifests).unwrap();
        assert!(release.is_none());
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_extract_release_multiple_error() {
        let manifests = vec![
            json!({
                "apiVersion": "gitops.nyl/v1",
                "kind": "Release",
                "metadata": {
                    "name": "app1",
                    "namespace": "default"
                }
            }),
            json!({
                "apiVersion": "gitops.nyl/v1",
                "kind": "Release",
                "metadata": {
                    "name": "app2",
                    "namespace": "default"
                }
            }),
        ];

        let result = extract_release(&manifests);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Multiple Release"));
    }

    #[test]
    fn test_release_rejects_unknown_fields() {
        let yaml = r"
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: test
  namespace: default
unknownField: should-fail
";
        let result: std::result::Result<Release, _> = serde_norway::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown field"));
    }

    #[test]
    fn test_release_rejects_non_object_application_override() {
        let yaml = r"
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: test
  namespace: default
spec:
  argocd:
    applicationOverride: hello
";
        let result: std::result::Result<Release, _> = serde_norway::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("applicationOverride must be a YAML/JSON object"));
    }

    #[test]
    fn test_release_parses_strip_empty_metadata_labels_override() {
        let yaml = r"
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: test
  namespace: default
spec:
  stripEmptyMetadataLabels: argocd
";
        let release: Release = serde_norway::from_str(yaml).unwrap();
        assert_eq!(
            release.spec.strip_empty_metadata_labels,
            Some(StripEmptyMetadataLabelsMode::Argocd)
        );
    }
}
