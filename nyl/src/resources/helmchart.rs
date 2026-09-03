/// HelmChart resource definition
///
/// Represents a declarative Helm chart deployment
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::constants::API_VERSION;

/// Reference to a Helm chart
///
/// Supports Git, OCI, traditional Helm repositories, and local filesystem paths
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ChartRef {
    /// Repository URL for Git, OCI, or traditional Helm repositories
    ///
    /// Protocol prefixes indicate the repository type:
    /// - `git+https://` or `git+git@` - Git repository
    /// - `oci://` - OCI registry
    /// - `https://` or no prefix - Traditional Helm repository
    ///
    /// Examples:
    /// - Git: "git+https://github.com/user/repo.git"
    /// - Git SSH: "git+git@github.com:user/repo.git"
    /// - OCI: "oci://ghcr.io/owner/chart"
    /// - Helm: "https://charts.example.com"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,

    /// Universal name field with context-dependent meaning
    ///
    /// When `repository` is set:
    /// - For Helm/OCI repositories: Specifies the chart name (required)
    /// - For Git repositories: Specifies subdirectory path within repo (optional)
    ///
    /// When `repository` is NOT set:
    /// - Treated as local filesystem path (absolute or relative)
    ///
    /// Examples:
    /// - Local filesystem: "./charts/mychart", "/opt/charts/app"
    /// - Git subpath: "charts/mychart", "deploy/helm/app"
    /// - Helm/OCI chart: "nginx", "prometheus"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Chart version or Git reference
    ///
    /// For Helm repositories: Specifies the chart version (required)
    /// For Git repositories: Specifies the Git reference - branch, tag, or commit
    ///                       (optional, defaults to HEAD)
    ///
    /// Examples:
    /// - Branch: "main", "develop"
    /// - Tag: "v1.0.0", "1.2.3"
    /// - Commit: "abc123..." (full or short hash)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Kubernetes object metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectMetadata {
    /// Resource name
    pub name: String,

    /// Namespace (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// Labels
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,

    /// Annotations
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

impl ObjectMetadata {
    /// Create minimal metadata with just a name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        }
    }
}

/// HelmChart specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelmChartSpec {
    /// Reference to the chart
    pub chart: ChartRef,

    /// Values to pass to the chart
    #[serde(default)]
    pub values: serde_json::Value,

    /// Whether to include CRDs from the chart's crds/ directory in the rendered output.
    /// Defaults to true when unset.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "includeCrds")]
    pub include_crds: Option<bool>,
}

impl Default for HelmChartSpec {
    fn default() -> Self {
        Self {
            chart: ChartRef::default(),
            values: serde_json::Value::Object(serde_json::Map::new()),
            include_crds: None,
        }
    }
}

/// HelmChart resource
///
/// This is a Kubernetes-style resource that represents a Helm chart deployment
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelmChart {
    /// API version (e.g., "nyl.niklasrosenstein.github.com/v1")
    #[serde(rename = "apiVersion")]
    pub api_version: String,

    /// Resource kind (always "HelmChart")
    pub kind: String,

    /// Kubernetes metadata
    pub metadata: ObjectMetadata,

    /// Chart specification
    pub spec: HelmChartSpec,
}

impl HelmChart {
    /// Create a new HelmChart resource
    pub fn new(name: impl Into<String>, chart_ref: ChartRef) -> Self {
        Self {
            api_version: API_VERSION.to_string(),
            kind: "HelmChart".to_string(),
            metadata: ObjectMetadata::new(name),
            spec: HelmChartSpec {
                chart: chart_ref,
                ..Default::default()
            },
        }
    }

    /// Set the values
    #[must_use]
    pub fn with_values(mut self, values: serde_json::Value) -> Self {
        self.spec.values = values;
        self
    }

