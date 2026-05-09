/// RemoteManifest resource definition
///
/// A RemoteManifest fetches YAML/JSON documents from one or more HTTPS URLs and injects
/// them into the normal render pipeline.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::constants::API_VERSION;
use crate::resources::ObjectMetadata;
use crate::{NylError, Result};

/// RemoteManifest specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteManifestSpec {
    /// Single HTTPS URL to fetch (mutually exclusive with `urls`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// List of HTTPS URL templates to fetch (mutually exclusive with `url`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
    /// Parameters for `{key}` substitution in `urls` URL templates
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<HashMap<String, String>>,
    /// Overwrite fetched manifest metadata.namespace with RemoteManifest metadata.namespace
    #[serde(default, rename = "overrideNamespace", skip_serializing_if = "std::ops::Not::not")]
    pub override_namespace: bool,
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
        match (&self.spec.url, &self.spec.urls) {
            (Some(_), Some(_)) => {
                return Err(NylError::Config(
                    "RemoteManifest spec.url and spec.urls are mutually exclusive".to_string(),
                ))
            }
            (None, None) => {
                return Err(NylError::Config(
                    "RemoteManifest must specify either spec.url or spec.urls".to_string(),
                ))
            }
            (Some(url), None) => {
                if self.spec.params.is_some() {
                    return Err(NylError::Config(
                        "RemoteManifest spec.params requires spec.urls".to_string(),
                    ));
                }
                let url = url.trim();
                if url.is_empty() {
                    return Err(NylError::Config(
                        "RemoteManifest spec.url must not be empty".to_string(),
                    ));
                }
                if !url.starts_with("https://") {
                    return Err(NylError::Config(format!(
                        "RemoteManifest spec.url must use https:// scheme, got '{url}'"
                    )));
                }
            }
            (None, Some(urls)) => {
                if urls.is_empty() {
                    return Err(NylError::Config(
                        "RemoteManifest spec.urls must not be empty".to_string(),
                    ));
                }
                for url in urls {
                    let trimmed = url.trim();
                    let resolved = match &self.spec.params {
                        Some(params) => Self::apply_params(trimmed, params)?,
                        None => trimmed.to_string(),
                    };
                    if !resolved.starts_with("https://") {
                        return Err(NylError::Config(format!(
                            "RemoteManifest spec.urls entry must use https:// scheme, got '{url}'"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Substitute `{key}` placeholders in `template` with values from `params`.
    pub fn apply_params(template: &str, params: &HashMap<String, String>) -> Result<String> {
        let mut result = template.to_string();
        for (key, value) in params {
            result = result.replace(&format!("{{{key}}}"), value);
        }
        if let Some(start) = result.find('{') {
            if let Some(rel_end) = result[start..].find('}') {
                let placeholder = &result[start..=start + rel_end];
                return Err(NylError::Config(format!(
                    "Unresolved placeholder '{placeholder}' in RemoteManifest URL template '{template}'"
                )));
            }
            return Err(NylError::Config(format!(
                "Malformed placeholder (unclosed '{{') in RemoteManifest URL template '{template}'"
            )));
        }
        Ok(result)
    }

    /// Resolve the list of concrete HTTPS URLs to fetch.
    ///
    /// Callers must invoke `validate()` before calling this method.
    pub fn resolve_urls(&self) -> Result<Vec<String>> {
        if let Some(url) = &self.spec.url {
            Ok(vec![url.trim().to_string()])
        } else if let Some(urls) = &self.spec.urls {
            urls.iter()
                .map(|s| {
                    let trimmed = s.trim();
                    match &self.spec.params {
                        Some(params) => Self::apply_params(trimmed, params),
                        None => Ok(trimmed.to_string()),
                    }
                })
                .collect()
        } else {
            unreachable!("resolve_urls called on unvalidated RemoteManifest (neither url nor urls set)")
        }
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
    fn test_remote_manifest_validate_rejects_neither_url_nor_urls() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("spec.url or spec.urls"));
    }

    #[test]
    fn test_remote_manifest_validate_rejects_both_url_and_urls() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "url": "https://example.com/a.yaml",
                "urls": ["https://example.com/b.yaml"]
            }
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn test_remote_manifest_validate_rejects_params_without_urls() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "url": "https://example.com/a.yaml",
                "params": {"version": "1.0.0"}
            }
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("spec.params requires spec.urls"));
    }

    #[test]
    fn test_remote_manifest_validate_rejects_empty_urls() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"urls": []}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("spec.urls must not be empty"));
    }

    #[test]
    fn test_remote_manifest_validate_rejects_non_https_in_urls() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"urls": ["https://example.com/a.yaml", "http://example.com/b.yaml"]}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("https://"));
    }

    #[test]
    fn test_remote_manifest_validate_accepts_urls_with_params() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "params": {"version": "1.5.1"},
                "urls": [
                    "https://example.com/v{version}/a.yaml",
                    "https://example.com/v{version}/b.yaml"
                ]
            }
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        remote.validate().unwrap();
    }

    #[test]
    fn test_remote_manifest_validate_rejects_unresolved_placeholder_in_urls() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "params": {"version": "1.5.1"},
                "urls": ["https://example.com/{version}/{unknown}/a.yaml"]
            }
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let err = remote.validate().unwrap_err();
        assert!(err.to_string().contains("Unresolved placeholder"));
    }

    #[test]
    fn test_remote_manifest_override_namespace_defaults_to_false() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "https://example.com/manifests.yaml"}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        assert!(!remote.spec.override_namespace);
    }

    #[test]
    fn test_remote_manifest_override_namespace_true() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "url": "https://example.com/manifests.yaml",
                "overrideNamespace": true
            }
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        assert!(remote.spec.override_namespace);
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

    #[test]
    fn test_apply_params_substitutes_placeholders() {
        let params = HashMap::from([("version".to_string(), "1.5.1".to_string())]);
        let result = RemoteManifest::apply_params(
            "https://example.com/v{version}/crds.yaml",
            &params,
        )
        .unwrap();
        assert_eq!(result, "https://example.com/v1.5.1/crds.yaml");
    }

    #[test]
    fn test_apply_params_multiple_substitutions() {
        let params = HashMap::from([
            ("version".to_string(), "1.5.1".to_string()),
            ("name".to_string(), "gatewayclasses".to_string()),
        ]);
        let result = RemoteManifest::apply_params(
            "https://example.com/v{version}/{name}.yaml",
            &params,
        )
        .unwrap();
        assert_eq!(result, "https://example.com/v1.5.1/gatewayclasses.yaml");
    }

    #[test]
    fn test_apply_params_rejects_unresolved_placeholder() {
        let params = HashMap::from([("version".to_string(), "1.5.1".to_string())]);
        let err = RemoteManifest::apply_params(
            "https://example.com/v{version}/{unknown}.yaml",
            &params,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unresolved placeholder"));
        assert!(err.to_string().contains("{unknown}"));
    }

    #[test]
    fn test_apply_params_rejects_unclosed_brace() {
        let params = HashMap::from([("version".to_string(), "1.5.1".to_string())]);
        let err = RemoteManifest::apply_params(
            "https://example.com/v{version}/{unclosed",
            &params,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Malformed placeholder"));
    }

    #[test]
    fn test_resolve_urls_from_url() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"url": "https://example.com/manifests.yaml"}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let urls = remote.resolve_urls().unwrap();
        assert_eq!(urls, vec!["https://example.com/manifests.yaml"]);
    }

    #[test]
    fn test_resolve_urls_trims_whitespace() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {"urls": ["  https://example.com/a.yaml  ", " https://example.com/b.yaml "]}
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let urls = remote.resolve_urls().unwrap();
        assert_eq!(urls, vec![
            "https://example.com/a.yaml",
            "https://example.com/b.yaml",
        ]);
    }

    #[test]
    fn test_resolve_urls_from_urls_with_params() {
        let manifest = json!({
            "apiVersion": "nyl.niklasrosenstein.github.com/v1",
            "kind": "RemoteManifest",
            "metadata": {"name": "remote"},
            "spec": {
                "params": {"version": "1.5.1"},
                "urls": [
                    "https://example.com/v{version}/a.yaml",
                    "https://example.com/v{version}/b.yaml"
                ]
            }
        });
        let remote = RemoteManifest::from_value(&manifest).unwrap();
        let urls = remote.resolve_urls().unwrap();
        assert_eq!(urls, vec![
            "https://example.com/v1.5.1/a.yaml",
            "https://example.com/v1.5.1/b.yaml",
        ]);
    }
}
