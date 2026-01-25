/// HelmChart resource definition
///
/// Represents a declarative Helm chart deployment

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Reference to a Helm chart
///
/// Phase 2: Only `path` is supported for local charts
/// Phase 3: `git` and `repository` for remote charts
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChartRef {
    /// Local filesystem path to the chart
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Git repository URL (Phase 3)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,

    /// Helm repository URL (Phase 3)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,

    /// Chart name (required if using git or repository)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Chart version
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Git reference (branch, tag, commit) - Phase 3
    #[serde(skip_serializing_if = "Option::is_none", rename = "ref")]
    pub git_ref: Option<String>,
}

/// Kubernetes object metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Helm release metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseMetadata {
    /// Release name
    pub name: String,

    /// Release namespace (overrides metadata.namespace)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    /// Create namespace if it doesn't exist
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub create_namespace: bool,
}

impl ReleaseMetadata {
    /// Create release metadata with just a name
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            namespace: None,
            create_namespace: false,
        }
    }
}

/// HelmChart specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelmChartSpec {
    /// Reference to the chart
    pub chart: ChartRef,

    /// Release metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseMetadata>,

    /// Values to pass to the chart
    #[serde(default)]
    pub values: serde_json::Value,
}

impl Default for HelmChartSpec {
    fn default() -> Self {
        Self {
            chart: ChartRef::default(),
            release: None,
            values: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// HelmChart resource
///
/// This is a Kubernetes-style resource that represents a Helm chart deployment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelmChart {
    /// API version (e.g., "nyl.io/v1")
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
            api_version: "nyl.io/v1".to_string(),
            kind: "HelmChart".to_string(),
            metadata: ObjectMetadata::new(name),
            spec: HelmChartSpec {
                chart: chart_ref,
                ..Default::default()
            },
        }
    }

    /// Set the release metadata
    pub fn with_release(mut self, release: ReleaseMetadata) -> Self {
        self.spec.release = Some(release);
        self
    }

    /// Set the values
    pub fn with_values(mut self, values: serde_json::Value) -> Self {
        self.spec.values = values;
        self
    }

    /// Set the namespace
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.metadata.namespace = Some(namespace.into());
        self
    }

    /// Get the effective namespace (from release or metadata)
    pub fn effective_namespace(&self) -> Option<&str> {
        self.spec
            .release
            .as_ref()
            .and_then(|r| r.namespace.as_deref())
            .or(self.metadata.namespace.as_deref())
    }

    /// Get the release name (from release metadata or resource name)
    pub fn effective_release_name(&self) -> &str {
        self.spec
            .release
            .as_ref()
            .map(|r| r.name.as_str())
            .unwrap_or(&self.metadata.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chart_ref_local_path() {
        let chart_ref = ChartRef {
            path: Some("./charts/mychart".to_string()),
            ..Default::default()
        };

        let yaml = serde_norway::to_string(&chart_ref).unwrap();
        assert!(yaml.contains("path: ./charts/mychart"));

        let deserialized: ChartRef = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(deserialized.path, Some("./charts/mychart".to_string()));
    }

    #[test]
    fn test_chart_ref_repository() {
        let chart_ref = ChartRef {
            repository: Some("https://charts.example.com".to_string()),
            name: Some("nginx".to_string()),
            version: Some("1.0.0".to_string()),
            ..Default::default()
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
    fn test_release_metadata() {
        let mut release = ReleaseMetadata::new("my-release");
        release.namespace = Some("prod".to_string());
        release.create_namespace = true;

        assert_eq!(release.name, "my-release");
        assert_eq!(release.namespace, Some("prod".to_string()));
        assert!(release.create_namespace);
    }

    #[test]
    fn test_helm_chart_new() {
        let chart_ref = ChartRef {
            path: Some("./charts/app".to_string()),
            ..Default::default()
        };

        let helm_chart = HelmChart::new("my-app", chart_ref);

        assert_eq!(helm_chart.api_version, "nyl.io/v1");
        assert_eq!(helm_chart.kind, "HelmChart");
        assert_eq!(helm_chart.metadata.name, "my-app");
        assert_eq!(
            helm_chart.spec.chart.path,
            Some("./charts/app".to_string())
        );
    }

    #[test]
    fn test_helm_chart_builder() {
        let chart_ref = ChartRef {
            path: Some("./charts/app".to_string()),
            ..Default::default()
        };

        let values = serde_json::json!({"replicas": 3, "image": "nginx:latest"});

        let helm_chart = HelmChart::new("my-app", chart_ref)
            .with_namespace("production")
            .with_release(ReleaseMetadata::new("my-release"))
            .with_values(values.clone());

        assert_eq!(helm_chart.metadata.namespace, Some("production".to_string()));
        assert_eq!(helm_chart.effective_release_name(), "my-release");
        assert_eq!(helm_chart.spec.values, values);
    }

    #[test]
    fn test_effective_namespace() {
        let chart_ref = ChartRef::default();

        // No namespace set
        let chart1 = HelmChart::new("app", chart_ref.clone());
        assert!(chart1.effective_namespace().is_none());

        // Namespace in metadata
        let chart2 = HelmChart::new("app", chart_ref.clone()).with_namespace("ns1");
        assert_eq!(chart2.effective_namespace(), Some("ns1"));

        // Namespace in release (takes priority)
        let mut release = ReleaseMetadata::new("rel");
        release.namespace = Some("ns2".to_string());
        let chart3 = HelmChart::new("app", chart_ref)
            .with_namespace("ns1")
            .with_release(release);
        assert_eq!(chart3.effective_namespace(), Some("ns2"));
    }

    #[test]
    fn test_effective_release_name() {
        let chart_ref = ChartRef::default();

        // No release metadata - uses resource name
        let chart1 = HelmChart::new("my-app", chart_ref.clone());
        assert_eq!(chart1.effective_release_name(), "my-app");

        // With release metadata
        let chart2 = HelmChart::new("my-app", chart_ref)
            .with_release(ReleaseMetadata::new("custom-release"));
        assert_eq!(chart2.effective_release_name(), "custom-release");
    }

    #[test]
    fn test_helm_chart_serialization() {
        let chart_ref = ChartRef {
            path: Some("./charts/nginx".to_string()),
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
        assert!(yaml.contains("apiVersion: nyl.io/v1"));
        assert!(yaml.contains("kind: HelmChart"));
        assert!(yaml.contains("name: nginx-app"));
        assert!(yaml.contains("replicaCount: 2"));

        // Round-trip
        let deserialized: HelmChart = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(deserialized.metadata.name, "nginx-app");
        assert_eq!(deserialized.spec.chart.path, Some("./charts/nginx".to_string()));
    }

    #[test]
    fn test_helm_chart_spec_defaults() {
        let spec = HelmChartSpec::default();
        assert!(spec.release.is_none());
        assert!(spec.values.is_object());
    }
}