    /// Set the namespace
    #[must_use]
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.metadata.namespace = Some(namespace.into());
        self
    }

    /// Get the release name (from metadata)
    pub fn release_name(&self) -> &str {
        &self.metadata.name
    }

    /// Get the release namespace (from metadata)
    pub fn release_namespace(&self) -> Option<&str> {
        self.metadata.namespace.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chart_ref_local_path() {
        let chart_ref = ChartRef {
            name: Some("./charts/mychart".to_string()),
            ..Default::default()
        };

        let yaml = serde_norway::to_string(&chart_ref).unwrap();
        assert!(yaml.contains("name: ./charts/mychart"));

        let deserialized: ChartRef = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(deserialized.name, Some("./charts/mychart".to_string()));
    }

    #[test]
    fn test_chart_ref_repository() {
        let chart_ref = ChartRef {
            repository: Some("https://charts.example.com".to_string()),
            name: Some("nginx".to_string()),
            version: Some("1.0.0".to_string()),
        };

        let yaml = serde_norway::to_string(&chart_ref).unwrap();
        assert!(yaml.contains("repository"));
        assert!(yaml.contains("nginx"));
    }

    #[test]
    fn test_object_metadata() {
        let mut metadata = ObjectMetadata::new("my-app");
        metadata.namespace = Some("default".to_string());
        metadata.labels.insert("app".to_string(), "web".to_string());

        assert_eq!(metadata.name, "my-app");
        assert_eq!(metadata.namespace, Some("default".to_string()));
        assert_eq!(metadata.labels.get("app").unwrap(), "web");
    }

    #[test]
    fn test_helm_chart_new() {
        let chart_ref = ChartRef {
            name: Some("./charts/app".to_string()),
            ..Default::default()
        };

        let helm_chart = HelmChart::new("my-app", chart_ref);

        assert_eq!(helm_chart.api_version, "nyl.niklasrosenstein.github.com/v1");
        assert_eq!(helm_chart.kind, "HelmChart");
        assert_eq!(helm_chart.metadata.name, "my-app");
        assert_eq!(helm_chart.spec.chart.name, Some("./charts/app".to_string()));
    }

    #[test]
    fn test_helm_chart_builder() {
        let chart_ref = ChartRef {
            name: Some("./charts/app".to_string()),
            ..Default::default()
        };

        let values = serde_json::json!({"replicas": 3});

        let helm_chart = HelmChart::new("my-app", chart_ref)
            .with_namespace("production")
            .with_values(values.clone());

        assert_eq!(helm_chart.release_name(), "my-app");
        assert_eq!(helm_chart.release_namespace(), Some("production"));
        assert_eq!(helm_chart.spec.values, values);
    }

    #[test]
    fn test_helm_chart_serialization() {
        let chart_ref = ChartRef {
            name: Some("./charts/nginx".to_string()),
            ..Default::default()
        };

        let values = serde_json::json!({
            "replicaCount": 2,
            "image": {
                "repository": "nginx",
                "tag": "1.21"
            }
        });

        let helm_chart = HelmChart::new("nginx-app", chart_ref)
            .with_namespace("default")
            .with_values(values);

        let yaml = serde_norway::to_string(&helm_chart).unwrap();
        assert!(yaml.contains("apiVersion: nyl.niklasrosenstein.github.com/v1"));
        assert!(yaml.contains("kind: HelmChart"));
        assert!(yaml.contains("name: nginx-app"));
        assert!(yaml.contains("replicaCount: 2"));

        // Round-trip
        let deserialized: HelmChart = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(deserialized.metadata.name, "nginx-app");
        assert_eq!(deserialized.spec.chart.name, Some("./charts/nginx".to_string()));
    }

    #[test]
    fn test_helm_chart_spec_defaults() {
        let spec = HelmChartSpec::default();
        assert!(spec.values.is_object());
        assert_eq!(spec.include_crds, None);
    }

    #[test]
    fn test_helm_chart_spec_include_crds_explicit_false() {
        let yaml = r"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
  includeCrds: false
";
        let result: HelmChart = serde_norway::from_str(yaml).unwrap();
        assert_eq!(result.spec.include_crds, Some(false));
    }

    #[test]
    fn test_helm_chart_spec_include_crds_explicit_true() {
        let yaml = r"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
  includeCrds: true
";
        let result: HelmChart = serde_norway::from_str(yaml).unwrap();
        assert_eq!(result.spec.include_crds, Some(true));
    }

    #[test]
    fn test_helm_chart_spec_include_crds_omitted_is_none() {
        let yaml = r"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
";
        let result: HelmChart = serde_norway::from_str(yaml).unwrap();
        assert_eq!(result.spec.include_crds, None);
    }

    #[test]
    fn test_helm_chart_spec_include_crds_not_serialized_when_none() {
        let helm_chart = HelmChart::new(
            "test",
            ChartRef {
                name: Some("mychart".to_string()),
                ..Default::default()
            },
        );
        let yaml = serde_norway::to_string(&helm_chart).unwrap();
        assert!(!yaml.contains("includeCrds"));
    }

    #[test]
    fn test_chart_ref_rejects_unknown_fields() {
        let yaml = r#"
name: mychart
version: "1.0.0"
unknownField: value
"#;
        let result: std::result::Result<ChartRef, _> = serde_norway::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown field"));
    }

    #[test]
    fn test_helm_chart_rejects_unknown_fields() {
        let yaml = r"
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: test
spec:
  chart:
    name: mychart
  unknownField: value
";
        let result: std::result::Result<HelmChart, _> = serde_norway::from_str(yaml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown field"));
    }
}
